//! Handing the controlling terminal to the child.

use crate::Result;
use nix::unistd::{tcsetpgrp, Pid};
use std::fs::File;
use std::io::IsTerminal;
use tracing::{debug, error};

/// Sets the process group as the foreground process group if a terminal is available
pub fn make_foreground(pgid: Pid) -> Result<()> {
    let tty = match File::open("/dev/tty") {
        Ok(tty) => tty,
        Err(e) => {
            debug!(
                "Cannot open /dev/tty ({}), skipping foreground process group setup",
                e
            );
            return Ok(());
        }
    };
    if !tty.is_terminal() {
        debug!("Not a terminal, skipping foreground process group setup");
        return Ok(());
    }
    debug!("Setting process group {} as foreground", &pgid);
    if let Err(e) = tcsetpgrp(tty, pgid) {
        error!("Failed to set process group {} as foreground: {}", &pgid, e);
        return Err(e.into());
    }
    Ok(())
}
