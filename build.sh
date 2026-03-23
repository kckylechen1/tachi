#!/bin/bash
# Build and install the memory server

cd /Users/kckylechen/Desktop/Sigil

# Build the release binary
cargo build --release -p memory-server

# Check if build succeeded
if [ $? -eq 0 ]; then
    echo "Build successful!"
    echo "Binary location: ./target/release/memory-server"
    ls -lh ./target/release/memory-server
else
    echo "Build failed!"
    exit 1
fi