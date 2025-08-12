use clap::Parser;
use eyre::eyre;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::file_watcher::FileWatchConfig;
use crate::port_manager::PortBindingConfig;

type Result<T> = color_eyre::eyre::Result<T>;

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

    /// Signal polling interval (ms)
    #[arg(long, default_value = "100")]
    pub signal_poll_interval_ms: u64,

    /// Zombie reaping interval (ms)
    #[arg(long, default_value = "5000")]
    pub zombie_reap_interval_ms: u64,

    /// Command to execute
    pub command: String,

    /// Arguments for the command
    pub args: Vec<String>,
}

/// Configuration for the init system
#[derive(Debug, Clone)]
pub struct Config {
    /// The command to execute
    pub command: String,
    /// Arguments for the command
    pub args: Vec<String>,
    /// Signal polling interval in milliseconds (optimized for performance)
    pub signal_poll_interval: Duration,
    /// Zombie reaping interval in milliseconds
    pub zombie_reap_interval: Duration,
    /// Live-reload configuration
    pub live_reload: LiveReloadConfig,
    /// Port binding configuration
    pub port_binding: PortBindingConfig,
}

#[derive(Debug, Clone)]
pub struct LiveReloadConfig {
    pub enabled: bool,
    pub watch_path: Option<PathBuf>,
    pub debounce_ms: u64,
    pub restart_delay_ms: u64,
    pub graceful_timeout_secs: u64,
}

impl Config {
    /// Parse command line arguments into configuration
    pub fn from_cli(cli: Cli) -> Result<Self> {
        // Parse bind address
        let bind_address: IpAddr = cli
            .bind_addr
            .parse()
            .map_err(|e| eyre!("Invalid bind address '{}': {}", cli.bind_addr, e))?;

        // Determine watch path
        let watch_path = cli.watch_path.or_else(|| {
            if cli.live_reload {
                Some(PathBuf::from(&cli.command))
            } else {
                None
            }
        });

        Ok(Config {
            command: cli.command,
            args: cli.args,
            signal_poll_interval: Duration::from_millis(cli.signal_poll_interval_ms),
            zombie_reap_interval: Duration::from_millis(cli.zombie_reap_interval_ms),
            live_reload: LiveReloadConfig {
                enabled: cli.live_reload,
                watch_path,
                debounce_ms: cli.debounce_ms,
                restart_delay_ms: cli.restart_delay_ms,
                graceful_timeout_secs: cli.graceful_timeout_secs,
            },
            port_binding: PortBindingConfig {
                ports: cli.ports,
                bind_address,
                reuse_port: true,
            },
        })
    }

    /// Get file watch configuration if live-reload is enabled
    pub fn file_watch_config(&self) -> Option<FileWatchConfig> {
        if self.live_reload.enabled {
            self.live_reload.watch_path.as_ref().map(|path| FileWatchConfig {
                watch_path: path.clone(),
                debounce_ms: self.live_reload.debounce_ms,
                recursive: false,
            })
        } else {
            None
        }
    }
}