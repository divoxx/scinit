//! Integration tests for scinit.
//!
//! Every test drives the real `scinit` binary with the `scinit-test-child`
//! fixture (tests/fixtures/test_child.rs) as its child, and asserts on what
//! the child reports plus scinit's own exit status.

pub mod harness;
pub mod scenarios;
