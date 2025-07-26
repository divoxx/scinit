#!/bin/bash

echo "Testing Scenario B: Signal forwarding with graceful shutdown"

# Start scinit in background
RUST_LOG=info cargo run -- sleep 30 &
SCINIT_PID=$!

echo "Started scinit with PID: $SCINIT_PID"
echo "Waiting 3 seconds, then sending SIGTERM..."

sleep 3

echo "Sending SIGTERM to scinit PID: $SCINIT_PID"
echo "Before signal - process groups:"
ps -eo pid,pgid,ppid,cmd | grep -E "(sleep|scinit|$SCINIT_PID)" | grep -v grep

kill -TERM $SCINIT_PID

echo "Waiting for graceful shutdown..."
wait $SCINIT_PID
echo "scinit exited with status: $?"