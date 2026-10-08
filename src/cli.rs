use clap::Parser;
use eyre::eyre;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::environment::Environment;
use crate::file_watcher::FileWatchConfig;
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
              unchanged.")]
pub struct Cli {
    /// Restart the child when its executable (looked up in PATH like exec does) changes
    #[arg(long)]
    pub watch: bool,

    /// With --watch, restart when <executable>.scinit next to the executable is created or touched, instead of when the executable changes
    #[arg(long, requires = "watch")]
    pub watch_sentinel: bool,

    /// With --watch, also restart when this file or directory (not recursive) changes; can be repeated
    #[arg(long, requires = "watch", value_name = "PATH")]
    pub watch_extra: Vec<PathBuf>,

    /// Comma-separated list of ports to bind
    #[arg(long, value_delimiter = ',')]
    pub ports: Vec<u16>,

    /// Address to bind ports to
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_addr: String,

    /// Set SO_REUSEPORT on the bound ports, so other processes can bind them too
    #[arg(long)]
    pub reuse_port: bool,

    /// With --watch, debounce time for file changes (ms)
    #[arg(long, default_value = "500", requires = "watch")]
    pub debounce_ms: u64,

    /// With --watch, delay before restart after graceful shutdown (ms)
    #[arg(long, default_value = "1000", requires = "watch")]
    pub restart_delay_ms: u64,

    /// Graceful shutdown timeout (seconds)
    #[arg(long, default_value = "30")]
    pub graceful_timeout_secs: u64,

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
    /// Graceful shutdown timeout (with or without live reload)
    pub graceful_timeout: Duration,
    /// Live-reload configuration, `None` when disabled
    pub live_reload: Option<LiveReloadConfig>,
    /// Port binding configuration
    pub port_binding: PortBindingConfig,
}

#[derive(Debug, Clone)]
pub struct LiveReloadConfig {
    /// Paths whose content changes restart the child: the executable (unless
    /// `--watch-sentinel` is set) and the `--watch-extra` paths
    pub watch_paths: Vec<PathBuf>,
    /// `<executable>.scinit` with `--watch-sentinel`
    pub sentinel: Option<PathBuf>,
    pub debounce: Duration,
    pub restart_delay: Duration,
}

impl Config {
    /// Parse command line arguments into configuration
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let mut command = cli.command.into_iter();
        let program = command.next().expect("clap requires a command");
        let args: Vec<String> = command.collect();

        // Parse bind address
        let bind_address: IpAddr = cli
            .bind_addr
            .parse()
            .map_err(|e| eyre!("Invalid bind address '{}': {}", cli.bind_addr, e))?;

        let live_reload = if cli.watch {
            let executable = watched_executable(&program)?;
            let mut watch_paths = Vec::new();
            let sentinel = if cli.watch_sentinel {
                Some(sentinel_path(&executable))
            } else {
                watch_paths.push(executable);
                None
            };
            watch_paths.extend(cli.watch_extra);
            Some(LiveReloadConfig {
                watch_paths,
                sentinel,
                debounce: Duration::from_millis(cli.debounce_ms),
                restart_delay: Duration::from_millis(cli.restart_delay_ms),
            })
        } else {
            None
        };

        Ok(Config {
            command: program,
            args,
            zombie_reap_interval: Duration::from_millis(cli.zombie_reap_interval_ms),
            graceful_timeout: Duration::from_secs(cli.graceful_timeout_secs),
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
        ProcessConfig {
            command: self.command.clone(),
            args: self.args.clone(),
            // Restarts only happen with live reload
            restart_delay: self
                .live_reload
                .as_ref()
                .map_or(Duration::ZERO, |live_reload| live_reload.restart_delay),
            graceful_shutdown_timeout: self.graceful_timeout,
            environment: Environment::new(),
        }
    }

    /// Get file watch configuration if live-reload is enabled
    pub fn file_watch_config(&self) -> Option<FileWatchConfig> {
        self.live_reload
            .as_ref()
            .map(|live_reload| FileWatchConfig {
                watch_paths: live_reload.watch_paths.clone(),
                sentinel: live_reload.sentinel.clone(),
                debounce: live_reload.debounce,
            })
    }
}

/// The executable `command` runs, which `--watch` watches
fn watched_executable(command: &str) -> Result<PathBuf> {
    resolve_program(command)
        .map_err(|_| eyre!("--watch: cannot find '{}' in PATH to watch", command))
}

/// The `--watch-sentinel` file for `executable`: `<executable>.scinit` next to it
fn sentinel_path(executable: &Path) -> PathBuf {
    let mut path = executable.as_os_str().to_os_string();
    path.push(".scinit");
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        let cli =
            Cli::try_parse_from(std::iter::once("scinit").chain(args.iter().copied())).unwrap();
        Config::from_cli(cli).unwrap()
    }

    #[test]
    fn test_watch_watches_the_executable() {
        let live_reload = config(&["--watch", "/bin/sh"]).live_reload.unwrap();
        assert_eq!(live_reload.watch_paths, vec![PathBuf::from("/bin/sh")]);
        assert_eq!(live_reload.sentinel, None);
    }

    #[test]
    fn test_watch_sentinel_replaces_the_executable() {
        let live_reload = config(&["--watch", "--watch-sentinel", "/app/bin/server"])
            .live_reload
            .unwrap();
        assert!(live_reload.watch_paths.is_empty());
        assert_eq!(
            live_reload.sentinel,
            Some(PathBuf::from("/app/bin/server.scinit"))
        );
    }

    #[test]
    fn test_watch_extra_paths_are_added() {
        let live_reload = config(&[
            "--watch",
            "--watch-sentinel",
            "--watch-extra",
            "config",
            "--watch-extra",
            "templates",
            "/app/bin/server",
        ])
        .live_reload
        .unwrap();
        assert_eq!(
            live_reload.watch_paths,
            vec![PathBuf::from("config"), PathBuf::from("templates")]
        );
    }

    #[test]
    fn test_watch_options_require_watch() {
        for args in [
            &["--watch-sentinel", "app"][..],
            &["--watch-extra", "config", "app"],
            &["--debounce-ms", "100", "app"],
            &["--restart-delay-ms", "100", "app"],
        ] {
            let result = Cli::try_parse_from(std::iter::once("scinit").chain(args.iter().copied()));
            assert!(result.is_err(), "{:?} must require --watch", args);
        }
        assert!(config(&["app"]).live_reload.is_none());
    }
}
