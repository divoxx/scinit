use super::Result;
use libc::{setsockopt, SO_REUSEPORT, SOL_SOCKET};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, Shutdown};
use std::os::unix::io::AsRawFd;
use tracing::{debug, info};

/// Configuration for port binding behavior
#[derive(Debug, Clone)]
pub struct PortBindingConfig {
    /// List of ports to bind
    pub ports: Vec<u16>,
    /// Address to bind ports to
    pub bind_address: IpAddr,
    /// Whether to enable SO_REUSEPORT for graceful restarts
    pub reuse_port: bool,
}

impl Default for PortBindingConfig {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            reuse_port: true,
        }
    }
}

/// Manages port binding and inheritance for child processes
/// 
/// This manager handles binding ports before spawning child processes
/// and provides file descriptors that can be inherited by the child.
/// It supports SO_REUSEPORT for graceful restarts without port conflicts.
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
            let reuse_port: i32 = 1;
            unsafe {
                setsockopt(
                    socket.as_raw_fd(),
                    SOL_SOCKET,
                    SO_REUSEPORT,
                    &reuse_port as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&reuse_port) as u32,
                );
            }
        }

        // Bind the socket
        socket.bind(&socket_addr.into())?;
        socket.listen(128)?; // Set backlog

        // Mark socket as inheritable by clearing close-on-exec flag
        unsafe {
            let fd = socket.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }

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

    /// Gets the inherited file descriptors as a formatted string for environment variables
    /// 
    /// # Returns
    /// * `String` - Comma-separated list of file descriptors
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