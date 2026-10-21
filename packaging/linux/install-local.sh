#!/bin/sh
# Installs ORBIT for the current user (~/.local). No root required, nothing outside $HOME.
set -eu
cd "$(dirname "$0")/../.."
cargo build --release --locked -p orbit-cli -p orbit-app
BIN="${HOME}/.local/bin"
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$BIN" "$APPS"
install -m 0755 target/release/orbit "$BIN/orbit"
install -m 0755 target/release/orbit-app "$BIN/orbit-app"
install -m 0644 packaging/linux/io.github.pietrolama.Orbit.desktop "$APPS/"
echo "Installato in $BIN (assicurati che sia nel PATH). Avvia con: orbit ui"
