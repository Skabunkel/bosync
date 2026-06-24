//! Be nice to the remote: batch pushes and pace/jitter fetches.
//!
//! A live mount commits on every save. Naively it would then push on every commit and fetch on a
//! short fixed interval — fine against a local folder, but wasteful and a quick route to a
//! rate-limit ban against a hosted remote (GitHub, …). [`SyncScheduler`] turns the raw stream of
//! "a change happened" signals into a *paced* stream of push and fetch actions:
//!
//! - **Pushes are batched.** The first unpushed change arms a timer (`push_base`). Each further
//!   change while the batch is still pending extends the deadline by a *geometrically shrinking*
//!   amount (`push_base * push_decay^n`). Those extensions form a convergent series, so a steady
//!   stream of edits can only ever delay the push to a hard ceiling of `push_base / (1 -
//!   push_decay)` after the first change — we are always eventually forced to push.
//! - **Fetches yield to pending pushes.** While anything is waiting to be pushed, no fetch is
//!   issued: we finish saying our piece before asking what's new. Once the batch is pushed (or the
//!   mount is simply idle) fetches run on an interval that backs off geometrically while the remote
//!   stays quiet and snaps back to `fetch_base` the moment something changes — each one jittered so
//!   many clients don't stampede the host in lockstep.
//!
//! The scheduler is pure and deterministic — jitter comes from a seeded xorshift PRNG — so the
//! whole policy is unit-testable with a synthetic clock and no real time, network, or randomness.

use std::time::{Duration, Instant};

/// How aggressively a mount is allowed to talk to its remote. See the module docs for the model.
#[derive(Debug, Clone, Copy)]
pub struct SyncPolicy {
    /// Quiet period after the *first* unpushed change before the batch is pushed.
    pub push_base: Duration,
    /// Geometric decay (0..1) of each follow-up change's extension to the push deadline. The
    /// extensions sum to a convergent series, capping the total delay at `push_base / (1 - decay)`.
    pub push_decay: f64,
    /// How long to wait before retrying after a failed push (e.g. an unsupported network remote).
    pub push_retry: Duration,
    /// Base interval between idle fetches (used when nothing is waiting to be pushed).
    pub fetch_base: Duration,
    /// A fetch that finds no remote change multiplies the interval by this (gentle backoff), up to
    /// `fetch_max`; any local change or an actual remote change resets it to `fetch_base`.
    pub fetch_growth: f64,
    /// Ceiling for the idle-fetch interval.
    pub fetch_max: Duration,
    /// Fraction (0..1) of the interval applied as ± jitter, so clients don't fetch in lockstep.
    pub fetch_jitter: f64,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self {
            push_base: Duration::from_secs(8),
            push_decay: 0.6, // converged push ceiling = 8s / 0.4 = 20s
            push_retry: Duration::from_secs(30),
            fetch_base: Duration::from_secs(60),
            fetch_growth: 1.5,
            fetch_max: Duration::from_secs(300),
            fetch_jitter: 0.25,
        }
    }
}

/// What the mount loop should do this turn, decided by [`SyncScheduler::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Push the batched commits now, then report the outcome via
    /// [`note_pushed`](SyncScheduler::note_pushed) / [`note_push_failed`](SyncScheduler::note_push_failed).
    Push,
    /// Fetch from the remote now, then report via [`note_fetched`](SyncScheduler::note_fetched).
    Fetch,
    /// Nothing is due; the caller may sleep up to this long before polling again.
    Idle(Duration),
}

/// Paces a single mount's push/fetch traffic. Drive it by reporting changes and the outcome of each
/// action; ask [`poll`](SyncScheduler::poll) what to do next.
#[derive(Debug)]
pub struct SyncScheduler {
    policy: SyncPolicy,
    rng: u64,
    /// When the current batch of unpushed changes began (anchors the convergence cap).
    batch_start: Option<Instant>,
    /// When the current batch should be pushed; `None` means nothing is waiting to push.
    push_deadline: Option<Instant>,
    /// Number of follow-up changes since `batch_start` (the `n` in `push_decay^n`).
    changes: u32,
    /// Current idle-fetch interval (grows on no-op fetches, resets on activity).
    fetch_interval: Duration,
    /// When the next idle fetch becomes due.
    next_fetch: Instant,
}

