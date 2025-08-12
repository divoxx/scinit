# RFC: Comprehensive Integration Testing Framework for scinit

**RFC Number:** RFC-2025-001  
**Date:** 2025-07-27  
**Status:** Draft  
**Author:** Claude Code  

## Summary

This RFC proposes the implementation of a comprehensive integration testing framework for the scinit init system. The framework will provide automated testing for signal handling, socket inheritance, process lifecycle management, failure scenarios, and performance characteristics. This addresses critical testing gaps identified in the current codebase and ensures proper init system semantics validation.

## Motivation

### Current State Analysis

The scinit project currently has:
- **Basic unit tests**: Embedded in individual modules but incomplete
- **Manual test scripts**: Shell scripts for basic signal testing (`test_ctrl_c.sh`, `test_signal_debug.sh`, etc.)
- **Echo server**: Exists for socket inheritance validation but lacks automated tests
- **Integration test references**: Mentioned in CLAUDE.md but not implemented

### Problem Statement

**Critical Testing Gaps:**
1. **Signal Handling**: No automated tests for SIGTERM, SIGINT, SIGKILL escalation, signal forwarding semantics
2. **Socket Inheritance**: No validation of zero-downtime restart capabilities
3. **Process Lifecycle**: Missing tests for graceful shutdown, zombie reaping, process group management
4. **Failure Scenarios**: No testing of error conditions, resource exhaustion, malformed inputs
5. **Performance Validation**: No regression testing for signal response times, memory usage
6. **Container Compatibility**: No validation of proper init system behavior in containerized environments

### Business/Technical Drivers

- **Reliability**: Init systems must handle all signal scenarios correctly
- **Performance**: Signal response must remain under 100ms for production use
- **Container Support**: Must validate proper PID 1 behavior in Docker/Kubernetes
- **Maintenance**: Automated tests reduce manual verification overhead
- **Compliance**: Init systems have specific POSIX requirements that must be validated

## Detailed Design

### Architecture Overview

The integration testing framework follows a layered architecture:

```
┌─────────────────────────────────────────┐
│           Test Orchestrator             │
│        (tests/integration/)             │
├─────────────────────────────────────────┤
│         Test Scenario Engine            │
│     - Signal Tests                      │
│     - Socket Tests                      │
│     - Lifecycle Tests                   │
│     - Performance Tests                 │
├─────────────────────────────────────────┤
│         Test Infrastructure             │
│   - Process Spawning                    │
│   - Signal Injection                    │
│   - Socket Validation                   │
│   - Performance Measurement             │
├─────────────────────────────────────────┤
│           Test Utilities                │
│   - Test Servers                        │
│   - Helper Functions                    │
│   - Mock Services                       │
└─────────────────────────────────────────┘
```

### Core Components

#### 1. Test Infrastructure (`tests/integration/infrastructure/`)

**ProcessTestHarness**
```rust
pub struct ProcessTestHarness {
    scinit_path: PathBuf,
    test_server_path: PathBuf,
    temp_dir: TempDir,
    environment: HashMap<String, String>,
}

impl ProcessTestHarness {
    pub async fn spawn_scinit(&self, args: &[&str]) -> Result<TestProcess>;
    pub async fn spawn_test_server(&self, port: u16) -> Result<TestProcess>;
    pub fn cleanup(&mut self) -> Result<()>;
}
```

**SignalTestFramework**
```rust
pub struct SignalTestFramework {
    harness: ProcessTestHarness,
    signal_timings: HashMap<Signal, Duration>,
}

impl SignalTestFramework {
    pub async fn send_signal(&self, process: &TestProcess, signal: Signal) -> Result<()>;
    pub async fn verify_signal_forwarding(&self, parent: &TestProcess, child: &TestProcess, signal: Signal) -> Result<()>;
    pub async fn measure_signal_response_time(&self, process: &TestProcess, signal: Signal) -> Result<Duration>;
}
```

**SocketTestFramework**
```rust
pub struct SocketTestFramework {
    harness: ProcessTestHarness,
    port_ranges: Vec<PortRange>,
}

impl SocketTestFramework {
    pub async fn test_socket_inheritance(&self, ports: &[u16]) -> Result<InheritanceResult>;
    pub async fn verify_zero_downtime_restart(&self, port: u16) -> Result<DowntimeMetrics>;
    pub async fn validate_fd_passing(&self, expected_fds: &[RawFd]) -> Result<()>;
}
```

