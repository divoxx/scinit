//! Finding the executable a command name refers to.

use nix::unistd::{access, AccessFlags};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Exit code for a command that doesn't exist, as shells use it
pub const NOT_FOUND_EXIT_CODE: i32 = 127;

/// Exit code for a command that exists but can't be executed, as shells use it
pub const NOT_EXECUTABLE_EXIT_CODE: i32 = 126;

/// A command that can't be run, found out by [`resolve_program`] or by the
/// child's `execve`
#[derive(Debug)]
pub struct ProgramError {
    command: String,
    kind: ProgramErrorKind,
}

#[derive(Debug)]
enum ProgramErrorKind {
    NotFound,
    /// Exists, but `execve` refuses it, with the OS error saying why
    NotExecutable(io::Error),
}

impl ProgramError {
    fn not_found(command: &str) -> Self {
        Self {
            command: command.to_string(),
            kind: ProgramErrorKind::NotFound,
        }
    }

    fn not_executable(command: &str, errno: i32) -> Self {
        Self {
            command: command.to_string(),
            kind: ProgramErrorKind::NotExecutable(io::Error::from_raw_os_error(errno)),
        }
    }

    /// The error `execve` of `command` failed with, if it means the command
    /// was not found or is not executable. `ENOEXEC` is a file the kernel
    /// can't run, such as a script without a `#!` line.
    pub fn from_exec_error(command: &str, error: &io::Error) -> Option<Self> {
        match error.raw_os_error()? {
            libc::ENOENT => Some(Self::not_found(command)),
            errno @ (libc::EACCES | libc::ENOEXEC) => Some(Self::not_executable(command, errno)),
            _ => None,
        }
    }

    /// The exit code scinit exits with: 127 when not found, 126 when not
    /// executable
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            ProgramErrorKind::NotFound => NOT_FOUND_EXIT_CODE,
            ProgramErrorKind::NotExecutable(_) => NOT_EXECUTABLE_EXIT_CODE,
        }
    }
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ProgramErrorKind::NotFound => {
                write!(f, "Failed to spawn process '{}': not found", self.command)
            }
            ProgramErrorKind::NotExecutable(e) => write!(
                f,
                "Failed to spawn process '{}': not executable: {}",
                self.command, e
            ),
        }
    }
}

impl std::error::Error for ProgramError {}

/// Resolves `command` to the file to `execve`, like `execvp` would: a
/// command containing `/` is that path, otherwise the first executable file
/// of that name in a `PATH` directory. When `PATH` only has files of that
/// name that aren't executable, it is not executable.
pub fn resolve_program(command: &str) -> Result<PathBuf, ProgramError> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return match check_executable(&path) {
            Some(Ok(())) => Ok(path),
            Some(Err(errno)) => Err(ProgramError::not_executable(command, errno)),
            None => Err(ProgramError::not_found(command)),
        };
    }
    search_path(command, &std::env::var_os("PATH").unwrap_or_default())
}

/// Looks `command` up in the directories of `path`, a `PATH` value
fn search_path(command: &str, path: &OsStr) -> Result<PathBuf, ProgramError> {
    let mut not_executable = None;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(OsStr::new(command));
        match check_executable(&candidate) {
            Some(Ok(())) => return Ok(candidate),
            // Directories in PATH don't count, as in shells
            Some(Err(libc::EISDIR)) | None => {}
            Some(Err(errno)) => not_executable = not_executable.or(Some(errno)),
        }
    }
    Err(match not_executable {
        Some(errno) => ProgramError::not_executable(command, errno),
        None => ProgramError::not_found(command),
    })
}

/// `None` if `path` doesn't exist; otherwise whether it is a file we may
/// execute, or the errno saying why not
fn check_executable(path: &Path) -> Option<Result<(), i32>> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        // A directory on the way we may not search
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Some(Err(libc::EACCES)),
        Err(_) => return None,
    };
    if metadata.is_dir() {
        return Some(Err(libc::EISDIR));
    }
    if !metadata.is_file() || access(path, AccessFlags::X_OK).is_err() {
        return Some(Err(libc::EACCES));
    }
    Some(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A file in `dir` with mode `mode`
    fn file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn exit_code(result: Result<PathBuf, ProgramError>) -> i32 {
        result.unwrap_err().exit_code()
    }

    #[test]
    fn test_resolve_path() {
        assert_eq!(
            resolve_program("/bin/sh").unwrap(),
            PathBuf::from("/bin/sh")
        );
        let dir = tempfile::tempdir().unwrap();
        let script = file(dir.path(), "script", 0o755);
        assert_eq!(resolve_program(script.to_str().unwrap()).unwrap(), script);
    }

    #[test]
    fn test_resolve_from_path() {
        assert!(resolve_program("sh").unwrap().ends_with("sh"));
    }

    #[test]
    fn test_not_found() {
        let err = resolve_program("scinit-definitely-not-a-command").unwrap_err();
        assert_eq!(err.exit_code(), NOT_FOUND_EXIT_CODE);
        assert_eq!(
            err.to_string(),
            "Failed to spawn process 'scinit-definitely-not-a-command': not found"
        );
        assert_eq!(
            exit_code(resolve_program("/nonexistent/scinit-no-such-binary")),
            NOT_FOUND_EXIT_CODE
        );
    }

    #[test]
    fn test_not_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "plain", 0o644);
        let err = resolve_program(path.to_str().unwrap()).unwrap_err();
        assert_eq!(err.exit_code(), NOT_EXECUTABLE_EXIT_CODE);
        assert!(
            err.to_string()
                .contains("not executable: Permission denied"),
            "{}",
            err
        );
    }

    #[test]
    fn test_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_program(dir.path().to_str().unwrap()).unwrap_err();
        assert_eq!(err.exit_code(), NOT_EXECUTABLE_EXIT_CODE);
        assert!(err.to_string().contains("Is a directory"), "{}", err);
    }

    #[test]
    fn test_path_search() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let path = std::env::join_paths(dirs.iter().map(|d| d.path())).unwrap();
        std::fs::create_dir(dirs[0].path().join("tool")).unwrap();
        file(dirs[1].path(), "tool", 0o644);
        let tool = file(dirs[2].path(), "tool", 0o755);
        file(dirs[1].path(), "plain", 0o644);
        std::fs::create_dir(dirs[0].path().join("dir")).unwrap();

        // The first executable file wins, past a directory and a plain file
        assert_eq!(search_path("tool", &path).unwrap(), tool);
        assert_eq!(
            exit_code(search_path("plain", &path)),
            NOT_EXECUTABLE_EXIT_CODE
        );
        assert_eq!(exit_code(search_path("dir", &path)), NOT_FOUND_EXIT_CODE);
        assert_eq!(
            exit_code(search_path("missing", &path)),
            NOT_FOUND_EXIT_CODE
        );
    }

    #[test]
    fn test_exec_errors() {
        let code = |errno| {
            ProgramError::from_exec_error("x", &io::Error::from_raw_os_error(errno))
                .map(|e| e.exit_code())
        };
        assert_eq!(code(libc::ENOENT), Some(NOT_FOUND_EXIT_CODE));
        assert_eq!(code(libc::EACCES), Some(NOT_EXECUTABLE_EXIT_CODE));
        assert_eq!(code(libc::ENOEXEC), Some(NOT_EXECUTABLE_EXIT_CODE));
        assert_eq!(code(libc::ENOTTY), None);
    }
}
