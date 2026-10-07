use clap::Parser;
use eyre::eyre;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::file_watcher::FileWatchConfig;
use crate::port_manager::PortBindingConfig;
use crate::Result;

/// A live-reloading init system for managing subprocesses
#[derive(Parser)]
#[command(name = "scinit")]
#[command(about = "A live-reloading init system for managing subprocesses")]
#[command(version)]
pub struct Cli {
    /// Enable live-reload functionality
    #[arg(long)]
    pub live_reload: bool,

    /// Path to watch for changes (default: executable path)
    #[arg(long)]
    pub watch_path: Option<PathBuf>,

    /// Comma-separated list of ports to bind
    #[arg(long, value_delimiter = ',')]
    pub ports: Vec<u16>,

    /// Address to bind ports to
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_addr: String,

    /// Debounce time for file changes (ms)
    #[arg(long, default_value = "500")]
    pub debounce_ms: u64,

    /// Delay before restart after graceful shutdown (ms)
    #[arg(long, default_value = "1000")]
    pub restart_delay_ms: u64,

    /// Graceful shutdown timeout (seconds)
    #[arg(long, default_value = "30")]
    pub graceful_timeout_secs: u64,

    /// Zombie reaping interval (ms)
    #[arg(long, default_value = "5000")]
    pub zombie_reap_interval_ms: u64,

    /// Command to execute
    pub command: String,

    /// Arguments for the command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
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
    /// Graceful shutdown timeout in seconds (with or without live reload)
    pub graceful_timeout_secs: u64,
    /// Live-reload configuration, `None` when disabled
    pub live_reload: Option<LiveReloadConfig>,
    /// Port binding configuration
    pub port_binding: PortBindingConfig,
}

#[derive(Debug, Clone)]
pub struct LiveReloadConfig {
    pub watch_path: PathBuf,
    pub debounce_ms: u64,
    pub restart_delay_ms: u64,
}

impl Config {
    /// Parse command line arguments into configuration
    pub fn from_cli(cli: Cli) -> Result<Self> {
        // Parse bind address
        let bind_address: IpAddr = cli
            .bind_addr
            .parse()
            .map_err(|e| eyre!("Invalid bind address '{}': {}", cli.bind_addr, e))?;

        // The watch path defaults to the command itself
        let live_reload = cli.live_reload.then(|| LiveReloadConfig {
            watch_path: cli
                .watch_path
                .unwrap_or_else(|| PathBuf::from(&cli.command)),
            debounce_ms: cli.debounce_ms,
            restart_delay_ms: cli.restart_delay_ms,
        });

        Ok(Config {
            command: cli.command,
            args: cli.args,
            zombie_reap_interval: Duration::from_millis(cli.zombie_reap_interval_ms),
            graceful_timeout_secs: cli.graceful_timeout_secs,
            live_reload,
            port_binding: PortBindingConfig {
                ports: cli.ports,
                bind_address,
            },
        })
    }

    /// Get file watch configuration if live-reload is enabled
    pub fn file_watch_config(&self) -> Option<FileWatchConfig> {
        self.live_reload.as_ref().map(|live_reload| FileWatchConfig {
            watch_path: live_reload.watch_path.clone(),
            debounce_ms: live_reload.debounce_ms,
        })
    }
}