#### 2. Test Scenarios (`tests/integration/scenarios/`)

**Signal Handling Tests**
- `test_sigterm_graceful_shutdown()`: Validates SIGTERM → graceful shutdown sequence
- `test_sigint_interrupt_handling()`: Tests SIGINT propagation and cleanup
- `test_sigkill_force_termination()`: Validates SIGKILL escalation after timeout
- `test_signal_forwarding()`: Tests SIGUSR1/SIGUSR2/SIGHUP forwarding to child processes
- `test_sigchld_zombie_reaping()`: Validates proper zombie process cleanup
- `test_critical_signal_passthrough()`: Ensures SIGFPE, SIGILL, SIGSEGV are never blocked

**Socket Inheritance Tests**
- `test_single_port_inheritance()`: Basic socket inheritance validation
- `test_multiple_port_inheritance()`: Tests SO_REUSEPORT with multiple ports
- `test_fd_lifecycle_management()`: Validates FD_CLOEXEC handling
- `test_zero_downtime_restart()`: Measures actual downtime during restarts
- `test_socket_inheritance_failure_modes()`: Tests invalid FD scenarios

**Process Lifecycle Tests**
- `test_process_group_creation()`: Validates proper process group isolation
- `test_graceful_shutdown_timeout()`: Tests timeout escalation to SIGKILL
- `test_child_process_monitoring()`: Validates child exit status handling
- `test_restart_delay_behavior()`: Tests configurable restart delays
- `test_working_directory_inheritance()`: Validates process working directory

**Failure Scenario Tests**
- `test_child_crash_handling()`: Validates container exit on child crash
- `test_resource_exhaustion()`: Tests behavior under resource limits
- `test_malformed_command_handling()`: Tests invalid command handling
- `test_permission_denied_scenarios()`: Tests privilege escalation failures
- `test_file_descriptor_exhaustion()`: Tests FD limit scenarios

#### 3. Performance Testing (`tests/integration/performance/`)

**Performance Benchmarks**
```rust
pub struct PerformanceBenchmarks {
    measurements: HashMap<String, PerformanceMetric>,
    baseline_results: HashMap<String, Duration>,
}

impl PerformanceBenchmarks {
    pub async fn benchmark_signal_response_time(&mut self) -> Result<Duration>;
    pub async fn benchmark_restart_performance(&mut self) -> Result<RestartMetrics>;
    pub async fn benchmark_memory_usage(&mut self) -> Result<MemoryMetrics>;
    pub async fn benchmark_cpu_utilization(&mut self) -> Result<CpuMetrics>;
}
```

**Performance Metrics**
- Signal response time (target: <100ms)
- Process restart time (target: <1s)
- Memory usage patterns
- CPU utilization under load
- File descriptor usage

#### 4. Test Utilities (`tests/integration/utils/`)

**Enhanced Echo Server**
```rust
// Extends existing echo server with testing capabilities
pub struct TestEchoServer {
    bind_ports: Vec<u16>,
    inherited_fds: Vec<RawFd>,
    metrics: ServerMetrics,
}

impl TestEchoServer {
    pub fn from_inherited_fds() -> Result<Self>;
    pub async fn start_with_metrics(&mut self) -> Result<()>;
    pub fn get_metrics(&self) -> &ServerMetrics;
    pub async fn simulate_load(&self, connections: usize) -> Result<LoadMetrics>;
}
```

**Test Assertions**
```rust
pub mod assertions {
    pub fn assert_signal_response_time(actual: Duration, expected_max: Duration);
    pub fn assert_process_state(process: &TestProcess, expected: ProcessState);
    pub fn assert_fd_inheritance(inherited: &[RawFd], expected: &[RawFd]);
    pub fn assert_zero_downtime(metrics: &DowntimeMetrics);
}
```

### File Structure

