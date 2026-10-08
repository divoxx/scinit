use crate::Result;
use eyre::eyre;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Events that can be emitted by the file watcher
#[derive(Debug)]
pub enum FileChangeEvent {
    /// A file change was detected
    FileChanged(PathBuf),
    /// An error occurred while watching
    WatchError(String),
}

/// Configuration for file watching behavior
#[derive(Debug, Clone)]
pub struct FileWatchConfig {
    /// Paths to watch for content changes (files or directories)
    pub watch_paths: Vec<PathBuf>,
    /// Sentinel file to watch: its creation and any change to it, including
    /// a metadata-only one (`touch`), count. It need not exist yet.
    pub sentinel: Option<PathBuf>,
    /// Debounce time for file changes (prevents excessive restarts)
    pub debounce: Duration,
}

/// What is watched for a watch path: a directory itself, or a single file
/// through its parent directory
#[derive(Debug)]
struct WatchTarget {
    /// The directory passed to the notify watcher
    dir: PathBuf,
    /// For a single file, its name: only events for this entry of `dir` count
    file_name: Option<OsString>,
    /// Whether this is the sentinel file (see [`FileWatcher::is_sentinel_change`])
    sentinel: bool,
}

impl WatchTarget {
    /// The target for `path`. A file is resolved through symlinks, and its
    /// parent directory is watched, so the watch survives the file being
    /// replaced by rename or deleted and recreated (inotify watches an inode,
    /// which a replacement leaves behind). Anything else is watched as is.
    fn new(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(WatchTarget {
                dir: path.to_path_buf(),
                file_name: None,
                sentinel: false,
            });
        }
        let file = path.canonicalize()?;
        match (file.parent(), file.file_name()) {
            (Some(dir), Some(name)) => Ok(WatchTarget {
                dir: dir.to_path_buf(),
                file_name: Some(name.to_os_string()),
                sentinel: false,
            }),
            _ => Ok(WatchTarget {
                dir: file,
                file_name: None,
                sentinel: false,
            }),
        }
    }

    /// The target for the sentinel file `path`, which need not exist: its
    /// parent directory is watched for entries with its name, so its
    /// creation is seen too
    fn sentinel(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| eyre!("invalid sentinel path {:?}", path))?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        Ok(WatchTarget {
            dir: dir.canonicalize()?,
            file_name: Some(name.to_os_string()),
            sentinel: true,
        })
    }

    /// Whether an event for `path` concerns the watched file or directory.
    /// The watch is non-recursive, so every event path is an entry of `dir`
    /// and only its name needs checking.
    fn matches(&self, path: &Path) -> bool {
        self.file_name
            .as_deref()
            .is_none_or(|name| path.file_name() == Some(name))
    }
}

/// Async file watcher that monitors files for changes and emits events
///
/// This watcher uses the `notify` crate for cross-platform file system monitoring
/// and includes debouncing to prevent excessive restarts when files are being
/// written or compiled. A single file is watched through its parent directory
/// (see [`WatchTarget::new`]).
pub struct FileWatcher {
    /// The underlying notify watcher (one watch per directory); dropping it
    /// stops watching
    _watcher: RecommendedWatcher,
    /// Channel receiver for file change events
    event_rx: mpsc::UnboundedReceiver<FileChangeEvent>,
}

impl FileWatcher {
    /// Starts watching `config.watch_paths` and the sentinel
    /// (non-recursively, and a single file through its parent directory),
    /// with a background task that debounces the changes into events
    pub fn start(config: FileWatchConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::channel(100);

        // Create the notify watcher
        let mut watcher = RecommendedWatcher::new(
            move |res: std::result::Result<notify::Event, notify::Error>| {
                if let Err(e) = tx.blocking_send(res) {
                    error!("Failed to send file change event: {}", e);
                }
            },
            notify::Config::default(),
        )?;

        // Start watching the configured paths. Targets in the same directory
        // (e.g. the sentinel and an extra file next to it) share one watch.
        let mut targets = Vec::new();
        for watch_path in &config.watch_paths {
            targets.push(WatchTarget::new(watch_path)?);
        }
        if let Some(sentinel) = &config.sentinel {
            targets.push(WatchTarget::sentinel(sentinel)?);
        }
        let mut watched_dirs = HashSet::new();
        for target in &targets {
            if watched_dirs.insert(&target.dir) {
                watcher.watch(&target.dir, RecursiveMode::NonRecursive)?;
            }
            if let Some(name) = &target.file_name {
                debug!("Watching {:?} through directory {:?}", name, target.dir);
            }
        }
        for watch_path in &config.watch_paths {
            info!("Started watching path: {:?}", watch_path);
        }
        if let Some(sentinel) = &config.sentinel {
            info!("Started watching sentinel: {:?}", sentinel);
        }

        tokio::spawn(debounce_events(rx, event_tx, targets, config.debounce));

        Ok(FileWatcher {
            _watcher: watcher,
            event_rx,
        })
    }

