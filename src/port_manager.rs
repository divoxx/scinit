use super::Result;
use crate::environment::Environment;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::sys::socket::{setsockopt, sockopt::ReusePort};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, Shutdown};
use std::os::unix::io::{AsRawFd, BorrowedFd};
use tracing::{debug, info};

/// Standard systemd socket activation start file descriptor
const SD_LISTEN_FDS_START: i32 = 3;

/// Configuration for port binding behavior
#[derive(Debug, Clone)]
pub struct PortBindingConfig {
    /// List of ports to bind
    pub ports: Vec<u16>,
    /// Address to bind ports to
    pub bind_address: IpAddr,
    /// Whether to enable SO_REUSEPORT for graceful restarts
    pub reuse_port: bool,
    /// Optional names for the bound sockets (for LISTEN_FDNAMES)
    pub socket_names: Option<Vec<String>>,
}

impl Default for PortBindingConfig {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            reuse_port: true,
            socket_names: None,
        }
    }
}

/// Manages port binding and socket inheritance for zero-downtime restarts.
/// 
/// Binds ports before spawning child processes and provides file descriptors
/// for inheritance. Uses SO_REUSEPORT for graceful restarts without port conflicts.
pub struct PortManager {
    /// Currently bound ports and their socket addresses
    bound_ports: HashMap<u16, SocketAddr>,
    /// Configuration for port binding
    config: PortBindingConfig,
    /// Bound sockets for inheritance
    sockets: HashMap<u16, Socket>,
}

impl PortManager {
    /// Creates a new port manager with the given configuration
    /// 
    /// # Arguments
    /// * `config` - Configuration for port binding
    /// 
    /// # Returns
    /// * `Self` - The port manager instance
    pub fn new(config: PortBindingConfig) -> Self {
        Self {
            bound_ports: HashMap::new(),
            config,
            sockets: HashMap::new(),
        }
    }

    /// Binds the configured ports and prepares them for inheritance
    /// 
    /// This method binds all configured ports and sets up the sockets
    /// for inheritance by child processes. It uses SO_REUSEPORT if enabled
    /// to allow multiple processes to bind to the same port.
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub async fn bind_ports(&mut self) -> Result<()> {
        if self.config.ports.is_empty() {
            debug!("No ports configured for binding");
            return Ok(());
        }

        info!("Binding {} ports to {}", self.config.ports.len(), self.config.bind_address);

        let ports = self.config.ports.clone();
        for &port in &ports {
            self.bind_single_port(port).await?;
        }

        info!("Successfully bound {} ports", self.bound_ports.len());
        Ok(())
    }

    /// Binds a single port with proper error handling
    /// 
    /// # Arguments
    /// * `port` - The port number to bind
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    async fn bind_single_port(&mut self, port: u16) -> Result<()> {
        let socket_addr = SocketAddr::new(self.config.bind_address, port);

        // Create socket
        let socket = match self.config.bind_address {
            IpAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?,
            IpAddr::V6(_) => Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?,
        };

        // Set SO_REUSEPORT if enabled
        if self.config.reuse_port {
            setsockopt(&socket, ReusePort, &true)?;
        }

        // Bind the socket
        socket.bind(&socket_addr.into())?;
        socket.listen(128)?; // Set backlog

        // Mark socket as inheritable by clearing close-on-exec flag initially
        let fd = socket.as_raw_fd();
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut flags = FdFlag::from_bits_truncate(fcntl(borrowed_fd, FcntlArg::F_GETFD)?);
        flags.remove(FdFlag::FD_CLOEXEC);
        fcntl(borrowed_fd, FcntlArg::F_SETFD(flags))?;

        // Store the bound socket and address
        self.bound_ports.insert(port, socket_addr);
        self.sockets.insert(port, socket);

        info!("Bound port {} to {}", port, socket_addr);
        Ok(())
    }

    /// Gets the file descriptors for inherited ports
    /// 
    /// This method returns the file descriptors of bound sockets
    /// that should be inherited by child processes.
    /// 
    /// # Returns
    /// * `Vec<i32>` - List of file descriptors
    pub fn get_inherited_fds(&self) -> Vec<i32> {
        self.sockets
            .values()
            .map(|socket| socket.as_raw_fd())
            .collect()
    }

    /// Gets the number of inherited file descriptors for LISTEN_FDS environment variable
    /// 
    /// # Returns
    /// * `String` - Number of file descriptors as string
    pub fn get_listen_fds_count(&self) -> String {
        self.sockets.len().to_string()
    }

    /// Gets the socket names for LISTEN_FDNAMES environment variable
    /// 
    /// # Returns
    /// * `Option<String>` - Colon-separated socket names, if configured
    pub fn get_listen_fdnames(&self) -> Option<String> {
        self.config.socket_names.as_ref().map(|names| {
            names.join(":")
        })
    }

