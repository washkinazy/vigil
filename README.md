# Vigil

Vigil is a planned session idle manager for Linux Wayland desktops. It will
coordinate activity detection, idle timers, inhibition, locking, display power,
and suspend, and explain its decisions through a CLI and a Waybar indicator.

The first supported environment will be Fedora with Niri. Compositor-specific
features will live behind adapters so other Wayland compositors can follow.
Vigil is intended to replace swayidle/hypridle and a separate audio idle inhibitor
once the relevant behavior has been implemented and validated.

## Current status

This repository is a Rust scaffold, not a functioning idle manager.

Implemented:

- A Cargo workspace with `vigild`, `vigilctl`, and `vigil-core`.
- Help and version output for both binaries.
- Shared names for the three automatic idle actions.

There are no desktop connections, timers, configuration parser, control socket,
service installation, or real lock/suspend actions yet. Running `vigild` without
an informational flag exits with an error. Control commands such as `status` are
not implemented and also fail explicitly. Nothing here replaces existing session
services automatically.

## Why

An open audio stream is not necessarily playback. A Speech Dispatcher dummy
stream can remain uncorked while its PipeWire device is suspended, causing a
simple audio inhibitor to keep the session awake indefinitely.

Vigil should make decisions from observable activity, distinguish unavailable
information from inactivity, and report which activity blocks which action.

## Intended behavior

The following is the initial design proposal, not implemented behavior or a
finalized default policy. Blank displays, auto-lock, and auto-suspend are separate
decisions; an inhibitor can block some actions while allowing others.

| Condition | Display blanking | Auto-lock | Auto-suspend |
| --- | --- | --- | --- |
| No qualifying activity | Follow timeout | Follow timeout | Follow timeout |
| Audio playback | Allow | Allow | Inhibit |
| Video playback or visible fullscreen window | Inhibit | Inhibit | Inhibit |
| Call or recording | Configurable | Configurable | Inhibit |
| Manual keep-awake | Explicit selected scope | Explicit selected scope | Inhibit |

Planned inputs:

- Wayland idle notifications, without recording raw keyboard or pointer events.
- PipeWire node and link activity, with configurable exclusions for dummy streams.
- MPRIS playback state for participating media players, including muted video.
- Supported application inhibition requests, mapped to their documented scope.
- Visible fullscreen state when the compositor exposes a reliable signal.
- Manual keep-awake requests, optionally with an expiration time.

A running media pipeline can carry silence. MPRIS coverage varies by application
and does not universally distinguish video from audio. Neither signal alone is
proof that a person is watching or listening.

The Niri version inspected during planning does not expose an explicit fullscreen
flag in its window IPC. Do not equate a maximized or output-sized window with
fullscreen without a documented, optional approximation. Fullscreen-dependent
features remain blocked on a reliable adapter or an explicit fallback decision.

Planned actions and guarantees:

- Use the existing locker, compositor display controls, and logind sleep API.
- Confirm the session is locked before requesting suspend. A failed or unconfirmed
  lock must not silently proceed to suspend.
- Respect explicit user actions separately from automatic idle policy.
- Coordinate external sleep requests with a bounded logind delay inhibitor.
- Release owned inhibitors on shutdown and handle disconnects and resume explicitly.
- Avoid immediate lock/suspend surprises when an inhibitor expires or disappears;
  define and test a grace/reset policy before enabling real timers.
- Optionally disable Redshift during qualifying fullscreen activity and restore
  its prior state. This needs state-aware integration that respects manual changes;
  blindly sending toggle signals is insufficient.

## Architecture

```text
Wayland / PipeWire / MPRIS / compositor / logind adapters
                         |
                   activity events
                         |
                   vigil-core policy
                         |
                 decisions and reasons
                         |
                vigild action adapters
                         |
             status API -> vigilctl -> Waybar
```

- `crates/vigil-core`: deterministic policy, state transitions, and shared domain
  types. No desktop I/O or wall-clock reads.
- `crates/vigild`: future event loop, platform adapters, timer scheduling, action
  execution, and a user-session control/status endpoint.
- `crates/vigilctl`: future user controls and machine-readable status output.

The daemon owns the authoritative inhibition reasons. Waybar should consume that
state rather than independently reconstructing why the session is awake.

Candidate integrations include `wayland-client`/`wayland-protocols` for Wayland,
`pipewire` for graph activity, and `zbus` for D-Bus and logind. These libraries are
not dependencies yet. Verify protocol coverage, file-descriptor ownership,
threading, reconnect behavior, and system-library requirements in small adapter
prototypes before selecting versions or an async runtime.

## Development

Use a Rust toolchain supporting edition 2024, with Cargo, rustfmt, and Clippy.
No external crates or desktop development libraries are required by the scaffold.
The initial scaffold was checked with Fedora's Rust 1.99.1; an MSRV has not yet
been established.

```sh
cargo build --workspace --locked --offline
cargo run --locked --offline -p vigild -- --help
cargo run --locked --offline -p vigilctl -- --help
```

Validation:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo doc --workspace --no-deps --locked --offline
```

The scaffold currently has no behavioral tests. Add policy tests with injected
clocks and fake adapters when implementing the first state transitions. Offline
commands will require cached dependencies once external crates are introduced.
Commit `Cargo.lock` because this workspace produces applications.

## Planned command interface

Only `--help` and `--version` work today. The following commands are proposals:

```text
vigild                    Run the session service
vigilctl status           Explain activity, timers, inhibitors, and backend health
vigilctl status --json    Produce machine-readable state for integrations
vigilctl keep-awake 1h    Request a bounded manual override
vigilctl automatic        Clear the manual override; keep activity-based policy
vigilctl lock             Explicitly request session locking
```

Exact syntax, override scope, configuration format, and transport are undecided.

## Milestones

1. Scaffold the workspace and document intended behavior.
2. Specify policy defaults, timer transitions, grace periods, and failure behavior;
   implement these with deterministic tests.
3. Add read-only desktop adapters and a dry-run daemon that explains decisions.
4. Add the control/status interface and Waybar integration.
5. Implement lock acknowledgement, display actions, inhibitors, and sleep/resume
   coordination; validate in an explicitly authorized integration session.
6. Package a user service and migrate one session from existing idle services.
7. Add fullscreen/Redshift behavior and other compositor adapters after capability
   checks and state-restoration tests.

System package installation belongs in the Fedora setup repository. User
preferences and session startup belong in the chezmoi dotfiles repository.
This repository owns the service implementation and eventual packaging assets.

No license has been selected yet.
