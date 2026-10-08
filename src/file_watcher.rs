use crate::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
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
    /// Path to watch for changes
    pub watch_path: PathBuf,
    /// Debounce time for file changes (prevents excessive restarts)
    pub debounce: Duration,
}

/// Async file watcher that monitors files for changes and emits events
///
/// This watcher uses the `notify` crate for cross-platform file system monitoring
/// and includes debouncing to prevent excessive restarts when files are being
/// written or compiled.
pub struct FileWatcher {
    /// The underlying notify watcher; dropping it stops watching
    _watcher: RecommendedWatcher,
    /// Channel receiver for file change events
    event_rx: mpsc::UnboundedReceiver<FileChangeEvent>,
}

impl FileWatcher {
    /// Starts watching `config.watch_path` (non-recursively), with a
    /// background task that debounces the changes into events
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

        // Start watching the configured path
        let watch_path = config.watch_path;
        watcher.watch(&watch_path, RecursiveMode::NonRecursive)?;
        info!("Started watching path: {:?}", watch_path);

        tokio::spawn(debounce_events(rx, event_tx, watch_path, config.debounce));

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
    fn is_relevant_change(event: &notify::Event) -> bool {
        use notify::event::ModifyKind;

        // Content changes and renames (editors save by renaming over the file)
        // count; metadata-only changes (permissions, timestamps, xattrs, e.g.
        // from creating an empty file) don't
        let content_or_rename = matches!(
            event.kind,
            notify::EventKind::Modify(
                ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any | ModifyKind::Other
            )
        );

        // Only files count, not directories
        content_or_rename && event.paths.iter().any(|path| path.is_file())
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
    watch_path: PathBuf,
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
                        if FileWatcher::is_relevant_change(&event) {
                            if pending.is_some() {
                                debug!("Debouncing file change");
                            }
                            pending = Some(changed_path(&event, &watch_path));
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

/// The event's first path (or `fallback`), canonicalized when possible
fn changed_path(event: &notify::Event, fallback: &Path) -> PathBuf {
    let path = event.paths.first().map_or(fallback, PathBuf::as_path);
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
            watch_path: temp_dir.path().to_path_buf(),
            debounce: Duration::from_millis(100),
        };

        assert!(FileWatcher::start(config).is_ok());
    }

    #[tokio::test]
    async fn test_file_change_detection() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
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
            watch_path: temp_dir.path().to_path_buf(),
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

        assert!(FileWatcher::is_relevant_change(&event));

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
        assert!(!FileWatcher::is_relevant_change(&event));
    }

    #[test]
    fn test_metadata_and_create_are_not_relevant() {
        use notify::event::{CreateKind, MetadataKind, ModifyKind, RenameMode};
        use notify::EventKind;

        let temp_dir = tempfile::tempdir().unwrap();
        let file = temp_dir.path().join("app.conf");
        std::fs::write(&file, "v1").unwrap();
        let event = |kind| notify::Event {
            kind,
            paths: vec![file.clone()],
            attrs: notify::event::EventAttributes::default(),
        };

        // Creating an empty file on macOS: Create + Modify(Metadata(Extended))
        assert!(!FileWatcher::is_relevant_change(&event(EventKind::Create(
            CreateKind::File
        ))));
        assert!(!FileWatcher::is_relevant_change(&event(EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Extended)
        ))));
        assert!(!FileWatcher::is_relevant_change(&event(EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        ))));
        // Editors saving by renaming over the file
        assert!(FileWatcher::is_relevant_change(&event(EventKind::Modify(
            ModifyKind::Name(RenameMode::To)
        ))));
    }
}
