//! Integration testing framework for scinit init system
//! 
//! This module provides comprehensive testing capabilities for signal handling,
//! socket inheritance, process lifecycle management, and performance validation.

pub mod infrastructure;
pub mod scenarios;

// Re-export commonly used types for convenience
pub use infrastructure::{ProcessTestHarness, SignalTestFramework, TestProcess};