#!/bin/sh
# Builds the bet CLI for Apple Silicon Macs into static/, where the project page links to it.
set -e
cd "$(dirname "$0")"
cargo build --release --bin bet
mkdir -p static
cp target/release/bet static/bet-macos-arm64
strip static/bet-macos-arm64
codesign --force --sign - static/bet-macos-arm64
shasum -a 256 static/bet-macos-arm64
