# Known Issues in scinit

This document tracks known bugs and issues in scinit that have been identified through testing.

## Signal Handling Issues

### SIGUSR1, SIGUSR2, and SIGHUP Cause Unexpected Exit

**Issue:** scinit incorrectly exits when receiving forwarding signals (SIGUSR1, SIGUSR2, SIGHUP) instead of forwarding them to child processes and continuing to run.

**Expected Behavior:** According to `src/signals.rs:222-228`, these signals should be forwarded to the child process and scinit should continue running (`SignalAction::Continue`).

**Current Behavior:** scinit exits when receiving these signals.

**Impact:** 
- Signal forwarding functionality is broken
- Applications expecting to receive forwarded signals from scinit will not work correctly
- Container orchestrators that rely on signal forwarding may not function properly

**Reproduction:**
```bash
# Start scinit with a long-running process
./target/debug/scinit sleep 30 &
SCINIT_PID=$!

# Send SIGUSR1 - scinit should continue running but currently exits
kill -USR1 $SCINIT_PID

# Check if still running
kill -0 $SCINIT_PID 2>/dev/null && echo "Still running" || echo "Exited (BUG)"
```

**Status:** Identified via integration tests in `tests/integration/scenarios/signal_handling_tests.rs`

**Priority:** High - This breaks core init system functionality

---

## Test Coverage

The integration test framework in `tests/integration/` successfully identifies this issue through systematic signal handling tests, demonstrating the value of comprehensive testing for init systems.