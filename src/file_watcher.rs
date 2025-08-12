use super::Result;
use crate::process_manager::ProcessManager;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Events that can be emitted by the file watcher
#[derive(Debug, Clone)]
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
    pub debounce_ms: u64,
    /// Whether to watch recursively
    pub recursive: bool,
}

impl Default for FileWatchConfig {
    fn default() -> Self {
        Self {
            watch_path: PathBuf::from("."),
            debounce_ms: 500,
            recursive: false,
        }
    }
}

/// Async file watcher that monitors files for changes and emits events
/// 
/// This watcher uses the `notify` crate for cross-platform file system monitoring
/// and includes debouncing to prevent excessive restarts when files are being
/// written or compiled.
pub struct FileWatcher {
    /// The underlying notify watcher
    watcher: Option<RecommendedWatcher>,
    /// Configuration for the watcher
    config: FileWatchConfig,
    /// Channel sender for file change events
    event_tx: mpsc::UnboundedSender<FileChangeEvent>,
    /// Channel receiver for file change events
    event_rx: mpsc::UnboundedReceiver<FileChangeEvent>,
}

impl FileWatcher {
    /// Creates a new file watcher with the given configuration
    /// 
    /// # Arguments
    /// * `config` - Configuration for the file watcher
    /// 
    /// # Returns
    /// * `Result<Self>` - The file watcher instance or an error
    pub fn new(config: FileWatchConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        
        Ok(FileWatcher {
            watcher: None,
            config,
            event_tx,
            event_rx,
        })
    }

    /// Starts watching the configured path for file changes
    /// 
    /// This method spawns a background task that monitors the file system
    /// and emits events when changes are detected.
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub async fn start_watching(&mut self) -> Result<()> {
        let (tx, mut rx) = mpsc::channel(100);
        
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
        let watch_path = self.config.watch_path.clone();
        let recursive_mode = if self.config.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };

        watcher.watch(&watch_path, recursive_mode)?;
        info!("Started watching path: {:?}", watch_path);

        // Store the watcher
        self.watcher = Some(watcher);

        // Spawn the event processing task
        let event_tx = self.event_tx.clone();
        let debounce_ms = self.config.debounce_ms;
        
