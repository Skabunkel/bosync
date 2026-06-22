# CLAUDE.md

Guidance for working in this repository. Read this before writing or moving code.

This project is written in **Rust** and uses a combination of **Clean Architecture**
(strict, one-directional dependencies enforced through traits) and **Vertical Slice
Architecture** (code organized by feature/use-case, not by technical concern).

The single most important rule: **dependencies only ever point inward.** When something
inner needs a capability that lives further out, you invert the dependency with a trait —
you never bend the arrow.

---

## The dependency rule

```
Application  <-  Infrastructure  <-  Service
   (core)                              (edge)
```

Read the arrows as *"is depended upon by"*:

- `Service` depends on `Infrastructure` and `Application`.
- `Infrastructure` depends on `Application`.
- `Application` depends on **nothing** in this list. It is the core and knows nothing
  about how data is stored or how the world reaches it.

This is enforced at compile time by the crate graph. A Cargo workspace makes the compiler
refuse violations for you:

```
crates/
  application/      # no dependency on infrastructure or service
  infrastructure/   # depends on application
  service/          # depends on infrastructure and application; the binary lives here
```

If you ever feel the urge to add `application -> infrastructure` to a `Cargo.toml`, stop.
That is the signal to invert with a trait instead (see "Inverting the flow").

---

## Layers

### Application

The core. This is where the real logic lives: calculations, state transitions, mutations
to domain objects, validation, decisions.

When the application needs to touch the outside world (a database, an API, the filesystem),
it does **not** import a concrete implementation. It declares a **trait** describing the
action it needs in general terms — *"store this data for customer X"*, *"load order Y"* —
and depends only on that trait. The concrete implementation is provided by an outer layer.

Application also **owns the core models** — the canonical domain types (`Customer`, `Order`,
`Money`, …). These are the single source of truth that the rest of the system speaks in.
They are shaped around the domain, not around any wire format, table schema, or transport.
Outer layers translate *into* and *out of* these types; the core never reshapes itself to
accommodate an external representation.

Application code must be fully testable on its own, with no Infrastructure or Service
present. If a piece of application logic can't be unit-tested without spinning up a
database or a server, it's in the wrong layer or depending on the wrong thing.

```rust
// crates/application/src/customer/mod.rs
pub struct Customer {
    pub id: CustomerId,
    pub balance: Money,
}

/// Declared in the consumer (here), implemented further out.
pub trait CustomerStore {
    fn load(&self, id: CustomerId) -> Result<Customer, StoreError>;
    fn save(&self, customer: &Customer) -> Result<(), StoreError>;
}

/// Pure logic. Generic over the trait, so it can be driven by a real store
/// or a fake one with zero changes.
pub fn apply_credit<S: CustomerStore>(
    store: &S,
    id: CustomerId,
    amount: Money,
) -> Result<(), AppError> {
    let mut customer = store.load(id)?;
    customer.balance = customer.balance + amount;
    store.save(&customer)?;
    Ok(())
}
```

### Infrastructure

This is where the application's outward-facing traits are **implemented**: database access,
API clients, file storage, caches, and so on. The "how" of reaching the world lives here.

The boundary: Infrastructure implements details **only when they have no connection to the
Service layer.** Anything tied to how the world reaches *us* (a message queue, for example)
is a Service concern and is implemented in Service, not here — even if it looks like "just
another client" on the surface.

```rust
// crates/infrastructure/src/customer/postgres.rs
use application::customer::{Customer, CustomerId, CustomerStore, StoreError};

pub struct PostgresCustomerStore {
    pool: PgPool,
}

impl CustomerStore for PostgresCustomerStore {
    fn load(&self, id: CustomerId) -> Result<Customer, StoreError> { /* ... */ }
    fn save(&self, customer: &Customer) -> Result<(), StoreError> { /* ... */ }
}
```

### Service

This is where the world interacts with us: the HTTP/REST server, gRPC, message-queue
consumers, schedulers, CLI entry points. The binary lives here.

Service is also where **composition** happens — concrete types are chosen and wired into
the generic application logic (see "Composition").

If an inner layer (Application or Infrastructure) needs information or a capability that
lives in Service, the inner layer declares a trait and Service implements it. Same inversion
rule as everywhere else.

