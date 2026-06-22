# bosync git as a file system.

We should have a crate for windows, linux and mac where we have specific syncing logic.

## git over ssh

1. Do a shallow copy of the repo to a user specific tmp folder, this is our ondisk proxy. 
```
let url = gix::url::parse("https://github.com/owner/repo.git".into())?;

let mut prepare = gix::prepare_clone(url, "./repo")?
    .with_shallow(Shallow::DepthAtRemote(NonZeroU32::new(1).unwrap()));
```
2. Mount this folder as drive/cloud sync (cloude file in windows) and a drive? in linux and mac
3. Mark them as cloud only.
4. When a file is open we sync redo the shallow copy of the repo, and then let them open the file.
5. When the file is saved, sync the changes and commit and push.
6. Go into idle state.

### Ideling

1. Every so refetch the repo again to keep us up to date.
2. We want to keep the gitsize down in this repo, so every so often we may prune the history.
3. If a file has not been accessed in a while(say a few minutes) we mark them as cloud only/remote.
4. When something lists the contents of our folder/subfolders again we trigger a new shallow copy, and serve them the updated proxy folder.
