use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::hash::{DefaultHasher, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::debug;

/// How recently a file may have been modified, relative to the scan that
/// hashed it, for its hash to be reused on an unchanged stat. Timestamps are
/// coarse (a kernel tick, or a second on some filesystems), so a write right
/// after that scan can leave size, mtime and inode as they were; such files
/// are rehashed until they are older than this
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// The regular files under a watched path and their contents: the path
/// itself if it is a file, or the directory's direct entries.
///
/// Live reload decides on these rather than on file system events: an event
/// only says when to look, and [`Snapshot::diff`] says what changed.
#[derive(Debug, Clone)]
pub struct Snapshot {
    root: PathBuf,
    /// When the scan started, for [`RACY_WINDOW`]
    taken_at: SystemTime,
    files: BTreeMap<PathBuf, FileState>,
}

/// One regular file in a [`Snapshot`]
#[derive(Debug, Clone)]
struct FileState {
    stat: Stat,
    modified: SystemTime,
    hash: u64,
}

/// The stat fields that tell whether a file may have changed since it was
/// hashed. Any write updates mtime and ctime; a rename into place changes the
/// inode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    dev: u64,
    ino: u64,
    mode: u32,
}

impl Stat {
    fn of(metadata: &Metadata) -> Self {
        Stat {
            size: metadata.size(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
        }
    }
}

/// A difference between two snapshots
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The file's contents changed (in place or by a rename over it)
    Modified,
    /// A new file with contents, including one renamed into place
    Created,
    /// A new empty file
    CreatedEmpty,
    /// The file is gone
    Deleted,
    /// Same contents, different metadata (timestamps, permissions, inode)
    Metadata,
}

impl Change {
    /// Whether the change restarts the child: everything except new empty
    /// files and metadata-only changes
    pub fn restarts(&self) -> bool {
        !matches!(self.kind, ChangeKind::CreatedEmpty | ChangeKind::Metadata)
    }
}

impl Snapshot {
    /// Snapshots `root`, hashing every file. A missing `root` is an empty
    /// snapshot.
    pub fn take(root: &Path) -> io::Result<Self> {
        Self::scan(root, None)
    }

    /// Snapshots the same path again, rehashing only the files whose stat
    /// changed since this snapshot (or that were modified just before it)
    pub fn rescan(&self) -> io::Result<Self> {
        Self::scan(&self.root, Some(self))
    }

    /// What changed from this snapshot to `newer`, in path order
    pub fn diff(&self, newer: &Snapshot) -> Vec<Change> {
        let mut changes = Vec::new();
        for (path, old) in &self.files {
            let kind = match newer.files.get(path) {
                None => ChangeKind::Deleted,
                Some(new) if new.hash != old.hash => ChangeKind::Modified,
                Some(new) if new.stat != old.stat => ChangeKind::Metadata,
                Some(_) => continue,
            };
            changes.push(Change {
                path: path.clone(),
                kind,
            });
        }
        for (path, new) in &newer.files {
            if !self.files.contains_key(path) {
                let kind = if new.stat.size == 0 {
                    ChangeKind::CreatedEmpty
                } else {
                    ChangeKind::Created
                };
                changes.push(Change {
                    path: path.clone(),
                    kind,
                });
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    fn scan(root: &Path, previous: Option<&Snapshot>) -> io::Result<Self> {
        let taken_at = SystemTime::now();
        let mut files = BTreeMap::new();
        for path in candidates(root)? {
            // Files that vanish or can't be read mid-scan are left out, as if
            // deleted
            match file_state(&path, previous) {
                Ok(Some(state)) => {
                    files.insert(path, state);
                }
                Ok(None) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => debug!("Leaving {:?} out of the snapshot: {}", path, e),
            }
        }
        Ok(Snapshot {
            root: root.to_path_buf(),
            taken_at,
            files,
        })
    }
}

/// The paths a snapshot of `root` looks at: `root` itself, or the entries
/// of the directory it names
fn candidates(root: &Path) -> io::Result<Vec<PathBuf>> {
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    if !metadata.is_dir() {
        return Ok(vec![root.to_path_buf()]);
    }
    match fs::read_dir(root) {
        Ok(entries) => entries.map(|entry| entry.map(|e| e.path())).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The state of `path` if it is a regular file (following symlinks), reusing
/// the previous hash when the file can't have changed
fn file_state(path: &Path, previous: Option<&Snapshot>) -> io::Result<Option<FileState>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let stat = Stat::of(&metadata);
    let modified = metadata.modified()?;

    let unchanged = previous.and_then(|snapshot| {
        let old = snapshot.files.get(path)?;
        let settled = old.modified + RACY_WINDOW < snapshot.taken_at;
        (old.stat == stat && settled).then_some(old.hash)
    });
    let hash = match unchanged {
        Some(hash) => hash,
        None => hash_file(path)?,
    };
    Ok(Some(FileState {
        stat,
        modified,
        hash,
    }))
}

/// A hash of the file's contents. Only compared within one scinit process,
/// so std's (unspecified but fixed) hasher is enough.
fn hash_file(path: &Path) -> io::Result<u64> {
    struct HashWriter(DefaultHasher);
    impl Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut hasher = HashWriter(DefaultHasher::new());
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.0.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn changes(before: &Snapshot) -> (Snapshot, Vec<Change>) {
        let after = before.rescan().unwrap();
        let changes = before.diff(&after);
        (after, changes)
    }

    fn change(path: &Path, kind: ChangeKind) -> Change {
        Change {
            path: path.to_path_buf(),
            kind,
        }
    }

    #[test]
    fn unchanged_directory_has_no_changes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("app.conf"), "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();

        assert_eq!(changes(&before).1, vec![]);
    }

    #[test]
    fn content_change_is_modified() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        // Same size, and possibly the same mtime: the racy window must catch it
        fs::write(&file, "v2").unwrap();

        let (_, changes) = changes(&before);
        assert_eq!(changes, vec![change(&file, ChangeKind::Modified)]);
        assert!(changes[0].restarts());
    }

    #[test]
    fn same_content_rewrite_is_metadata_only() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&file, "v1").unwrap();

        let (_, changes) = changes(&before);
        assert_eq!(changes, vec![change(&file, ChangeKind::Metadata)]);
        assert!(!changes[0].restarts());
    }