```
tests/
├── integration/
│   ├── mod.rs                          # Main integration test module
│   ├── infrastructure/
│   │   ├── mod.rs
│   │   ├── process_harness.rs          # Process spawning and management
│   │   ├── signal_framework.rs         # Signal testing infrastructure
│   │   └── socket_framework.rs         # Socket inheritance testing
│   ├── scenarios/
│   │   ├── mod.rs
│   │   ├── signal_handling_tests.rs    # Signal behavior tests
│   │   ├── socket_inheritance_tests.rs # Socket inheritance validation
│   │   ├── process_lifecycle_tests.rs  # Process management tests
│   │   └── failure_scenario_tests.rs   # Error condition tests
│   ├── performance/
│   │   ├── mod.rs
│   │   ├── benchmarks.rs               # Performance regression tests
│   │   └── metrics.rs                  # Performance measurement utilities
│   └── utils/
│       ├── mod.rs
│       ├── test_servers.rs             # Enhanced test servers
│       ├── assertions.rs               # Custom test assertions
│       └── helpers.rs                  # Common test utilities
└── fixtures/
    ├── test_configs/                   # Test configuration files
    ├── test_scripts/                   # Helper scripts for complex scenarios
    └── expected_outputs/               # Expected output files for validation
```

### Test Data Models

```rust
#[derive(Debug, Clone)]
pub struct TestProcess {
    pub pid: Pid,
    pub process_group: Pid,
    pub command: String,
    pub args: Vec<String>,
    pub start_time: Instant,
    pub child: Child,
}

#[derive(Debug)]
pub struct SignalTestResult {
    pub signal: Signal,
    pub response_time: Duration,
    pub forwarded_to_child: bool,
    pub child_exit_status: Option<ExitStatus>,
    pub cleanup_successful: bool,
}

#[derive(Debug)]
pub struct InheritanceResult {
    pub inherited_fds: Vec<RawFd>,
    pub bound_ports: Vec<u16>,
    pub inheritance_successful: bool,
    pub fd_validation_results: HashMap<RawFd, bool>,
}

#[derive(Debug)]
pub struct PerformanceMetric {
    pub name: String,
    pub value: Duration,
    pub baseline: Option<Duration>,
    pub threshold: Duration,
    pub passed: bool,
}
```

## Implementation Plan

### Phase 1: Core Infrastructure (Week 1-2)
1. **Setup test directory structure**
   - Create `tests/integration/` hierarchy
   - Configure Cargo.toml for integration tests
   - Setup test dependencies

2. **Implement ProcessTestHarness**
   - Process spawning utilities
   - Cleanup mechanisms
   - Environment management

3. **Basic Signal Testing Framework**
   - Signal injection capabilities
   - Response time measurement
   - Process state validation

**Deliverables:**
- Working test infrastructure
- Basic signal tests (SIGTERM, SIGINT)
- Documentation for test framework usage

### Phase 2: Signal Handling Tests (Week 3)
1. **Complete signal test scenarios**
   - All signal types (SIGTERM, SIGINT, SIGKILL, SIGUSR1, SIGUSR2, SIGHUP, SIGCHLD)
   - Signal forwarding validation
   - Critical signal passthrough tests

2. **Process lifecycle tests**
   - Graceful shutdown sequences
   - Timeout handling
   - Zombie reaping validation

**Deliverables:**
- Comprehensive signal handling test suite
- Process lifecycle validation tests
- Signal forwarding verification

### Phase 3: Socket Inheritance Tests (Week 4)
1. **Socket inheritance framework**
   - FD passing validation
   - SO_REUSEPORT testing
   - Zero-downtime restart measurement

2. **Enhanced echo server**
   - Metrics collection
   - Load simulation
   - Inheritance validation

**Deliverables:**
- Socket inheritance test suite
- Zero-downtime restart validation
- Enhanced test echo server

### Phase 4: Failure Scenarios & Performance (Week 5)
1. **Failure scenario tests**
   - Resource exhaustion scenarios
   - Invalid command handling
   - Permission failures

2. **Performance testing framework**
   - Baseline establishment
   - Regression detection
   - Performance reporting

**Deliverables:**
- Failure scenario test suite
- Performance benchmarking framework
- Automated performance regression detection

### Phase 5: CI/CD Integration & Documentation (Week 6)
1. **CI/CD integration**
   - GitHub Actions workflow
   - Container testing environment
   - Performance tracking

2. **Documentation and validation**
   - Test framework documentation
   - Usage guidelines
   - Acceptance criteria validation

**Deliverables:**
- Complete CI/CD integration
- Comprehensive documentation
- Validated test framework

## Test Scenarios Specification

### Signal Handling Test Matrix