impl SyncScheduler {
    /// Create a scheduler. `seed` seeds the jitter PRNG (any value; `0` is replaced internally so
    /// jitter is never degenerate). The first idle fetch is scheduled `fetch_base` (jittered) out.
    pub fn new(policy: SyncPolicy, now: Instant, seed: u64) -> Self {
        let rng = if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed };
        let mut s = Self {
            policy,
            rng,
            batch_start: None,
            push_deadline: None,
            changes: 0,
            fetch_interval: policy.fetch_base,
            next_fetch: now,
        };
        s.next_fetch = now + s.jittered(policy.fetch_base);
        s
    }

    /// Record that a local change was committed and now awaits a push. The first change arms the
    /// batch timer; each later one nudges the deadline out by a geometrically shrinking amount,
    /// capped so a steady edit stream still pushes within the converged ceiling.
    pub fn note_change(&mut self, now: Instant) {
        match self.batch_start {
            None => {
                self.batch_start = Some(now);
                self.changes = 0;
                self.push_deadline = Some(now + self.policy.push_base);
            }
            Some(start) => {
                self.changes = self.changes.saturating_add(1);
                let add = self
                    .policy
                    .push_base
                    .mul_f64(self.policy.push_decay.powi(self.changes as i32));
                let cap = start + self.converged_max();
                let extended = self.push_deadline.unwrap_or(now) + add;
                self.push_deadline = Some(extended.min(cap));
            }
        }
        // Local activity means the remote is probably moving too; once we're done pushing, resume
        // idle fetches promptly rather than from a backed-off interval.
        self.fetch_interval = self.policy.fetch_base;
    }

    /// Decide what to do at `now`. A pending push always wins over a fetch — we never fetch while we
    /// still owe the remote a push.
    pub fn poll(&mut self, now: Instant) -> Tick {
        if let Some(dl) = self.push_deadline {
            return if now >= dl {
                Tick::Push
            } else {
                Tick::Idle(dl - now)
            };
        }
        if now >= self.next_fetch {
            Tick::Fetch
        } else {
            Tick::Idle(self.next_fetch - now)
        }
    }

    /// Report that the batched push succeeded: clear the batch and schedule the next idle fetch.
    pub fn note_pushed(&mut self, now: Instant) {
        self.batch_start = None;
        self.push_deadline = None;
        self.changes = 0;
        self.fetch_interval = self.policy.fetch_base;
        self.next_fetch = now + self.jittered(self.policy.fetch_base);
    }

    /// Report that the batched push failed: keep the batch and retry after `push_retry`.
    pub fn note_push_failed(&mut self, now: Instant) {
        self.push_deadline = Some(now + self.policy.push_retry);
    }

    /// Report a completed fetch. `brought_changes` resets the idle interval to `fetch_base`; a no-op
    /// fetch grows it geometrically toward `fetch_max`. The next fetch is scheduled with jitter.
    pub fn note_fetched(&mut self, now: Instant, brought_changes: bool) {
        self.fetch_interval = if brought_changes {
            self.policy.fetch_base
        } else {
            self.policy
                .fetch_max
                .min(self.fetch_interval.mul_f64(self.policy.fetch_growth))
        };
        self.next_fetch = now + self.jittered(self.fetch_interval);
    }

    /// The hard ceiling on how long a batch can be delayed: `push_base / (1 - push_decay)`.
    fn converged_max(&self) -> Duration {
        self.policy
            .push_base
            .mul_f64(1.0 / (1.0 - self.policy.push_decay))
    }

    /// `base` scaled by a random factor in `[1 - jitter, 1 + jitter)`.
    fn jittered(&mut self, base: Duration) -> Duration {
        let r = self.next_rand();
        let factor = 1.0 + (2.0 * r - 1.0) * self.policy.fetch_jitter;
        base.mul_f64(factor)
    }

    /// xorshift64* — a tiny deterministic PRNG so the core stays dependency-free. Returns `[0, 1)`.
    fn next_rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SyncPolicy {
        SyncPolicy {
            push_base: Duration::from_secs(8),
            push_decay: 0.6,
            push_retry: Duration::from_secs(30),
            fetch_base: Duration::from_secs(60),
            fetch_growth: 1.5,
            fetch_max: Duration::from_secs(300),
            fetch_jitter: 0.25,
        }
    }

    #[test]
    fn first_change_pushes_after_base() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        s.note_change(t0);
        assert!(matches!(s.poll(t0), Tick::Idle(_)));
        assert_eq!(s.poll(t0 + Duration::from_secs(8)), Tick::Push);
    }

    #[test]
    fn bursts_extend_the_deadline_but_converge_to_a_ceiling() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        s.note_change(t0);
        // A long stream of edits, all "at once": the deadline keeps moving but can't pass the cap.
        for _ in 0..100 {
            s.note_change(t0);
        }
        // ceiling = 8 / (1 - 0.6) = 20s.
        assert_eq!(s.poll(t0 + Duration::from_secs(19)), Tick::Idle(Duration::from_secs(1)));
        assert_eq!(s.poll(t0 + Duration::from_secs(20)), Tick::Push);
    }

    #[test]
    fn fetch_is_suppressed_while_a_push_is_pending() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        s.note_change(t0);
        // Well past any fetch interval, but a push is owed → never a fetch.
        assert_eq!(s.poll(t0 + Duration::from_secs(600)), Tick::Push);
    }

    #[test]
    fn after_push_an_idle_fetch_eventually_runs() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        s.note_change(t0);
        let push_at = t0 + Duration::from_secs(8);
        assert_eq!(s.poll(push_at), Tick::Push);
        s.note_pushed(push_at);
        // Within the next fetch window it's idle; well past it, a fetch is due.
        assert!(matches!(s.poll(push_at + Duration::from_secs(1)), Tick::Idle(_)));
        assert_eq!(s.poll(push_at + Duration::from_secs(120)), Tick::Fetch);
    }

    #[test]
    fn idle_fetch_backs_off_and_resets_on_change() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        // Three quiet fetches: interval grows 60 -> 90 -> 135 -> 202.5 (× 1.5 each, < max).
        let mut now = t0;
        for _ in 0..3 {
            now += Duration::from_secs(600); // force a fetch
            assert_eq!(s.poll(now), Tick::Fetch);
            s.note_fetched(now, false);
        }
        // A fetch that brings changes snaps the interval back to base; the next is ~60s out (±25%).
        now += Duration::from_secs(600);
        assert_eq!(s.poll(now), Tick::Fetch);
        s.note_fetched(now, true);
        assert!(matches!(s.poll(now), Tick::Idle(_)));
        assert_eq!(s.poll(now + Duration::from_secs(80)), Tick::Fetch);
    }

    #[test]
    fn failed_push_retries_after_backoff_not_immediately() {
        let t0 = Instant::now();
        let mut s = SyncScheduler::new(policy(), t0, 1);
        s.note_change(t0);
        let at = t0 + Duration::from_secs(8);
        assert_eq!(s.poll(at), Tick::Push);
        s.note_push_failed(at);
        assert!(matches!(s.poll(at + Duration::from_secs(1)), Tick::Idle(_)));
        assert_eq!(s.poll(at + Duration::from_secs(30)), Tick::Push);
    }

    #[test]
    fn fetch_scheduling_stays_within_jitter_bounds() {
        let t0 = Instant::now();
        // Different seeds must all land inside [base*(1-j), base*(1+j)] = [45s, 75s] for base 60s.
        for seed in 1..50u64 {
            let s = SyncScheduler::new(policy(), t0, seed);
            // First fetch is `fetch_base` jittered; poll just before/after the bounds.
            assert!(matches!(s_poll_at(&s, t0 + Duration::from_secs(44)), Tick::Idle(_)));
            assert_eq!(s_poll_at(&s, t0 + Duration::from_secs(76)), Tick::Fetch);
        }
    }

    /// Poll without mutating (jitter PRNG only advances on note_*); lets the bounds test reuse one
    /// scheduler per seed.
    fn s_poll_at(s: &SyncScheduler, now: Instant) -> Tick {
        // Mirror `poll`'s read-only decision.
        if let Some(dl) = s.push_deadline {
            return if now >= dl { Tick::Push } else { Tick::Idle(dl - now) };
        }
        if now >= s.next_fetch {
            Tick::Fetch
        } else {
            Tick::Idle(s.next_fetch - now)
        }
    }
}
