# Vigil

An idle manager with a caffeine switch for Niri. Vigil handles screen blanking,
locking, and suspend, and pauses automatic actions for application activity or
manual keep-awake.

## Setup

Requires Niri with `ext-idle-notify` v2, systemd/logind, PipeWire's `pw-dump`, and
a locker service using `Type=notify` to confirm the session is locked. The default
locker is `wm-lock.service`.

Build with Rust (edition 2024) and `just`:

```sh
cargo fetch --locked
just install
vigild --check-config
```

This installs to `~/.local/bin` and creates `~/.config/vigil/config.toml` if
missing. Edit it using [config.example.toml](config.example.toml) as a reference.
Installation preserves existing configuration and does not start services.
After upgrading, run `systemctl --user restart vigil.service`; restart the tray
with `systemctl --user try-restart vigil-tray.service` if it is running.

After testing with `just smoke`, stop your previous idle manager and audio
inhibitor, then start Vigil:

```sh
systemctl --user start vigil.service
```

For Niri startup, add:

```kdl
spawn-at-startup "systemctl" "--user" "start" "vigil.service"
```

## Use

```sh
vigilctl status
vigilctl toggle              # toggle keep-awake
vigilctl keep-awake 30m      # pause idle actions for 30 minutes
vigilctl automatic          # turn off keep-awake
vigilctl reload             # apply configuration changes
```

There is one keep-awake switch. A new duration replaces the previous one; toggling
it off or restarting the daemon clears it. Application activity still applies
when keep-awake is off. See `vigilctl --help` for all commands.

For a tray button, enable your panel's tray module and run:

```sh
systemctl --user start vigil-tray.service
```

To start it with Niri, add the same startup line above using `vigil-tray.service`.
Click to toggle; right-click for 30/60/90-minute pauses.
The tray shows Automatic, Keep awake, or Paused with the inhibition reason.
Alternatively, `vigilctl waybar --watch` provides Waybar JSON output.

## Idle behavior

Defaults: blank after 15 minutes, lock after 20, suspend after 30. Set an action's
timeout to `0` to disable it.

- PipeWire playback/recording and MPRIS playback pause all automatic actions.
- Fullscreen detection is opt-in: set `activity.fullscreen = "geometry"`. It
  approximates fullscreen from visible Niri window geometry and can mistake
  borderless windows for fullscreen.
- Manual keep-awake and compositor/logind idle inhibitors pause all actions by
  default. Audio and media scopes are configurable.

Input and resume reset idle timers. When the last inhibitor for an action clears,
that action starts a fresh idle timeout. Explicit lock/suspend commands bypass
activity inhibition; Vigil confirms the lock before initiating suspend.

Firefox’s known `speech-dispatcher-dummy` and `sd_dummy` streams are always
excluded; no configuration is needed. Paused, unlinked, and muted playback is
also ignored, but other running streams can still carry silence. MPRIS cannot
reliably distinguish music from video. Backend warnings appear in `vigilctl status`.

Upgrading from 0.5: saved manual requests are no longer restored; `release`,
`--scope`, and `--reason` are removed. Old `ignore_audio` and
`release_grace_seconds` settings are accepted but ignored. Existing audio scopes
are preserved; use the example config for the new defaults.

## Development

```sh
just check   # formatting, Clippy, tests, build, docs
just smoke   # isolated dry-run against the current desktop; no session actions
```

Rustfmt and Clippy are required for `just check`. Builds use the committed lockfile
and cached dependencies. `vigild --dry-run` observes activity without executing
session actions; use a separate `--socket` when another instance is running.

[MIT license](LICENSE).
