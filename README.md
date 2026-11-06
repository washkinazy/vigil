# Vigil

Vigil is a Rust session idle manager for Niri on Linux. It owns idle timers,
activity inhibitors, display power, locking, sleep coordination, and a local
control socket. Configuration is TOML. It replaces swayidle and a separate audio
idle inhibitor in the tested Fedora/Niri session.

## Behavior

| Condition | Display blanking | Auto-lock | Auto-suspend |
| --- | --- | --- | --- |
| Idle, no inhibitors | 15 minutes | 20 minutes | 30 minutes |
| Active audio playback or recording | Allow | Allow | Inhibit |
| MPRIS player reporting Playing | Inhibit | Inhibit | Inhibit |
| Visible fullscreen geometry, if enabled | Inhibit | Inhibit | Inhibit |
| Compositor or logind idle inhibitor | Inhibit | Inhibit | Inhibit |
| Manual keep-awake | Selected scope, all actions by default | Selected scope | Selected scope |

Audio scopes and all timeouts are configurable. Set a timeout to zero to disable
that action. Expired/released inhibitors give affected actions at least 60 seconds
of grace, or their remaining ordinary idle timeout, whichever is longer. An action
runs once per idle cycle; failures retry after 30 seconds. Input and resume reset
timers. Restart starts a fresh idle cycle and clears manual overrides.

Vigil uses native `ext-idle-notify-v1` **version 2** notifications, including the
input-only variant, to distinguish physical inactivity from compositor inhibition.
No keyboard events, audio samples, or clipboard contents are read. Window IPC
includes titles, but Vigil neither retains them nor includes them in status.
PipeWire nodes must be running and connected through active links. Suspended,
unlinked, muted playback, monitor captures, and configured dummy streams are
excluded. This fixes the `speech-dispatcher-dummy` stream keeping an otherwise
inactive session awake simply because it remains uncorked.

Explicit `lock` and `suspend` commands bypass Vigil's activity policy. Suspend first
requires an active **Type=notify** locker service, whose READY notification must
mean the Wayland session lock has actually been acquired. The default is the
existing `wm-lock.service` running gtklock. A failed/unconfirmed lock prevents
Vigil's suspend command. Display and suspend commands use argument arrays, not a
shell. Their errors appear in status and the user journal.

External sleep requests are handled through logind's bounded delay inhibitor.
Vigil tries to lock before releasing it, keeping a half-second margin below
`InhibitDelayMaxUSec`. External sleep cannot be prevented indefinitely: if the
locker fails or exceeds that deadline, Vigil reports the failure and releases the
delay. Resume restores displays and restarts idle timers. logind session Lock
signals invoke the locker; Unlock signals never bypass locker authentication.

## Requirements

- Linux, systemd/logind, a user systemd manager, and a Wayland compositor exposing
  ext-idle-notify version 2. Niri is the tested compositor.
- `pw-dump` (PipeWire tools), `playerctl`, and `niri` for enabled adapters.
- A working Type=notify locker unit and permission to suspend through logind.
- Rust with edition 2024 support, Cargo, `just`; rustfmt and Clippy for development.
  Tested with Fedora Rust 1.99.1. No desktop C development libraries are required.

## Build and install

```sh
just check
just install
vigild --check-config
systemctl --user start vigil.service
vigilctl status
```

Builds use the committed Cargo.lock and `--offline`; on a fresh machine first use
`cargo fetch --locked` to populate the dependency cache. `just install` builds a
release, installs binaries to `~/.local/bin`, installs the user unit, and creates
`$XDG_CONFIG_HOME/vigil/config.toml` (normally `~/.config/vigil/config.toml`) only
when missing. It reloads systemd but does not start/restart services. Re-run it to
upgrade, then restart `vigil.service`. Installation never overwrites your TOML.

Start Vigil from Niri with:

```kdl
spawn-at-startup "systemctl" "--user" "start" "vigil.service"
```

Migrate only after a successful dry-run. Stop the previous idle manager and audio
inhibitor when starting live Vigil, and remove their session startup entries.
Leaving an old audio inhibitor running can cause Vigil to honor its logind block.
The service stops with `graphical-session.target`. Rollback: stop Vigil, restore
previous startup entries, and start the old idle services.

