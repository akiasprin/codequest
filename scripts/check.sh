#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo build --workspace --offline
python3 scripts/check_boundaries.py
go test providers/go-types/main.go providers/go-types/main_test.go
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
