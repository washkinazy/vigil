# Vigil

Vigil is a Rust session idle manager for Niri on Linux. It owns idle timers,
activity inhibitors, display power, locking, sleep coordination, and a local
control socket. Configuration is TOML. It combines idle scheduling and activity
inhibition in one daemon.

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
timers. Restart starts a fresh idle cycle and restores manual inhibitors.

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
- `pw-dump` (PipeWire tools) and `niri` for enabled adapters; MPRIS uses D-Bus directly.
- A working Type=notify locker unit and permission to suspend through logind.
- Rust with edition 2024 support, Cargo, `just`; rustfmt and Clippy for development.
  No desktop C development libraries are required.

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
can be applied with `vigilctl reload` or `systemctl --user reload vigil`. Invalid TOML leaves the current configuration
untouched. Successful reload resets idle timers, preserves manual inhibitors,
restores blanked displays using the previous command, and refreshes sensor policy.
Automatic actions wait for a fresh observation; stale pre-reload observations are
discarded. Sleep preparation uses the latest validated locker settings.

## Activity observation and session idle state

PipeWire activity uses one persistent `pw-dump --monitor` process. MPRIS uses
D-Bus player lifecycle and property-change signals. Optional fullscreen geometry
uses Niri's socket event stream. Subscriptions rebuild their state after a
backend disconnect or configuration reload; old-generation observations are
ignored. Window titles are discarded before storing Niri state.

Logind IdleHint export is disabled by default. Set `session.idle_hint_seconds`
to a positive threshold to let Vigil own it. The hint becomes true only after
observed inactivity with no inhibitors or degraded observation backends. Input,
resume, inhibition, disabling export, and orderly shutdown clear the hint.
`vigilctl status --json` exposes the last successfully exported `idle_hint`;
`null` means disabled, unavailable, or dry-run. An abrupt process termination
cannot perform cleanup. Only enable this when no other session component owns
IdleHint: it may activate logind's configured `IdleAction` independently of
Vigil's own suspend timer. Dry-run never writes IdleHint.

## Tray indicator

Run `vigilctl tray` or start `vigil-tray.service` with the user systemd manager.
For Niri startup:

```kdl
spawn-at-startup "systemctl" "--user" "start" "vigil-tray.service"
```

The tray service starts Vigil if needed. It requires a StatusNotifierItem host,
such as Waybar's `tray` module. The indicator uses a flat native D-Bus menu and embedded antialiased eye icons
at 16, 22, 24, 32, and 48 pixels; no GTK or icon-font dependency is required.

Left-click toggles automatic idle handling. Right-click provides **Enabled** and
**Disable for 30 / 60 / 90 minutes**. Enabled reflects the shared manual toggle;
application inhibitors and separate keep-awake requests still apply. A new timed
selection replaces the toggle's previous deadline, and expiry restores Enabled.
Enabling removes only that override. Explicit lock/suspend commands still work.

The indicator follows daemon status every two seconds and after each action.
Unavailable daemon connections show a warning and disable menu actions until
reconnection. It exits after executable replacement; the tray service restarts
it with the installed version. Stopping the indicator does not clear an override.
The service is packaged but is not automatically started or enabled.

Waybar users can remove `custom/vigil` and keep the standard `tray` module.
The optional `vigilctl waybar` integration remains supported.

## Controls and Waybar

```sh
vigilctl status
vigilctl status --json
vigilctl explain suspend           # blockers, timer state, and backend warnings
vigilctl keep-awake 1h --reason "Presentation"
vigilctl keep-awake 30m --scope suspend --reason "Download"
vigilctl keep-awake                 # until released
# Each keep-awake prints an ID, for example manual:1.
vigilctl release manual:1           # release only that request
vigilctl automatic                 # clear all manual requests
vigilctl toggle                     # toggle its own all-action request
vigilctl reload                     # validate and apply TOML without restarting
vigilctl lock
vigilctl suspend
vigilctl wake
vigilctl waybar --watch
```

Each `keep-awake` creates an independent inhibitor with its own scope, reason,
and optional expiration. Removing or expiring one preserves all other requests.
`toggle` atomically controls the dedicated `manual:toggle` inhibitor, so a
keybinding cannot accidentally clear a download or presentation request.
`automatic` explicitly clears all manual inhibitors; observed application
activity still applies. `status` shows request IDs and remaining times;
`status --json` exposes them in `manual_inhibitors` (a null `remaining_seconds`
means no expiration). Reasons must be nonblank, at most 256 bytes, and contain
no control characters. IDs are preserved across daemon restarts.

