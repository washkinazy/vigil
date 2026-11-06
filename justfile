default:
    @just --list

build:
    cargo build --workspace --release --locked --offline

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked --offline -- -D warnings
    cargo test --workspace --locked --offline
    cargo build --workspace --locked --offline
    cargo doc --workspace --no-deps --locked --offline

install: build
    sh scripts/install.sh

status:
    systemctl --user status vigil.service
    vigilctl status

# Installation does not start or replace existing session services.
start:
    systemctl --user start vigil.service

stop:
    systemctl --user stop vigil.service

# Read-only integration against the current Wayland desktop; never performs actions.
smoke:
    cargo build --workspace --locked --offline
    python3 scripts/smoke.py
