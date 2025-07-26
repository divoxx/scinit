#!/bin/bash

echo "Testing SIGINT (Ctrl+C) handling"

# Start scinit in background
RUST_LOG=debug cargo run -- sleep 10 &
SCINIT_PID=$!

echo "Started scinit with PID: $SCINIT_PID"
sleep 2

echo "Sending SIGINT (Ctrl+C equivalent) to PID: $SCINIT_PID"
kill -INT $SCINIT_PID

echo "Waiting for scinit to handle SIGINT..."
wait $SCINIT_PID
echo "scinit exit status: $?"
