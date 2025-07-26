#!/bin/bash
set -x

echo "Testing signal handling with debug output"

# Start scinit 
RUST_LOG=debug cargo run -- sleep 60 &
SCINIT_PID=$!

echo "scinit PID: $SCINIT_PID"
sleep 2

echo "Process groups before signal:"
ps -eo pid,pgid,ppid,cmd | grep -E "(sleep|scinit)" | grep -v grep

echo "Sending SIGTERM to scinit PID $SCINIT_PID"
kill -TERM $SCINIT_PID

echo "Waiting for scinit to handle signal..."
wait $SCINIT_PID
echo "scinit exit status: $?"