    /// Waits up to `timeout_duration` for the next event
    #[cfg(test)]
    pub async fn wait_for_event(
        &mut self,
        timeout_duration: Duration,
    ) -> Result<Option<FileChangeEvent>> {
        match tokio::time::timeout(timeout_duration, self.event_rx.recv()).await {
            Ok(Some(event)) => Ok(Some(event)),
            Ok(None) => Ok(None), // Channel closed
            Err(_) => Ok(None),   // Timeout
        }
    }

    /// Waits for the next (debounced) file change event.
    ///
    /// Cancel-safe, so it can be a `select!` branch.
    pub async fn next_event(&mut self) -> Option<FileChangeEvent> {
        self.event_rx.recv().await
    }

    /// Whether a file system event should trigger a restart
    fn is_relevant_change(event: &notify::Event, target: &WatchTarget) -> bool {
        use notify::event::ModifyKind;

        if target.sentinel {
            return Self::is_sentinel_change(event, target);
        }

        // Content changes and renames (editors save by renaming over the file)
        // count; metadata-only changes (permissions, timestamps, xattrs, e.g.
        // from creating an empty file) don't
        let content_or_rename = matches!(
            event.kind,
            notify::EventKind::Modify(
                ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any | ModifyKind::Other
            )
        );

        // Only files count, not directories, and for a single-file watch only
        // that file, not the rest of its directory
        content_or_rename
            && event
                .paths
                .iter()
                .any(|path| target.matches(path) && path.is_file())
    }

    /// Whether a file system event for the sentinel should trigger a
    /// restart: its creation and any change, including a metadata-only one
    /// (the builder `touch`es it), but not its removal. The sentinel is the
    /// only watched file whose metadata-only changes count.
    fn is_sentinel_change(event: &notify::Event, target: &WatchTarget) -> bool {
        matches!(
            event.kind,
            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
        ) && event
            .paths
            .iter()
            .any(|path| target.matches(path) && path.is_file())
    }
}

/// Turns raw notify events into debounced [`FileChangeEvent`]s.
///
/// Trailing-edge debounce: every relevant change (re)arms the deadline, and
/// the restart fires once changes have been quiet for `debounce`, so the
/// last change is never dropped.
async fn debounce_events(
    mut rx: mpsc::Receiver<notify::Result<notify::Event>>,
    event_tx: mpsc::UnboundedSender<FileChangeEvent>,
    targets: Vec<WatchTarget>,
    debounce: Duration,
) {
    let mut pending: Option<PathBuf> = None;
    let deadline = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            res = rx.recv() => {
                let Some(res) = res else { break };
                match res {
                    Ok(event) => {
                        debug!("File system event: {:?}", event);
                        if let Some(target) = targets
                            .iter()
                            .find(|target| FileWatcher::is_relevant_change(&event, target))
                        {
                            if pending.is_some() {
                                debug!("Debouncing file change");
                            }
                            pending = Some(changed_path(&event, target));
                            deadline.as_mut().reset(tokio::time::Instant::now() + debounce);
                        }
                    }
                    Err(e) => {
                        error!("File watching error: {}", e);
                        if event_tx.send(FileChangeEvent::WatchError(e.to_string())).is_err() {
                            break;
                        }
                    }
                }
            }
            _ = &mut deadline, if pending.is_some() => {
                let path = pending.take().unwrap();
                if let Err(e) = event_tx.send(FileChangeEvent::FileChanged(path)) {
                    error!("Failed to send file change event: {}", e);
                    break;
                }
            }
        }
    }
}