Service also owns the **boundary mapping**. External models — request/response DTOs, gRPC
messages, queue payloads — live in Service, and Service is responsible for translating them
into the Application's core models on the way in and back out on the way out. Those external
types never leak inward: Application and Infrastructure only ever see core models. Keep the
mapping in one obvious place per slice (e.g. a `From`/`TryFrom` impl or a small `mapper`
module) so the translation is easy to find and test.

The same applies to **errors**. Application and Infrastructure raise internal error types
that describe *what went wrong* in domain terms (`AppError`, `StoreError`, …). Service maps
those into **world errors** — the representation the outside expects: HTTP status codes,
gRPC `Status`, a problem-details body, a queue nack/retry decision. Internal error types
never cross the boundary; the world only ever sees a world error. Keep this mapping next to
the model mapping for each slice so the whole translation lives in one place.

```rust
// crates/service/src/http/customer.rs
use application::customer::{self, Customer, CustomerId, Money};

/// External model — lives in Service, never crosses inward.
#[derive(serde::Deserialize)]
struct CreditRequestDto {
    customer_id: String,
    amount_cents: i64,
}

/// Map the DTO into core models at the boundary.
impl TryFrom<CreditRequestDto> for CreditCommand {
    type Error = ApiError;
    fn try_from(dto: CreditRequestDto) -> Result<Self, Self::Error> {
        Ok(CreditCommand {
            id: CustomerId::parse(&dto.customer_id)?,
            amount: Money::from_cents(dto.amount_cents),
        })
    }
}

struct CreditCommand {
    id: CustomerId,
    amount: Money,
}

/// Map internal errors into a world error at the boundary.
impl From<application::customer::AppError> for ApiError {
    fn from(err: application::customer::AppError) -> Self {
        use application::customer::AppError::*;
        match err {
            CustomerNotFound(_) => ApiError::not_found("customer not found"),
            InsufficientFunds   => ApiError::conflict("insufficient funds"),
            Store(_)            => ApiError::internal(), // don't leak internal detail
        }
    }
}
```

---

## Inverting the flow

Whenever a layer needs a capability provided by a layer *further out*, you invert the
dependency rather than reversing the arrow:

> Declare the trait in the **consumer** (the layer that needs it).
> Implement it in the **provider** (the layer that has the capability).

This keeps the compile-time dependency pointing inward while letting behavior flow outward
at runtime.

- **Application needs persistence** → trait in `Application`, impl in `Infrastructure`.
- **Application needs to publish to a message queue** → the queue is a Service concern, so
  trait in `Application` (e.g. `EventPublisher`), impl in `Service`.
- **Infrastructure needs something owned by Service** → trait in `Infrastructure`, impl in
  `Service`.

```rust
// crates/application/src/customer/mod.rs
pub trait EventPublisher {
    fn publish(&self, event: &DomainEvent) -> Result<(), PublishError>;
}

// crates/service/src/messaging/rabbit.rs
use application::customer::{DomainEvent, EventPublisher, PublishError};

pub struct RabbitPublisher { /* ... */ }

impl EventPublisher for RabbitPublisher {
    fn publish(&self, event: &DomainEvent) -> Result<(), PublishError> { /* ... */ }
}
```

---

## Composition

We compose applications by combining the relevant Infrastructure and Service pieces, and
we do that **in the Service layer**.

- **Prefer static dispatch.** Use generics and monomorphization. Do **not** reach for
  `dyn` / `Box<dyn Trait>` unless it is genuinely necessary (e.g. heterogeneous collections
  or a real need to choose an implementation at runtime). When `dyn` is used, leave a short
  comment explaining why it was necessary.
- **Wire concrete types in at the edge** using the turbofish operator, passing in the
  relevant types so the compiler resolves everything statically.

```rust
// crates/service/src/http/customer.rs
async fn credit_handler(state: AppState, req: CreditRequest) -> ApiResult {
    let store = &state.customer_store; // concrete PostgresCustomerStore

    application::customer::apply_credit::<PostgresCustomerStore>(
        store,
        req.customer_id,
        req.amount,
    )?;

    Ok(())
}
```

Concrete implementations are constructed once near the entry point (`main`) and passed down.
The application logic stays generic; only Service knows the real types.

---

## Vertical slices

Within each layer, organize by **feature / use-case**, not by technical kind. A slice owns
its types, its logic, and the traits it needs.