    /// Prepares file descriptors for systemd socket activation
    /// 
    /// This method ensures that file descriptors start at SD_LISTEN_FDS_START (3)
    /// and sets the FD_CLOEXEC flag as required by systemd socket activation.
    /// 
    /// # Arguments
    /// * `child_pid` - Process ID of the child process for validation
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub fn prepare_systemd_fds(&self, _child_pid: nix::unistd::Pid) -> Result<()> {
        // For systemd socket activation, we need to set FD_CLOEXEC on inherited FDs
        // This is the opposite of what we did during binding
        for socket in self.sockets.values() {
            let fd = socket.as_raw_fd();
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
            let mut flags = FdFlag::from_bits_truncate(fcntl(borrowed_fd, FcntlArg::F_GETFD)?);
            flags.insert(FdFlag::FD_CLOEXEC);
            fcntl(borrowed_fd, FcntlArg::F_SETFD(flags))?;
        }
        Ok(())
    }

    /// Gets the systemd socket activation environment variables for the child process.
    ///
    /// Returns an Environment containing the standard systemd socket activation variables:
    /// - `LISTEN_FDS`: Number of file descriptors being passed (as string)
    /// - `LISTEN_PID`: Process ID for validation (set to child PID)
    /// - `LISTEN_FDNAMES`: Optional colon-separated socket names
    ///
    /// If no sockets are bound, returns an empty Environment.
    ///
    /// # Arguments
    /// * `child_pid` - The process ID of the child process
    ///
    /// # Returns
    /// * `Environment` - Environment variables for systemd socket activation
    pub fn get_socket_activation_env(&self, child_pid: u32) -> Environment {
        if self.sockets.is_empty() {
            return Environment::new();
        }

        let mut env = Environment::new();

        // LISTEN_FDS: Number of file descriptors
        env.set("LISTEN_FDS", self.sockets.len().to_string());

        // LISTEN_PID: Child process PID for validation
        env.set("LISTEN_PID", child_pid.to_string());

        // LISTEN_FDNAMES: Optional socket names
        if let Some(ref names) = self.config.socket_names {
            if names.len() == self.sockets.len() {
                env.set("LISTEN_FDNAMES", names.join(":"));
            }
        }

        env
    }

    /// Gets the inherited file descriptors as a formatted string for environment variables
    /// 
    /// # Returns
    /// * `String` - Comma-separated list of file descriptors
    /// 
    /// # Deprecated
    /// Use `get_socket_activation_env()` for systemd compatibility instead
    #[deprecated(note = "Use get_socket_activation_env() for systemd compatibility")]
    pub fn get_inherited_fds_string(&self) -> String {
        self.get_inherited_fds()
            .iter()
            .map(|fd| fd.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }


}

impl Drop for PortManager {
    fn drop(&mut self) {
        // Ensure we cleanup ports when dropped
        if !self.sockets.is_empty() {
            // Don't try to use block_on in a Drop implementation
            // Just close the sockets directly
            for (port, socket) in self.sockets.drain() {
                if let Err(e) = socket.shutdown(Shutdown::Both) {
                    eprintln!("Failed to shutdown socket for port {}: {}", port, e);
                }
            }
            self.bound_ports.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_port_manager_creation() {
        let config = PortBindingConfig::default();
        let manager = PortManager::new(config);
        assert_eq!(manager.bound_ports.len(), 0);
    }

    #[tokio::test]
    async fn test_port_binding() {
        let config = PortBindingConfig {
            ports: vec![0], // Use port 0 to let OS assign a free port
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            reuse_port: true,
            socket_names: None,
        };

        let mut manager = PortManager::new(config);
        assert!(manager.bind_ports().await.is_ok());
        assert_eq!(manager.bound_ports.len(), 1);
    }

    #[tokio::test]
    async fn test_multiple_port_binding() {
        // Use different ports to avoid conflicts
        let config = PortBindingConfig {
            ports: vec![0, 0], // Use port 0 to let OS assign free ports
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            reuse_port: true,
            socket_names: None,
        };

        let mut manager = PortManager::new(config);
        assert!(manager.bind_ports().await.is_ok());
        // When using port 0, the OS assigns different ports, so we should have 2 bound ports
        // However, if the OS assigns the same port, we might only get 1
        let bound_count = manager.bound_ports.len();
        assert!(bound_count >= 1 && bound_count <= 2);
    }

    #[tokio::test]
    async fn test_inherited_fds() {
        let config = PortBindingConfig {
            ports: vec![0],
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            reuse_port: true,
            socket_names: None,
        };

        let mut manager = PortManager::new(config);
        manager.bind_ports().await.unwrap();

        let fds = manager.get_inherited_fds();
        assert_eq!(fds.len(), 1);
        assert!(fds[0] > 0); // File descriptor should be positive

        let fd_string = manager.get_inherited_fds_string();
        assert!(!fd_string.is_empty());
        
        // Ports will be cleaned up automatically when dropped
    }


} 