//! Finding the executable a command name refers to.

use crate::Result;
use eyre::eyre;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Resolves `command` against `PATH` like `execvp` would
pub fn resolve_program(command: &str) -> Result<PathBuf> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(OsStr::new(command)))
        .find(|candidate| {
            candidate
                .metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .ok_or_else(|| eyre!("No such file or directory: {} not found in PATH", command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_program() {
        assert_eq!(resolve_program("/bin/sh").unwrap(), PathBuf::from("/bin/sh"));
        assert!(resolve_program("sh").unwrap().ends_with("sh"));
        assert!(resolve_program("scinit-definitely-not-a-command").is_err());
    }
}