```
crates/application/src/
  customer/      # Customer type, CustomerStore trait, apply_credit, ...
  orders/        # Order type, OrderStore trait, place_order, ...
  billing/
```

Infrastructure and Service mirror these slices where it makes sense
(`infrastructure/src/customer/`, `service/src/http/customer.rs`), so a single feature is
easy to trace top-to-bottom. Add a new feature by adding a slice, not by editing a pile of
shared "managers" or "helpers."

Keep slices independent. Shared domain primitives can live in a small `shared`/`common`
module inside the same crate, but resist the temptation to grow a god-module.

---

## Testing

- **Application logic is tested without Infrastructure or Service.** Implement the layer's
  traits with in-memory fakes and drive the logic directly. If a unit test needs a real
  database or server, the design has leaked.
- Infrastructure implementations get their own integration tests against the real backing
  service.
- Service gets end-to-end / contract tests.

```rust
#[derive(Default)]
struct FakeCustomerStore {
    customers: std::cell::RefCell<HashMap<CustomerId, Customer>>,
}

impl CustomerStore for FakeCustomerStore {
    fn load(&self, id: CustomerId) -> Result<Customer, StoreError> { /* ... */ }
    fn save(&self, customer: &Customer) -> Result<(), StoreError> { /* ... */ }
}

#[test]
fn apply_credit_increases_balance() {
    let store = FakeCustomerStore::default();
    // seed, act, assert — no DB, no server
}
```

---

## Rust conventions

### Strings

Prefer `Box<str>` over `String` wherever the value does not need to be mutated or grown.
Most strings in core models and stored data are set once and only read afterwards — an
owned name, an ID, a status, a parsed field. For those, `Box<str>` is the right default:
it's one word smaller than `String` (no spare capacity field) and it states the intent that
the value is immutable.

Use `String` only when you actually need to build up or mutate the value — accumulating in
a loop, pushing/appending, repeated reallocation. Take `&str` for borrowed parameters as
usual; `Box<str>` is for *owned, fixed* string data.

```rust
pub struct Customer {
    pub id: Box<str>,        // set once, never mutated
    pub display_name: Box<str>,
    pub balance: Money,
}

// Build with String, then freeze:
let mut buf = String::new();
buf.push_str(&first);
buf.push(' ');
buf.push_str(&last);
let display_name: Box<str> = buf.into(); // String -> Box<str>
```

Conversions are cheap and explicit: `s.into()` / `String::from(b)` move between the two,
`&b` / `&*b` give you a `&str`.

### Lints

Use **Clippy**. Code is not done until it is clippy-clean. Run it across everything:

```sh
cargo clippy --all-targets --all-features -- -D warnings
```

Treat Clippy warnings as errors (`-D warnings`) — don't let them accumulate. Fix the cause
rather than silencing the lint. If a lint genuinely doesn't apply, suppress it as narrowly
as possible (`#[allow(clippy::lint_name)]` on the specific item, never crate-wide) and add a
one-line comment explaining why.


---

## Checklist before adding code

1. Which **layer** does this belong to? (logic → Application, "how we reach the world" →
   Infrastructure, "how the world reaches us" → Service)
2. Which **slice/feature** does it belong to?
3. Does it need something from a layer further out? If so, declare a **trait in this layer**
   and implement it further out — never add an inward-pointing dependency.
4. Am I using `dyn`? If yes, is it actually necessary? If not, make it generic.
5. Can the Application part of this be unit-tested with a fake? If not, rework it.
6. Is composition happening in Service via generics + turbofish, with concrete types chosen
   at the edge?
7. Am I passing an external model (DTO, gRPC message, queue payload) inward, or returning an
   internal error outward? If so, map it in Service first — external types must not cross
   into Application or Infrastructure, and internal errors must become world errors before
   they reach the edge.
8. Am I using `String` for a value that's set once and only read? Use `Box<str>` instead —
   `String` is only for strings I need to mutate or grow.
9. Is the code clippy-clean? Run `cargo clippy --all-targets --all-features -- -D warnings`
   and fix the cause; only `#[allow(...)]` narrowly, with a reason.
10. Avoid unnecessary copies, and action that may cause realloc.
11. This repo should be rust only, if something has a non rust dependency we should consider swapping it out.