| Signal | Test Scenario | Expected Behavior | Validation Method |
|--------|---------------|-------------------|-------------------|
| SIGTERM | Graceful shutdown | Forward to child, wait for exit, clean shutdown | Process exit status, timing |
| SIGINT | Interrupt handling | Forward to child, graceful shutdown | Signal propagation, cleanup |
| SIGKILL | Force termination | Immediate termination | Process state, cleanup |
| SIGUSR1 | User signal forwarding | Forward to child only | Child receives signal |
| SIGUSR2 | User signal forwarding | Forward to child only | Child receives signal |
| SIGHUP | Hangup forwarding | Forward to child only | Child receives signal |
| SIGCHLD | Child exit handling | Zombie reaping, status collection | Zombie process count |
| SIGFPE | Critical signal | Never blocked, immediate termination | Signal handling state |

### Socket Inheritance Test Scenarios

| Scenario | Configuration | Expected Result | Validation |
|----------|---------------|-----------------|------------|
| Single port inheritance | `--ports 8080` | Child inherits FD for port 8080 | Socket connection works |
| Multiple port inheritance | `--ports 8080,8081,8082` | Child inherits all FDs | All ports accessible |
| Zero-downtime restart | File change trigger | <1ms connection interruption | Connection timing |
| Invalid FD handling | Malformed FD environment | Graceful error handling | Error messages |
| SO_REUSEPORT validation | Multiple processes | Port sharing works | Concurrent connections |

### Performance Test Specifications

| Metric | Target | Measurement Method | Acceptance Criteria |
|--------|--------|-------------------|---------------------|
| Signal response time | <100ms | Signal send to process exit | Must be under threshold |
| Process restart time | <1s | File change to new process ready | Must be under threshold |
| Memory usage baseline | <10MB | Process memory monitoring | No memory leaks |
| CPU utilization | <5% idle | Process CPU monitoring | Minimal CPU usage |
| File descriptor leaks | 0 | FD count before/after tests | No FD leaks |

## Testing Strategy

### Unit Test Integration
- Existing unit tests remain unchanged
- Integration tests validate end-to-end behavior
- Unit tests focus on individual component logic

### Container Testing
```dockerfile
# Test container for CI/CD
FROM rust:1.75-slim
RUN apt-get update && apt-get install -y \
    procps \
    strace \
    lsof \
    netstat-nat
COPY . /app
WORKDIR /app
RUN cargo build --release
CMD ["cargo", "test", "--", "--test-threads=1"]
```

### Test Isolation
- Each test runs in isolated temporary directory
- Process cleanup ensures no interference
- Dedicated port ranges prevent conflicts
- Timeout mechanisms prevent hanging tests

### Test Data Validation
- Expected output files for complex scenarios
- Golden file comparison for regression testing
- Structured test result serialization
- Performance baseline tracking

## Alternatives Considered

### Alternative 1: Shell Script Testing Framework
**Pros:**
- Simpler implementation
- Easier to understand
- Lower maintenance overhead

**Cons:**
- Limited validation capabilities
- Poor CI/CD integration
- Difficult performance measurement
- No structured result reporting

**Decision:** Rejected due to limited validation capabilities and poor CI/CD integration.

### Alternative 2: External Testing Framework (e.g., pytest)
**Pros:**
- Rich testing features
- Good reporting capabilities
- Familiar to many developers

**Cons:**
- Additional language dependency
- Complex integration with Rust builds
- Process management complexity
- Performance measurement limitations

**Decision:** Rejected to maintain single-language codebase and better integration.

### Alternative 3: Minimal Integration Tests Only
**Pros:**
- Faster implementation
- Lower complexity
- Focused scope

**Cons:**
- Insufficient coverage for init system
- Missing performance validation
- No failure scenario testing
- Limited CI/CD value

**Decision:** Rejected due to insufficient coverage for critical init system requirements.

## Risks & Mitigation

### Risk 1: Test Flakiness in CI/CD
**Risk:** Signal timing and process lifecycle tests may be flaky in containerized CI environments.

**Mitigation:**
- Generous timeout values for CI environments
- Retry mechanisms for timing-sensitive tests
- Environment detection and adjustment
- Test isolation and cleanup mechanisms

### Risk 2: Performance Test Baseline Drift
**Risk:** Performance baselines may drift over time due to infrastructure changes.

**Mitigation:**
- Relative performance measurement (percentage changes)
- Baseline recalibration mechanisms
- Infrastructure-specific baselines
- Trend analysis rather than absolute values

