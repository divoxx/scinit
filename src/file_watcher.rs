use crate::snapshot::{Change, Snapshot};
use crate::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
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
/// written or compiled. Events only say when to look: once they go quiet, the
/// watched path is compared against a [`Snapshot`] of its contents, which
/// decides whether anything changed.
pub struct FileWatcher {
    /// The underlying notify watcher; dropping it stops watching
    _watcher: RecommendedWatcher,
    /// Channel receiver for file change events
    event_rx: mpsc::UnboundedReceiver<FileChangeEvent>,
}

impl FileWatcher {
    /// Starts watching `config.watch_path` (non-recursively) and takes its
    /// first snapshot, with a background task that debounces the events and
    /// diffs the snapshots into change events
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

        // After the watch starts, so a change in between is in the snapshot
        // or delivered as an event, and before the child spawns, so events
        // for earlier writes (FSEvents replays some) find nothing changed
        let snapshot = Snapshot::take(&watch_path)?;

        tokio::spawn(debounce_events(rx, event_tx, snapshot, config.debounce));

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
}

/// Whether a file system event is only a read (an open, or a close without
/// writing). Reads can't change the snapshot, and hashing files produces
/// them on Linux, so they don't arm the debounce.
fn is_read(event: &notify::Event) -> bool {
    use notify::event::{AccessKind, AccessMode};

    matches!(event.kind, notify::EventKind::Access(kind)
        if kind != AccessKind::Close(AccessMode::Write))
}

/// Turns raw notify events into debounced [`FileChangeEvent`]s.
///
/// Trailing-edge debounce: every event (re)arms the deadline, and once
/// events have been quiet for `debounce` the watched path is rescanned and
/// diffed against `snapshot`. A restarting change (see [`Change::restarts`])
/// emits [`FileChangeEvent::FileChanged`] with its path.
async fn debounce_events(
    mut rx: mpsc::Receiver<notify::Result<notify::Event>>,
    event_tx: mpsc::UnboundedSender<FileChangeEvent>,
    mut snapshot: Snapshot,
    debounce: Duration,
) {
    let mut pending = false;
    let deadline = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            res = rx.recv() => {
                let Some(res) = res else { break };
                match res {
                    Ok(event) => {
                        debug!("File system event: {:?}", event);
                        if !is_read(&event) {
                            if pending {
                                debug!("Debouncing file change");
                            }
                            pending = true;
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
            _ = &mut deadline, if pending => {
                pending = false;
                let (next, changes) = rescan(snapshot).await;
                snapshot = next;
                let event = match changes {
                    Ok(changes) => {
                        for change in &changes {
                            debug!("{:?} since the last snapshot: {:?}", change.kind, change.path);
                        }
                        match changes.into_iter().find(Change::restarts) {
                            Some(change) => FileChangeEvent::FileChanged(change.path),
                            None => {
                                debug!("Nothing changed that restarts the child");
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to snapshot the watched path: {}", e);
                        FileChangeEvent::WatchError(e.to_string())
                    }
                };
                if let Err(e) = event_tx.send(event) {
                    error!("Failed to send file change event: {}", e);
                    break;
                }
            }
        }
    }
}

/// Rescans `previous` off the async runtime (hashing can take a while) and
/// diffs it against the result. Returns the snapshot to diff against next
/// time: the new one, or `previous` if the scan failed.
async fn rescan(previous: Snapshot) -> (Snapshot, std::io::Result<Vec<Change>>) {
    tokio::task::spawn_blocking(move || match previous.rescan() {
        Ok(next) => {
            let changes = previous.diff(&next);
            (next, Ok(changes))
        }
        Err(e) => (previous, Err(e)),
    })
    .await
    .expect("snapshot task panicked")
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
            assert_eq!(path, test_file);
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

    #[tokio::test]
    async fn writes_before_start_and_metadata_changes_emit_nothing() {
        let temp_dir = tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        // Written right before the watch starts: FSEvents may still deliver it
        fs::write(&test_file, "test content").unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
            debounce: Duration::from_millis(100),
        };
        let mut watcher = FileWatcher::start(config).unwrap();

        fs::File::create(temp_dir.path().join("empty.txt")).unwrap();
        fs::File::options()
            .write(true)
            .open(&test_file)
            .unwrap()
            .set_modified(std::time::SystemTime::now())
            .unwrap();

        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(event.is_none(), "unexpected event: {:?}", event);
    }

    #[tokio::test]
    async fn deleting_a_file_is_a_change() {
        let temp_dir = tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test content").unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
            debounce: Duration::from_millis(100),
        };
        let mut watcher = FileWatcher::start(config).unwrap();

        fs::remove_file(&test_file).unwrap();

        let event = watcher
            .wait_for_event(Duration::from_millis(1000))
            .await
            .unwrap();
        assert!(
            matches!(&event, Some(FileChangeEvent::FileChanged(path)) if *path == test_file),
            "expected a change for {:?}, got {:?}",
            test_file,
            event
        );
    }
}