    #[test]
    fn chmod_is_metadata_only() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            changes(&before).1,
            vec![change(&file, ChangeKind::Metadata)]
        );
    }

    #[test]
    fn new_files_are_created_or_created_empty() {
        let dir = tempdir().unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        let empty = dir.path().join("empty");
        let full = dir.path().join("full");
        File::create(&empty).unwrap();
        fs::write(&full, "data").unwrap();

        let (_, changes) = changes(&before);
        assert_eq!(
            changes,
            vec![
                change(&empty, ChangeKind::CreatedEmpty),
                change(&full, ChangeKind::Created)
            ]
        );
        assert!(!changes[0].restarts());
        assert!(changes[1].restarts());
    }

    #[test]
    fn empty_file_gaining_content_is_modified() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        let before = Snapshot::take(dir.path()).unwrap();
        File::create(&file).unwrap();
        let (middle, _) = changes(&before);
        fs::write(&file, "v1").unwrap();

        assert_eq!(
            changes(&middle).1,
            vec![change(&file, ChangeKind::Modified)]
        );
    }

    #[test]
    fn deleted_file_is_deleted() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        fs::remove_file(&file).unwrap();

        let (_, changes) = changes(&before);
        assert_eq!(changes, vec![change(&file, ChangeKind::Deleted)]);
        assert!(changes[0].restarts());
    }

    #[test]
    fn rename_over_file_is_modified_and_temp_file_is_not_seen() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        let temp = dir.path().join(".app.conf.tmp");
        fs::write(&temp, "v2").unwrap();
        fs::rename(&temp, &file).unwrap();

        assert_eq!(
            changes(&before).1,
            vec![change(&file, ChangeKind::Modified)]
        );
    }

    #[test]
    fn rename_into_place_is_created() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        let staged = outside.path().join("app");
        fs::write(&staged, "binary").unwrap();
        let file = dir.path().join("app");
        fs::rename(&staged, &file).unwrap();

        assert_eq!(changes(&before).1, vec![change(&file, ChangeKind::Created)]);
    }

    #[test]
    fn subdirectories_and_their_contents_are_ignored() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested.conf"), "v1").unwrap();
        let before = Snapshot::take(dir.path()).unwrap();
        fs::write(sub.join("nested.conf"), "v2").unwrap();
        fs::create_dir(dir.path().join("other")).unwrap();

        assert_eq!(changes(&before).1, vec![]);
    }

    #[test]
    fn single_file_root() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app");
        fs::write(&file, "v1").unwrap();
        let before = Snapshot::take(&file).unwrap();
        // Other files next to it are not watched
        fs::write(dir.path().join("other"), "x").unwrap();
        let (middle, found) = changes(&before);
        assert_eq!(found, vec![]);

        fs::write(&file, "v2").unwrap();
        let (after, found) = changes(&middle);
        assert_eq!(found, vec![change(&file, ChangeKind::Modified)]);

        fs::remove_file(&file).unwrap();
        assert_eq!(changes(&after).1, vec![change(&file, ChangeKind::Deleted)]);
    }

    #[test]
    fn missing_root_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");
        let before = Snapshot::take(&missing).unwrap();
        fs::write(&missing, "v1").unwrap();

        assert_eq!(
            changes(&before).1,
            vec![change(&missing, ChangeKind::Created)]
        );
    }

    #[test]
    fn settled_unchanged_file_is_not_rehashed() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let mut before = Snapshot::take(dir.path()).unwrap();
        // Age the scan past the racy window and tamper with the stored hash:
        // a rescan that reuses it keeps the tampered value
        before.taken_at += RACY_WINDOW * 2;
        before.files.get_mut(&file).unwrap().hash ^= 1;

        let after = before.rescan().unwrap();
        assert_eq!(after.files[&file].hash, before.files[&file].hash);
        assert_eq!(before.diff(&after), vec![]);
    }

    #[test]
    fn recently_modified_file_is_rehashed() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("app.conf");
        fs::write(&file, "v1").unwrap();
        let mut before = Snapshot::take(dir.path()).unwrap();
        before.files.get_mut(&file).unwrap().hash ^= 1;

        let after = before.rescan().unwrap();
        assert_ne!(after.files[&file].hash, before.files[&file].hash);
    }
}
