#!/bin/bash

echo "Testing terminal Ctrl+C behavior - this needs to be run interactively"
echo "Run this script, then press Ctrl+C when scinit starts"
echo ""

# Show process tree before
echo "=== Process tree before scinit ==="
ps -eo pid,pgid,ppid,cmd | head -1
ps -eo pid,pgid,ppid,cmd | grep -E "(bash|$$)" | grep -v grep

echo ""
echo "Starting scinit with 'sleep 30' - press Ctrl+C after it starts"
echo "Expected: Both sleep and scinit should exit"
echo ""

RUST_LOG=debug cargo run -- sleep 30

echo ""
echo "=== Process tree after scinit exit ==="
ps -eo pid,pgid,ppid,cmd | head -1  
ps -eo pid,pgid,ppid,cmd | grep -E "(sleep|scinit)" | grep -v grep || echo "No sleep/scinit processes found (good)"