Use [config.example.toml](config.example.toml) as the configuration reference.
Unknown keys, invalid scopes, and unsafe empty commands fail validation. Changes
require `vigild --check-config` followed by `systemctl --user restart vigil`.

## Controls and Waybar

```sh
vigilctl status
vigilctl status --json
vigilctl keep-awake 1h
vigilctl keep-awake 30m --scope suspend
vigilctl keep-awake                 # until cleared or daemon restart
vigilctl automatic
vigilctl toggle                     # toggle manual all-action keep-awake
vigilctl lock
vigilctl suspend
vigilctl wake
vigilctl waybar --watch
```

The Waybar output is newline-delimited JSON with escaped tooltips containing the
actual inhibitor reasons, scopes, backend warnings, and timer states. Example:

```json
"custom/vigil": {
  "exec": "$HOME/.local/bin/vigilctl waybar --watch",
  "return-type": "json",
  "restart-interval": 5,
  "format": "{text}",
  "hide-empty-text": true,
  "tooltip": true
}
```

The indicator hides when automatic policy has no inhibitors. A disconnected
daemon shows a warning. Existing Waybar `idle_inhibitor` buttons work through
compositor inhibition; `vigilctl toggle` is also suitable for a keybinding.

The socket is `$XDG_RUNTIME_DIR/vigil/control.sock`, inside a private directory.
Both binaries accept `--socket PATH`; its existing parent must be private.
The local JSON request/response protocol is an internal interface, not a stable
public API yet. No network listener is created.

## Dry-run and validation

```sh
vigild --config config.example.toml --check-config
# Use a dedicated private parent directory for a second instance.
mkdir -m 700 "$XDG_RUNTIME_DIR/vigil-test"
vigild --config config.example.toml --socket "$XDG_RUNTIME_DIR/vigil-test/control.sock" --dry-run
vigilctl --socket "$XDG_RUNTIME_DIR/vigil-test/control.sock" status
```

Dry-run observes real desktop activity and records proposed actions but never
locks, blanks, suspends, or acquires a sleep delay inhibitor. It has an independent
control socket and does not replace running idle services.

`just check` runs formatting, Clippy with warnings denied, policy/adapter/runtime
unit tests, debug builds, and documentation builds. Tests cover overlapping
inhibitors, expiry/grace, action scopes, continuous input, reconnection, resume,
muted/dummy streams, fullscreen visibility, command timeouts, and lock-before-sleep
ordering. Live lock/suspend testing is intentionally separate from unit/dry-run
validation because it interrupts the desktop.

## Limits and next steps

- PipeWire, MPRIS, and optional Niri geometry use snapshots every two seconds by
  default (plus collection time). This starts short-lived CLI processes; native
  event subscriptions are a future optimization. A running pipeline can carry
  silence; Vigil does not sample sound to determine audibility.
- MPRIS does not reliably distinguish video from music. Any player reporting
  Playing blocks the configured media scope, even if its audio is muted.
- Niri currently lacks an authoritative fullscreen field in window IPC.
  `fullscreen = "geometry"` explicitly opts in to matching visible output-sized
  windows at the output origin (or the active window when IPC omits positions).
  It can misclassify borderless/maximized windows;
  the status reason says geometry. It is disabled in publishable defaults.
- Unavailable Wayland observation blocks all automatic actions; unavailable
  PipeWire observation blocks suspend. Other backend failures appear in health.
  Missing MPRIS/fullscreen/logind observations cannot reconstruct unknown activity.
- Manual overrides are process-local. Only one manual override exists; a new one
  replaces it. `toggle` is a convenience client read/modify operation.
- Optional Redshift automation is not implemented. It needs state-aware ownership
  and restoration so automation cannot undo a later manual decision.
- This implements the core idle lifecycle, not swayidle's shell-command language,
  arbitrary timeout hooks, or every compositor-specific feature. IdleHint export,
  additional compositor adapters, and configuration reload are not implemented.

`vigil-core` contains deterministic policy and TOML types; `vigild` owns I/O and
policy execution; `vigilctl` renders the daemon's decisions. Host package selection
belongs in the Fedora setup repository; user preferences/startup belong in chezmoi.
No license has been selected yet.