/// The event's first path that `target` matches (or the target's
/// directory), canonicalized when possible
fn changed_path(event: &notify::Event, target: &WatchTarget) -> PathBuf {
    let path = event
        .paths
        .iter()
        .find(|path| target.matches(path))
        .map_or(target.dir.as_path(), PathBuf::as_path);
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_file_watcher_start() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_paths: vec![temp_dir.path().to_path_buf()],
            sentinel: None,
            debounce: Duration::from_millis(100),
        };

        assert!(FileWatcher::start(config).is_ok());
    }

    #[tokio::test]
    async fn test_file_change_detection() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_paths: vec![temp_dir.path().to_path_buf()],
            sentinel: None,
            debounce: Duration::from_millis(100),
        };

        let mut watcher = FileWatcher::start(config).unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        // Wait for the file change event
        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(event.is_some());

        if let Some(FileChangeEvent::FileChanged(path)) = event {
            // The FileWatcher now emits canonical paths, so compare with canonical test file path
            let canonical_test_file = test_file.canonicalize().unwrap_or(test_file);
            assert_eq!(path, canonical_test_file);
        } else {
            panic!("Expected FileChanged event");
        }
    }

    #[tokio::test]
    async fn test_debouncing() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_paths: vec![temp_dir.path().to_path_buf()],
            sentinel: None,
            debounce: Duration::from_millis(500),
        };

        let mut watcher = FileWatcher::start(config).unwrap();

        let test_file = temp_dir.path().join("test.txt");

        // Write to file multiple times quickly
        for i in 0..5 {
            fs::write(&test_file, format!("content {}", i)).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Should only get one event due to debouncing
        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(event.is_some());

        // Should not get more events immediately
        let event2 = watcher
            .wait_for_event(Duration::from_millis(200))
            .await
            .unwrap();
        assert!(event2.is_none());
    }

    #[test]
    fn test_is_relevant_change() {
        use notify::EventKind;
        use tempfile::tempdir;

        // Create a temporary directory and file for testing
        let temp_dir = tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "test content").unwrap();

        // Test file modification event
        let event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![test_file],
            attrs: notify::event::EventAttributes::default(),
        };

        let target = WatchTarget::new(temp_dir.path()).unwrap();
        assert!(FileWatcher::is_relevant_change(&event, &target));

        // Test directory modification event (should be ignored)
        let test_dir = temp_dir.path().join("test_dir");
        std::fs::create_dir(&test_dir).unwrap();

        let event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![test_dir],
            attrs: notify::event::EventAttributes::default(),
        };

        // This should be false because it's a directory
        assert!(!FileWatcher::is_relevant_change(&event, &target));
    }

    #[test]
    fn test_metadata_and_create_are_not_relevant() {
        use notify::event::{CreateKind, MetadataKind, ModifyKind, RenameMode};
        use notify::EventKind;

        let temp_dir = tempfile::tempdir().unwrap();
        let file = temp_dir.path().join("app.conf");
        std::fs::write(&file, "v1").unwrap();
        let target = WatchTarget::new(temp_dir.path()).unwrap();
        let relevant = |kind| {
            let event = notify::Event {
                kind,
                paths: vec![file.clone()],
                attrs: notify::event::EventAttributes::default(),
            };
            FileWatcher::is_relevant_change(&event, &target)
        };

        // Creating an empty file on macOS: Create + Modify(Metadata(Extended))
        assert!(!relevant(EventKind::Create(CreateKind::File)));
        assert!(!relevant(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended
        ))));
        assert!(!relevant(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::WriteTime
        ))));
        // Editors saving by renaming over the file
        assert!(relevant(EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
    }

    #[test]
    fn test_single_file_is_watched_through_parent() {
        let temp_dir = tempdir().unwrap();
        let dir = temp_dir.path().canonicalize().unwrap();
        let file = dir.join("app");
        fs::write(&file, "v1").unwrap();

        let target = WatchTarget::new(&file).unwrap();
        assert_eq!(target.dir, dir);
        assert_eq!(target.file_name.as_deref(), Some("app".as_ref()));

        // A directory is watched itself
        let target = WatchTarget::new(&dir).unwrap();
        assert_eq!(target.dir, dir);
        assert_eq!(target.file_name, None);

        // A symlink to a file is resolved to the file it points to
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("app-1.2"), "v1").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(real.join("app-1.2"), &link).unwrap();
        let target = WatchTarget::new(&link).unwrap();
        assert_eq!(target.dir, real);
        assert_eq!(target.file_name.as_deref(), Some("app-1.2".as_ref()));
    }

    #[test]
    fn test_single_file_ignores_siblings() {
        use notify::event::{DataChange, ModifyKind, RenameMode};
        use notify::EventKind;

        let temp_dir = tempdir().unwrap();
        let file = temp_dir.path().join("app");
        let sibling = temp_dir.path().join("app.tmp");
        fs::write(&file, "v1").unwrap();
        fs::write(&sibling, "v2").unwrap();
        let target = WatchTarget::new(&file).unwrap();
        let event = |kind, paths| notify::Event {
            kind,
            paths,
            attrs: notify::event::EventAttributes::default(),
        };
        let write = EventKind::Modify(ModifyKind::Data(DataChange::Content));

        assert!(FileWatcher::is_relevant_change(
            &event(write, vec![file.clone()]),
            &target
        ));
        assert!(!FileWatcher::is_relevant_change(
            &event(write, vec![sibling.clone()]),
            &target
        ));
        // A rename of the sibling over the file reports both paths
        let both = event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![sibling.clone(), file.clone()],
        );
        assert!(FileWatcher::is_relevant_change(&both, &target));
        assert_eq!(changed_path(&both, &target), file.canonicalize().unwrap());
    }

    /// Discard events until none arrives for a while: FSEvents (macOS) can
    /// report writes made just before the watcher started
    async fn settle(watcher: &mut FileWatcher) {
        while watcher
            .wait_for_event(Duration::from_millis(500))
            .await
            .unwrap()
            .is_some()
        {}
    }

    /// Replace `file` by writing a sibling and renaming it over `file`
    fn replace_by_rename(file: &Path, content: &str) {
        let tmp = file.with_extension("new");
        fs::write(&tmp, content).unwrap();
        fs::rename(&tmp, file).unwrap();
    }

    #[tokio::test]
    async fn test_single_file_survives_replacement_by_rename() {
        let temp_dir = tempdir().unwrap();
        let file = temp_dir.path().join("app");
        fs::write(&file, "v0").unwrap();
        let mut watcher = FileWatcher::start(FileWatchConfig {
            watch_paths: vec![file.clone()],
            sentinel: None,
            debounce: Duration::from_millis(100),
        })
        .unwrap();
        settle(&mut watcher).await;

        for i in 1..=2 {
            replace_by_rename(&file, &format!("v{}", i));
            let event = watcher
                .wait_for_event(Duration::from_millis(2000))
                .await
                .unwrap();
            match event {
                Some(FileChangeEvent::FileChanged(path)) => {
                    assert_eq!(path, file.canonicalize().unwrap())
                }
                other => panic!("replacement {}: expected FileChanged, got {:?}", i, other),
            }
        }
    }

    #[tokio::test]
    async fn test_single_file_survives_delete_and_recreate() {
        let temp_dir = tempdir().unwrap();
        let file = temp_dir.path().join("app");
        fs::write(&file, "v0").unwrap();
        let mut watcher = FileWatcher::start(FileWatchConfig {
            watch_paths: vec![file.clone()],
            sentinel: None,
            debounce: Duration::from_millis(100),
        })
        .unwrap();
        settle(&mut watcher).await;

        for i in 1..=2 {
            fs::remove_file(&file).unwrap();
            fs::write(&file, format!("v{}", i)).unwrap();
            let event = watcher
                .wait_for_event(Duration::from_millis(2000))
                .await
                .unwrap();
            assert!(
                matches!(event, Some(FileChangeEvent::FileChanged(_))),
                "recreation {}: expected FileChanged, got {:?}",
                i,
                event
            );
        }
    }

    #[tokio::test]
    async fn test_single_file_sibling_change_is_ignored() {
        let temp_dir = tempdir().unwrap();
        let file = temp_dir.path().join("app");
        let sibling = temp_dir.path().join("other");
        fs::write(&file, "v0").unwrap();
        fs::write(&sibling, "v0").unwrap();
        let mut watcher = FileWatcher::start(FileWatchConfig {
            watch_paths: vec![file],
            sentinel: None,
            debounce: Duration::from_millis(100),
        })
        .unwrap();
        settle(&mut watcher).await;

        fs::write(&sibling, "v1").unwrap();
        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(event.is_none(), "expected no event, got {:?}", event);
    }

    #[test]
    fn test_sentinel_is_watched_through_parent_before_it_exists() {
        let temp_dir = tempdir().unwrap();
        let dir = temp_dir.path().canonicalize().unwrap();
        let sentinel = dir.join("app.scinit");

        let target = WatchTarget::sentinel(&sentinel).unwrap();
        assert_eq!(target.dir, dir);
        assert_eq!(target.file_name.as_deref(), Some("app.scinit".as_ref()));
        assert!(target.sentinel);
    }

    #[test]
    fn test_sentinel_creation_and_metadata_changes_are_relevant() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, MetadataKind, ModifyKind, RemoveKind,
        };
        use notify::EventKind;

        let temp_dir = tempdir().unwrap();
        let sentinel = temp_dir.path().join("app.scinit");
        let other = temp_dir.path().join("app");
        fs::write(&sentinel, "").unwrap();
        fs::write(&other, "v1").unwrap();
        let target = WatchTarget::sentinel(&sentinel).unwrap();
        let relevant = |kind, path: &Path| {
            let event = notify::Event {
                kind,
                paths: vec![path.to_path_buf()],
                attrs: notify::event::EventAttributes::default(),
            };
            FileWatcher::is_relevant_change(&event, &target)
        };

        assert!(relevant(EventKind::Create(CreateKind::File), &sentinel));
        // `touch` on an existing sentinel
        assert!(relevant(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            &sentinel
        ));
        assert!(!relevant(
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            &sentinel
        ));
        // Other files in the directory, the executable included, don't count
        assert!(!relevant(EventKind::Create(CreateKind::File), &other));
        // Nor does removing the sentinel
        fs::remove_file(&sentinel).unwrap();
        assert!(!relevant(EventKind::Remove(RemoveKind::File), &sentinel));
    }

    #[tokio::test]
    async fn test_sentinel_creation_and_touch_trigger() {
        let temp_dir = tempdir().unwrap();
        let sentinel = temp_dir.path().join("app.scinit");
        let mut watcher = FileWatcher::start(FileWatchConfig {
            watch_paths: vec![],
            sentinel: Some(sentinel.clone()),
            debounce: Duration::from_millis(100),
        })
        .unwrap();
        settle(&mut watcher).await;

        fs::File::create(&sentinel).unwrap();
        let event = watcher
            .wait_for_event(Duration::from_millis(2000))
            .await
            .unwrap();
        assert!(
            matches!(event, Some(FileChangeEvent::FileChanged(_))),
            "creation: expected FileChanged, got {:?}",
            event
        );
        settle(&mut watcher).await;

        // A metadata-only change, as `touch` makes
        let mtime = std::time::SystemTime::now() + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&sentinel)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let event = watcher
            .wait_for_event(Duration::from_millis(2000))
            .await
            .unwrap();
        assert!(
            matches!(event, Some(FileChangeEvent::FileChanged(_))),
            "touch: expected FileChanged, got {:?}",
            event
        );
    }

    #[tokio::test]
    async fn test_extra_path_and_sentinel_share_a_directory() {
        let temp_dir = tempdir().unwrap();
        let config = temp_dir.path().join("app.conf");
        let exe = temp_dir.path().join("app");
        fs::write(&config, "v0").unwrap();
        fs::write(&exe, "v0").unwrap();
        let mut watcher = FileWatcher::start(FileWatchConfig {
            watch_paths: vec![config.clone()],
            sentinel: Some(temp_dir.path().join("app.scinit")),
            debounce: Duration::from_millis(100),
        })
        .unwrap();
        settle(&mut watcher).await;

        // The executable next to the sentinel isn't watched
        fs::write(&exe, "v1").unwrap();
        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(event.is_none(), "expected no event, got {:?}", event);

        fs::write(&config, "v1").unwrap();
        let event = watcher
            .wait_for_event(Duration::from_millis(2000))
            .await
            .unwrap();
        match event {
            Some(FileChangeEvent::FileChanged(path)) => {
                assert_eq!(path, config.canonicalize().unwrap())
            }
            other => panic!("expected FileChanged, got {:?}", other),
        }
    }
}
