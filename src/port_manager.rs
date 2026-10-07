use crate::environment::Environment;
use crate::Result;
use nix::sys::socket::{setsockopt, sockopt::ReusePort};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr};
use std::os::unix::io::{AsRawFd, RawFd};
use tracing::{debug, info, warn};

/// Configuration for port binding behavior
#[derive(Debug, Clone)]
pub struct PortBindingConfig {
    /// List of ports to bind
    pub ports: Vec<u16>,
    /// Address to bind ports to
    pub bind_address: IpAddr,
}

impl Default for PortBindingConfig {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        }
    }
}

/// Manages port binding and socket inheritance for zero-downtime restarts.
///
/// Binds ports before spawning child processes and provides file descriptors
/// for inheritance. Uses SO_REUSEPORT for graceful restarts without port conflicts.
pub struct PortManager {
    /// Configuration for port binding
    config: PortBindingConfig,
    /// Bound sockets for inheritance
    sockets: HashMap<u16, Socket>,
}

impl PortManager {
    pub fn new(config: PortBindingConfig) -> Self {
        Self {
            config,
            sockets: HashMap::new(),
        }
    }

    /// Binds the configured ports that aren't bound yet, with SO_REUSEPORT
    pub fn bind_ports(&mut self) -> Result<()> {
        if self.config.ports.is_empty() {
            debug!("No ports configured for binding");
            return Ok(());
        }

        info!(
            "Binding {} ports to {}",
            self.config.ports.len(),
            self.config.bind_address
        );

        // Sockets are bound once and kept for scinit's lifetime, so every
        // child (including after a live-reload restart) gets the same
        // listeners and connections queue in their backlog in between
        let unbound: Vec<u16> = self
            .unique_ports()
            .filter(|port| !self.sockets.contains_key(port))
            .collect();
        for port in unbound {
            self.bind_single_port(port)?;
        }

        info!("Successfully bound {} ports", self.sockets.len());
        Ok(())
    }

    fn bind_single_port(&mut self, port: u16) -> Result<()> {
        let socket_addr = SocketAddr::new(self.config.bind_address, port);

        let socket = Socket::new(
            Domain::for_address(socket_addr),
            Type::STREAM,
            Some(Protocol::TCP),
        )?;
        setsockopt(&socket, ReusePort, &true)?;
        socket.bind(&socket_addr.into())?;
        socket.listen(128)?;

        // The socket stays close-on-exec (socket2's default): the child gets
        // a copy at its systemd fd number instead, see socket_activation
        self.sockets.insert(port, socket);

        info!("Bound port {} to {}", port, socket_addr);
        Ok(())
    }

    /// File descriptors of the bound sockets, in `--ports` order. This is the
    /// order they are passed to the child in (fd 3, 4, ...).
    pub fn listen_fds(&self) -> Vec<RawFd> {
        self.unique_ports()
            .filter_map(|port| self.sockets.get(&port))
            .map(|socket| socket.as_raw_fd())
            .collect()
    }

    /// The configured ports in `--ports` order, without repeats
    fn unique_ports(&self) -> impl Iterator<Item = u16> + '_ {
        let mut seen = HashSet::new();
        self.config.ports.iter().copied().filter(move |&port| seen.insert(port))
    }

    /// `LISTEN_FDS` for the child, or nothing if no sockets are bound.
    ///
    /// `LISTEN_PID` is not included: only the forked child knows its pid, so
    /// `socket_activation` fills it in there.
    pub fn socket_activation_env(&self) -> Environment {
        let mut env = Environment::new();
        if !self.sockets.is_empty() {
            env.set("LISTEN_FDS", self.sockets.len().to_string());
        }
        env
    }
}

impl Drop for PortManager {
    fn drop(&mut self) {
        for (port, socket) in self.sockets.drain() {
            if let Err(e) = socket.shutdown(Shutdown::Both) {
                warn!("Failed to shutdown socket for port {}: {}", port, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(ports: Vec<u16>) -> PortBindingConfig {
        PortBindingConfig {
            ports,
            ..Default::default()
        }
    }

    #[test]
    fn test_port_manager_creation() {
        let manager = PortManager::new(PortBindingConfig::default());
        assert_eq!(manager.sockets.len(), 0);
    }

    #[test]
    fn test_port_binding() {
        // Port 0 lets the OS assign a free port
        let mut manager = PortManager::new(ports(vec![0]));
        assert!(manager.bind_ports().is_ok());
        assert_eq!(manager.sockets.len(), 1);
    }

    #[test]
    fn test_multiple_port_binding() {
        // Both entries are port 0, which is only bound once
        let mut manager = PortManager::new(ports(vec![0, 0]));
        assert!(manager.bind_ports().is_ok());
        let bound_count = manager.sockets.len();
        assert!((1..=2).contains(&bound_count));
    }

    #[test]
    fn test_inherited_fds() {
        let mut manager = PortManager::new(ports(vec![0]));
        manager.bind_ports().unwrap();

        let fds = manager.listen_fds();
        assert_eq!(fds.len(), 1);
        assert!(fds[0] > 0);
    }
}
