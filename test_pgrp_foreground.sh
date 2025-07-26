#!/bin/bash

# Start scinit and immediately check process groups
RUST_LOG=debug cargo run -- sleep 60 &
SCINIT_PID=$!
sleep 1
echo "Process groups:"
ps -eo pid,pgid,cmd | grep -E "(scinit|sleep)" | grep -v grep
