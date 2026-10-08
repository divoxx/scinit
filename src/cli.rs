use clap::Parser;
use eyre::eyre;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::environment::Environment;
use crate::file_watcher::FileWatchConfig;
use crate::graceful_timeout::{self, GracefulTimeout, HostEnv};
use crate::port_manager::PortBindingConfig;
use crate::process_manager::ProcessConfig;
use crate::program::resolve_program;
use crate::Result;

/// Command-line interface (the help text's description comes from Cargo.toml)
#[derive(Parser)]
#[command(name = "scinit")]
#[command(about)]
#[command(version)]
#[command(after_help = "\
Environment:
  SCINIT_LOG  Filter for scinit's own logs, in tracing EnvFilter syntax:
              a level (error, warn, info, debug, trace) and/or per-module
              directives such as scinit::file_watcher=debug.
              Default: error.

              Logs go to stderr only, one event per line, without
              timestamps:
                ERROR scinit: Failed to spawn process 'my-app': ...
                 INFO scinit::process_manager: Spawning process: ...
              Colored only when stderr is a terminal and NO_COLOR is unset.

              scinit does not read RUST_LOG; it reaches the child
              unchanged.

  SCINIT_GRACEFUL_TIMEOUT_SECS
              Graceful timeout in seconds when --graceful-timeout-secs
              isn't given. An invalid value is an error.")]
pub struct Cli {
    /// Enable live-reload functionality
    #[arg(long)]
    pub live_reload: bool,

    /// Path to watch for changes (default: the command's executable, looked up in PATH like exec does)
    #[arg(long)]
    pub watch_path: Option<PathBuf>,

    /// Comma-separated list of ports to bind
    #[arg(long, value_delimiter = ',')]
    pub ports: Vec<u16>,

    /// Address to bind ports to
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_addr: String,

    /// Set SO_REUSEPORT on the bound ports, so other processes can bind them too
    #[arg(long)]
    pub reuse_port: bool,

    /// Debounce time for file changes (ms)
    #[arg(long, default_value = "500")]
    pub debounce_ms: u64,

    /// Delay before restart after graceful shutdown (ms)
    #[arg(long, default_value = "1000")]
    pub restart_delay_ms: u64,

    /// How long a live-reload restart waits for the old child to exit before SIGKILL (seconds, needs --live-reload)
    #[arg(long, default_value = "2", requires = "live_reload")]
    pub restart_timeout_secs: u64,

    /// Graceful shutdown timeout (seconds) [default: by runtime, 8 if none detected]
    ///
    /// How long to wait for the child to exit after a termination signal
    /// before sending SIGKILL to its process group.
    ///
    /// Without this flag, SCINIT_GRACEFUL_TIMEOUT_SECS is used; without that,
    /// a default for the detected container runtime. The runtime's
    /// configured stop timeout isn't visible inside the container, so each
    /// default is a little below that runtime's default stop timeout,
    /// letting scinit's SIGKILL come first:
    ///   25  Kubernetes (KUBERNETES_SERVICE_HOST is set): default
    ///       terminationGracePeriodSeconds is 30
    ///    8  podman (/run/.containerenv exists): podman stop waits 10
    ///    8  Docker (/.dockerenv exists): docker stop waits 10
    ///    8  no runtime detected
    #[arg(long, verbatim_doc_comment)]
    pub graceful_timeout_secs: Option<u64>,

    /// Zombie reaping interval (ms, at least 1)
    #[arg(long, default_value = "5000", value_parser = clap::value_parser!(u64).range(1..))]
    pub zombie_reap_interval_ms: u64,

    /// Command to execute and its arguments, passed to the child unchanged
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// Configuration for the init system
#[derive(Debug, Clone)]
pub struct Config {
    /// The command to execute
    pub command: String,
    /// Arguments for the command
    pub args: Vec<String>,
    /// Zombie reaping interval
    pub zombie_reap_interval: Duration,
    /// Timeout for shutdowns after a termination signal
    pub graceful_timeout: GracefulTimeout,
    /// Live-reload configuration, `None` when disabled
    pub live_reload: Option<LiveReloadConfig>,
    /// Port binding configuration
    pub port_binding: PortBindingConfig,
}

#[derive(Debug, Clone)]
pub struct LiveReloadConfig {
    pub watch_path: PathBuf,
    pub debounce: Duration,
    pub restart_delay: Duration,
    /// Timeout for stopping the old child on a restart
    pub restart_timeout: Duration,
}

impl Config {
    /// Parse command line arguments into configuration, with defaults that
    /// depend on the environment taken from `host`
    pub fn from_cli(cli: Cli, host: &dyn HostEnv) -> Result<Self> {
        let mut command = cli.command.into_iter();
        let program = command.next().expect("clap requires a command");
        let args: Vec<String> = command.collect();

        // Parse bind address
        let bind_address: IpAddr = cli
            .bind_addr
            .parse()
            .map_err(|e| eyre!("Invalid bind address '{}': {}", cli.bind_addr, e))?;

        let live_reload = if cli.live_reload {
            let watch_path = match cli.watch_path {
                Some(path) => path,
                None => default_watch_path(&program)?,
            };
            Some(LiveReloadConfig {
                watch_path,
                debounce: Duration::from_millis(cli.debounce_ms),
                restart_delay: Duration::from_millis(cli.restart_delay_ms),
                restart_timeout: Duration::from_secs(cli.restart_timeout_secs),
            })
        } else {
            None
        };

        Ok(Config {
            command: program,
            args,
            zombie_reap_interval: Duration::from_millis(cli.zombie_reap_interval_ms),
            graceful_timeout: graceful_timeout::resolve(cli.graceful_timeout_secs, host)?,
            live_reload,
            port_binding: PortBindingConfig {
                ports: cli.ports,
                bind_address,
                reuse_port: cli.reuse_port,
            },
        })
    }

    /// Configuration for the managed child
    pub fn process_config(&self) -> ProcessConfig {
        // Restarts only happen with live reload
        let (restart_delay, restart_timeout) = self
            .live_reload
            .as_ref()
            .map_or((Duration::ZERO, Duration::ZERO), |live_reload| {
                (live_reload.restart_delay, live_reload.restart_timeout)
            });
        ProcessConfig {
            command: self.command.clone(),
            args: self.args.clone(),
            restart_delay,
            restart_timeout,
            graceful_shutdown_timeout: self.graceful_timeout.duration,
            environment: Environment::new(),
        }
    }

    /// Get file watch configuration if live-reload is enabled
    pub fn file_watch_config(&self) -> Option<FileWatchConfig> {
        self.live_reload
            .as_ref()
            .map(|live_reload| FileWatchConfig {
                watch_path: live_reload.watch_path.clone(),
                debounce: live_reload.debounce,
            })
    }
}

/// Watch path without `--watch-path`: the executable `command` runs
fn default_watch_path(command: &str) -> Result<PathBuf> {
    resolve_program(command).map_err(|_| {
        eyre!(
            "--live-reload: cannot find '{}' in PATH to watch; pass --watch-path",
            command
        )
    })
}
