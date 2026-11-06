# Project scope

Vigil is a Rust session idle manager for Linux Wayland desktops. Target Fedora
and Niri first. The intended scope includes idle timers, activity-based and manual
inhibition, locking, display power, sleep/resume coordination, and status output.
Read README.md before changing behavior; distinguish implemented features from
proposals. Do not present scaffold commands as a working service.

- Keep service code here, system installation in the Fedora setup repository,
  and user preferences/session startup in chezmoi.
- Do not edit other repositories unless the task calls for integration changes.
- Preserve unrelated working-tree changes. Do not commit or publish unless asked.
- Do not select a license or claim compatibility with untested compositors.

# Architecture and implementation

- Keep deterministic policy and domain state in `vigil-core`.
- Put desktop I/O, event loops, scheduling, and action adapters in `vigild`.
- Keep `vigilctl` a control/status client, not a second policy engine.
- Inject elapsed time and observations into policy; use monotonic time for timers.
- Model display blanking, locking, and suspension independently.
- Track inhibitor identity, reason, action scope, and expiration. Multiple reasons
  must compose correctly; removing one must not release another's inhibition.
- Prefer protocol events to polling. Document any polling interval and its cost.
- Verify native protocol support and installed versions before adding integrations.
- Use typed argument lists for subprocesses, not interpolated shell commands.
- Keep errors contextual. Avoid panics in daemon paths and distinguish unavailable
  backends from inactive devices or applications.
- Add dependencies when needed, keep `Cargo.lock`, and document native libraries.
- Keep unsafe code forbidden in first-party crates unless a concrete integration
  requires a reviewed, narrowly scoped exception with documented invariants.

# Session behavior

- An open or uncorked stream is not sufficient evidence of active playback.
  Explicitly cover dummy streams, paused streams, muted playback, and disconnects.
- Fullscreen detection must use an authoritative compositor signal where possible.
  Geometry matching is an approximation, never an undisclosed equivalent.
- Inhibit automatic actions without silently suppressing explicit user commands.
- Confirm lock completion before suspension. Process launch success is not lock
  confirmation. Handle locker failure and timeout explicitly.
- Design external sleep preparation around bounded delay inhibitors; do not hang
  a system sleep request indefinitely waiting for a failed locker.
- Preserve and restore owned state, including optional Redshift changes, without
  overwriting subsequent user decisions. Do not implement automation as blind toggles.
- Define inhibitor-release grace behavior, restart recovery, and resume handling
  before enabling timers that perform real actions.
- The status API must expose actual policy decisions, active reasons, remaining
  timers, and degraded backend state. Do not show simulated success as live state.
- Avoid collecting clipboard contents, camera frames, audio samples, keystrokes,
  or unnecessary window titles. Escape external labels in UI output.

# Validation

Run checks appropriate to the change:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo build --workspace --locked --offline
cargo doc --workspace --no-deps --locked --offline
```

Report missing tools, unavailable dependencies, and environment-blocked checks.
Do not change dependencies merely to make an offline check succeed.

- Use injected clocks and fake adapters for policy and action tests.
- Cover overlapping inhibitors, expiration, activity resumption, stale events,
  backend disconnect/reconnect, lock failure, and suspend/resume ordering.
- Run CLI smoke checks for supported flags and explicit failure of unsupported
  commands. Do not add tests that merely restate constants or placeholder types.
- Use dry-run/read-only integration checks by default. Do not lock the user's
  session, turn off displays, suspend, alter Redshift, or replace running idle
  services during validation unless that integration exercise is authorized.
- Keep the user's existing idle services in place until a migration is requested
  and the replacement's required behavior has been validated.