Manual requests, including the toggle, are saved in
`$XDG_STATE_HOME/vigil/manual.json` (normally `~/.local/state/vigil/manual.json`).
Timed requests retain their expiration; untimed requests survive restarts and
logins until released or cleared with `automatic`. State is written atomically
before a command reports success. A failed save leaves the previous requests
intact and reports an error. Invalid saved state prevents startup and is left
untouched for recovery; remove that file to explicitly discard saved requests.
Only manual requests are saved, never observed application activity.

`vigild --state-file PATH` selects another private state file. With a custom
`--socket`, the default state file is beside that socket with a `.manual.json`
extension. Dry-run instances neither read nor write saved state.

`vigilctl explain display|lock|suspend` reports the daemon's current automatic
policy decision, scoped inhibitors, manual expiration, idle timeout, release
grace or retry delay, and backend warnings. It does not perform an action or
predict future activity. Explicit `lock` and `suspend` still bypass idle policy.

The Waybar output is newline-delimited JSON with escaped tooltips containing a
compact list of activity reasons. Duplicate media sources are combined; detailed
scopes, backend warnings, and timers remain available through `vigilctl status`. Example:

```json
"custom/vigil": {
  "exec": "vigilctl waybar --watch",
  "return-type": "json",
  "restart-interval": 5,
  "format": "{text}",
  "hide-empty-text": false,
  "on-click": "vigilctl toggle",
  "tooltip": true
}
```

The button remains visible in automatic mode. Its CSS class is `automatic` when
there are no inhibitors, `manual` when the dedicated keep-awake toggle is on,
and `inhibited` when other requests or application activity inhibit actions.
Manual mode uses a coffee icon; other inhibition uses an eye, and automatic mode
uses an eye with a slash. The tooltip shows the current state and activity reasons. Turning the toggle off preserves other keep-awake requests and application
inhibitors. A disconnected daemon shows a warning with the `error` class.

Replace Waybar's `idle_inhibitor` module with `custom/vigil` to use this button.
`vigilctl toggle` is also suitable for a keybinding.

The watcher checks its executable identity on the existing two-second refresh
cycle. After an atomic package replacement it exits, allowing Waybar's
`restart-interval` to launch the updated client. The daemon and Waybar itself
are not restarted. Restart the Vigil service separately after package upgrades.

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
unit tests, debug builds, and documentation builds. `just smoke` runs an isolated
dry-run instance against the current session, including concurrent control calls
and valid/invalid configuration reloads. Tests cover overlapping
inhibitors, expiry/grace, action scopes, continuous input, reconnection, resume,
muted/dummy streams, fullscreen visibility, command timeouts, and lock-before-sleep
ordering. Live lock/suspend testing is intentionally separate from unit/dry-run
validation because it interrupts the desktop.

## Limits

- PipeWire stream state cannot establish audibility: a running pipeline can carry
  silence. Vigil does not sample sound.
- Logind inhibitors and Niri output geometry are reconciled every two seconds by
  default, controlled by `activity.poll_seconds`. These use D-Bus/socket requests;
  they do not launch polling subprocesses.
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
- Persisted expirations use wall-clock deadlines across restarts. System clock
  changes can affect restored durations. Running timers
  remain monotonic.
- Night-color control belongs to the desktop's Redshift/Gammastep configuration;
  Vigil does not change it.
- Niri is the supported compositor. Other compositors need compatible idle
  protocols and their own display commands; fullscreen observation is Niri-only.
  Vigil does not implement swayidle's shell-command language or arbitrary hooks.

`vigil-core` contains deterministic policy and TOML types; `vigild` owns I/O and
policy execution; `vigilctl` renders the daemon's decisions. Host package selection
belongs in the Fedora setup repository; user preferences/startup belong in chezmoi.
Licensed under the [MIT license](LICENSE).

Tray icon sources live in `assets/tray/*.svg`. To regenerate their embedded ARGB
images, run `python3 scripts/render-tray-icons.py` with Inkscape and ImageMagick
installed. These tools are not needed to build or run Vigil.