### Risk 3: Platform-Specific Signal Behavior
**Risk:** Signal handling may behave differently across Linux distributions and container runtimes.

**Mitigation:**
- Platform detection and conditional test logic
- Container runtime specific test configurations
- Comprehensive documentation of platform differences
- CI testing across multiple environments

### Risk 4: Test Maintenance Overhead
**Risk:** Comprehensive test suite may require significant maintenance effort.

**Mitigation:**
- Well-structured test framework with reusable components
- Clear documentation and contribution guidelines
- Automated test generation where possible
- Regular test suite review and cleanup

## Implementation Dependencies

### Build Dependencies
```toml
[dev-dependencies]
# Existing
tempfile = "3.8"
chrono = "0.4"

# New dependencies for integration testing
nix = { version = "0.30.1", features = ["process", "term", "signal", "sys"] }
libc = "0.2"
sysinfo = "0.30"
procfs = "0.16"
criterion = { version = "0.5", features = ["html_reports"] }
serial_test = "3.0"
```

### System Dependencies
- `/proc` filesystem access for process monitoring
- `strace` for signal tracing validation (optional)
- `lsof` for file descriptor validation
- Container runtime for container-specific tests

### Test Infrastructure Requirements
- Minimum 2GB RAM for parallel test execution
- Root privileges for process group management tests
- Network access for socket inheritance tests
- Temporary directory with sufficient space (100MB)

## Migration Plan

### Phase 1: Framework Setup (No Breaking Changes)
- Add test infrastructure without modifying existing code
- Create parallel test structure
- Establish CI/CD pipeline updates

### Phase 2: Test Implementation (Additive Changes)
- Implement test scenarios incrementally
- Add performance baselines
- Enhance existing echo server for testing

### Phase 3: Integration (Configuration Changes)
- Update CI/CD workflows
- Add performance monitoring
- Update documentation

### Phase 4: Validation (Verification Phase)
- Run comprehensive test suite
- Validate performance baselines
- Confirm CI/CD integration

**Rollback Plan:**
- Tests are additive and can be disabled via feature flags
- Framework removal does not affect core functionality
- Performance baselines can be reset if needed

## Future Considerations

### Extensibility
The testing framework is designed to support future enhancements:

1. **Additional Signal Types**: Framework can easily add new signal test scenarios
2. **Container Runtime Testing**: Support for Docker, Podman, containerd-specific tests
3. **Performance Profiling**: Integration with profiling tools for detailed analysis
4. **Load Testing**: Stress testing capabilities for high-load scenarios
5. **Security Testing**: Permission and privilege escalation validation

### Long-term Vision
This framework establishes the foundation for:
- Automated performance regression detection
- Comprehensive init system compliance validation
- Container runtime certification testing
- Production readiness validation

### Integration Opportunities
- **Monitoring Integration**: Export test metrics to monitoring systems
- **Documentation Generation**: Auto-generate compatibility matrices
- **Benchmarking Database**: Historical performance tracking
- **Security Scanning**: Integration with security testing tools

## Acceptance Criteria

### Functional Requirements
- [ ] All signal types properly tested with expected behaviors
- [ ] Socket inheritance validated for single and multiple ports
- [ ] Zero-downtime restart measured and validated
- [ ] Process lifecycle management fully tested
- [ ] Failure scenarios comprehensively covered
- [ ] Performance baselines established and tracked

### Non-Functional Requirements
- [ ] Test suite runs in under 5 minutes
- [ ] Tests pass consistently in CI/CD environment
- [ ] Test coverage exceeds 90% for integration scenarios
- [ ] Performance tests detect 10% regressions
- [ ] Documentation complete and up-to-date

### Quality Requirements
- [ ] Test framework is maintainable and extensible
- [ ] Test results are clearly reported and actionable
- [ ] Test isolation prevents interference between tests
- [ ] Test cleanup prevents resource leaks
- [ ] Test framework integrates seamlessly with existing workflows

### Success Metrics
- **Signal Response Time**: Consistently under 100ms
- **Test Reliability**: >99% pass rate in CI/CD
- **Coverage**: >90% integration scenario coverage
- **Performance Baseline**: Established for all key metrics
- **Documentation**: Complete framework usage guide

This comprehensive integration testing framework will ensure scinit meets the rigorous requirements of a production-ready init system while providing confidence for future development and deployment.