        tokio::spawn(async move {
            let mut last_change = None;
            
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(event) => {
                        debug!("File system event: {:?}", event);
                        
                        // Check if this is a file modification event
                        if Self::is_relevant_change(&event) {
                            let now = std::time::Instant::now();
                            
                            // Debounce the change
                            if let Some(last) = last_change {
                                if now.duration_since(last).as_millis() < debounce_ms as u128 {
                                    debug!("Debouncing file change");
                                    continue;
                                }
                            }
                            
                            last_change = Some(now);
                            
                            // Emit the change event
                            if let Err(e) = event_tx.send(FileChangeEvent::FileChanged(
                                event.paths.first().cloned().unwrap_or_else(|| watch_path.clone())
                            )) {
                                error!("Failed to send file change event: {}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("File watching error: {}", e);
                        if let Err(e) = event_tx.send(FileChangeEvent::WatchError(e.to_string())) {
                            error!("Failed to send watch error event: {}", e);
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }


    /// Waits for the next file change event
    /// 
    /// This method waits for the next file change event with an optional timeout.
    /// 
    /// # Arguments
    /// * `timeout_duration` - Maximum time to wait for an event
    /// 
    /// # Returns
    /// * `Result<Option<FileChangeEvent>>` - The event or None if timeout
    pub async fn wait_for_event(&mut self, timeout_duration: Duration) -> Result<Option<FileChangeEvent>> {
        match timeout(timeout_duration, self.event_rx.recv()).await {
            Ok(Some(event)) => Ok(Some(event)),
            Ok(None) => Ok(None), // Channel closed
            Err(_) => Ok(None),   // Timeout
        }
    }

    /// Checks if a file system event is relevant for triggering a restart
    /// 
    /// # Arguments
    /// * `event` - The file system event to check
    /// 
    /// # Returns
    /// * `bool` - True if the event should trigger a restart
    fn is_relevant_change(event: &notify::Event) -> bool {
        // Check if this is a file modification event
        if !event.kind.is_modify() {
            return false;
        }

        // Check if any of the changed paths are files (not directories)
        event.paths.iter().any(|path| {
            if let Ok(metadata) = std::fs::metadata(path) {
                metadata.is_file()
            } else {
                false
            }
        })
    }

}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        // Ensure we stop watching when dropped
        if self.watcher.is_some() {
            // Don't try to use block_on in a Drop implementation
            // Just drop the watcher directly
            self.watcher.take();
        }
    }
}

/// Handles file change events and triggers process restarts
pub async fn handle_file_events(file_watcher: &mut Option<FileWatcher>, process_manager: &mut ProcessManager) -> Result<bool> {
    if let Some(ref mut file_watcher) = file_watcher {
        if let Some(event) = file_watcher.wait_for_event(Duration::from_millis(100)).await? {
            match event {
                FileChangeEvent::FileChanged(path) => {
                    info!("File changed: {:?}, triggering restart", path);
                    let restart_result = process_manager
                        .restart_process_with_reason("file_change")
                        .await?;
                    if !restart_result {
                        info!("Process restart limit exceeded, exiting");
                        return Ok(true); // Signal to exit
                    }
                }
                FileChangeEvent::WatchError(error) => {
                    warn!("File watching error: {}", error);
                }
            }
        }
    }
    Ok(false) // Continue normal operation
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_file_watcher_creation() {
        let config = FileWatchConfig::default();
        let watcher = FileWatcher::new(config);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn test_file_watcher_start() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
            debounce_ms: 100,
            recursive: false,
        };

        let mut watcher = FileWatcher::new(config).unwrap();
        
        // Start watching
        assert!(watcher.start_watching().await.is_ok());
        
        // Watcher will be dropped automatically
    }

    #[tokio::test]
    async fn test_file_change_detection() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
            debounce_ms: 100,
            recursive: false,
        };

        let mut watcher = FileWatcher::new(config).unwrap();
        watcher.start_watching().await.unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        // Wait for the file change event
        let event = watcher.wait_for_event(Duration::from_millis(1000)).await.unwrap();
        assert!(event.is_some());

        if let Some(FileChangeEvent::FileChanged(path)) = event {
            assert_eq!(path, test_file);
        } else {
            panic!("Expected FileChanged event");
        }

        // Watcher will be dropped automatically
    }

    #[tokio::test]
    async fn test_debouncing() {
        let temp_dir = tempdir().unwrap();
        let config = FileWatchConfig {
            watch_path: temp_dir.path().to_path_buf(),
            debounce_ms: 500,
            recursive: false,
        };

        let mut watcher = FileWatcher::new(config).unwrap();
        watcher.start_watching().await.unwrap();

        let test_file = temp_dir.path().join("test.txt");
        
        // Write to file multiple times quickly
        for i in 0..5 {
            fs::write(&test_file, format!("content {}", i)).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Should only get one event due to debouncing
        let event = watcher.wait_for_event(Duration::from_millis(1000)).await.unwrap();
        assert!(event.is_some());

        // Should not get more events immediately
        let event2 = watcher.wait_for_event(Duration::from_millis(200)).await.unwrap();
        assert!(event2.is_none());

        // Watcher will be dropped automatically
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
            kind: EventKind::Modify(notify::event::ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![test_file],
            attrs: notify::event::EventAttributes::default(),
        };

        assert!(FileWatcher::is_relevant_change(&event));

        // Test directory modification event (should be ignored)
        let test_dir = temp_dir.path().join("test_dir");
        std::fs::create_dir(&test_dir).unwrap();
        
        let event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![test_dir],
            attrs: notify::event::EventAttributes::default(),
        };

        // This should be false because it's a directory
        assert!(!FileWatcher::is_relevant_change(&event));
    }
} 