type Result<T> = color_eyre::eyre::Result<T>;

mod signals;

use eyre::eyre;
use nix::sys::signal::{self, Signal};
use nix::unistd::{getpgid, tcsetpgrp, Pid};
use std::fs::File;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::{pin, select};
use tracing::{debug, info, instrument};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
#[instrument]
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

    let mut subprocess = Subprocess::spawn(bin_name, bin_args)?;

    let tty = File::open("/dev/tty")?;
    if tty.is_terminal() {
        let pgid = subprocess.process_group_id()?;
        debug!("setting process group {} as foreground", &pgid);
        tcsetpgrp(tty.as_raw_fd(), pgid)?;
    }

    pin! {
        let subprocess_wait = subprocess.child.wait();
    }

    loop {
        select! {
            _ = &mut subprocess_wait => {
                println!("child exitted");
                break Ok(())
            },

            _ = sigs.next() => {
                signal::kill(subprocess.pid, Signal::SIGINT).unwrap();
            },
        }
    }
}

/// Subprocess represents a running processs.
///
/// It providers a wrapper around tokio::process::Child with some additional behavior as it
/// pertains to this application.
#[derive(Debug)]
struct Subprocess {
    /// Store the pid of the child process, for easy of access.
    pid: Pid,

    /// Store a reference to the Child instance.
    child: Child,
}

impl Subprocess {
    #[instrument]
    fn spawn(bin: String, args: Vec<String>) -> Result<Self> {
        let child = Command::new(&bin)
            .args(&args)
            .process_group(0)
            .kill_on_drop(true)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

        let pid = match child.id() {
            Some(pid) => Pid::from_raw(pid.try_into()?),
            None => return Err(eyre!("failed to get pid")),
        };

        debug!("spawned subprocess with pid {}", &pid);

        Ok(Subprocess { pid, child })
    }

    #[instrument]
    fn process_group_id(&self) -> Result<Pid> {
        Ok(getpgid(Some(self.pid))?)
    }
}

//async fn spawn_child(bin: String, args: Vec<String>) -> Child {
//    let child = Command::new(bin)
//        .args(args)
//        .process_group(0)
//        .spawn()
//        .expect("failed to start child process");
//
//    let pid = child.id().map(|id| Pid::from_raw(id as i32));
//
//    let pgid = getpgid(pid).unwrap();
//    dbg!(pgid);
//    dbg!(tcsetpgrp(std::io::stdin().as_raw_fd(), pgid).unwrap());
//
//    child
//}
