type Result<T> = color_eyre::eyre::Result<T>;

mod signals;

use eyre::eyre;
use nix::sys::signal::{self, Signal};
use nix::unistd::{getpgid, tcsetpgrp, Pid};
use once_cell::sync::Lazy;
use std::fs::File;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::{pin, select};
use tracing::{debug, info, instrument};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static TTY: Lazy<File> = Lazy::new(|| File::open("/dev/tty").unwrap());

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    info!("started");

    let mut args = std::env::args().skip(1);

    let bin_name = args.next().expect("no cmd name provided");
    let bin_args = args.collect::<Vec<String>>();

    let mut sigs = signals::Signals::new()?;

    let subprocess = Subprocess::spawn(bin_name, bin_args)?;
    process_group_to_foreground(subprocess.process_group_id()?)?;

    loop {
        select! {
            Some(sig) = sigs.next() => {
                dbg!("received signal {}", sig);
                if sig == signals::Signal::SIGCHLD {
                    info!("child exited; exiting scinit");
                    return Ok(())
                }

                info!("forwarding {} to subprocess", sig);
                signal::kill(subprocess.pid, Signal::SIGINT).unwrap();
            },
        }
    }
}

fn process_group_to_foreground(pgid: Pid) -> Result<()> {
    if TTY.is_terminal() {
        debug!("setting process group {} as foreground", &pgid);
        tcsetpgrp(TTY.as_raw_fd(), pgid)?;
    }

    Ok(())
}

/// Subprocess represents a running processs.
///
/// It providers a wrapper around tokio::process::Child with some additional behavior as it
/// pertains to this application.
#[derive(Debug)]
struct Subprocess {
    /// Store the pid of the child process, for easy of access.
    pid: Pid,
}

impl Subprocess {
    #[instrument]
    fn spawn(bin: String, args: Vec<String>) -> Result<Self> {
        let child = Command::new(&bin)
            .args(&args)
            // create a new process group for this child process
            .process_group(0)
            // ensure scinit kills the process if the Child instance is dropped
            .kill_on_drop(true)
            // set up stdin/stderr/stdout to inherit from init
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            // start the child process and return a handle
            .spawn()?;

        // Grab the PID, and return an error if it's None.
        // Since this is done immediately after the child is created, it should always be present.
        let pid = match child.id() {
            Some(pid) => Pid::from_raw(pid.try_into()?),
            None => return Err(eyre!("failed to get pid")),
        };

        debug!("spawned subprocess with pid {}", &pid);

        Ok(Subprocess { pid })
    }

    #[instrument]
    fn process_group_id(&self) -> Result<Pid> {
        Ok(getpgid(Some(self.pid))?)
    }
}
