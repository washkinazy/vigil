mod actions;
mod idle_hint;
mod logind;
mod manual_store;
mod observers;
mod sensors;
mod wayland;
use std::{
    collections::BTreeMap,
    os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
    path::PathBuf,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{mpsc, oneshot, watch},
    time::{Instant, timeout},
};
use vigil_core::{
    ACTIONS, Action, Config, Inhibitor, ManualInhibitors, Policy, Request, Response, Status,
};

pub enum Event {
    IdleHint(Option<bool>),
    InputIdle(bool),
    EffectiveIdle(bool),
    Health(String, Option<String>),
    Sleep(bool),
    LockRequested,
    Sensors(u64, Vec<Inhibitor>, BTreeMap<String, String>),
    Control(Request, oneshot::Sender<Response>),
}
struct Runtime {
    c: Config,
    config_file: PathBuf,
    config_tx: watch::Sender<(u64, Config)>,
    revision: u64,
    dry: bool,
    policy: Policy,
    sensors: Vec<Inhibitor>,
    health: BTreeMap<String, String>,
    sensor_health: BTreeMap<String, String>,
    input_idle: bool,
    effective_idle: bool,
    sleeping: bool,
    blanked: bool,
    manual: ManualInhibitors,
    manual_store: Option<manual_store::Store>,
    last_action: Option<String>,
    idle_hint: Option<bool>,
    idle_hint_tx: watch::Sender<Option<bool>>,
}
impl Runtime {
    fn inhibitors(&mut self, now: u64) -> Vec<Inhibitor> {
        self.manual.expire(now);
        let mut list = self.sensors.clone();
        if self.input_idle && !self.effective_idle {
            list.push(Inhibitor {
                id: "compositor".into(),
                reason: "Application compositor idle inhibition".into(),
                scope: ACTIONS.to_vec(),
            });
        }
        if self.health.contains_key("wayland") || self.health.contains_key("sensors") {
            list.push(Inhibitor {
                id: "degraded:wayland".into(),
                reason: "Idle or activity observation unavailable".into(),
                scope: ACTIONS.to_vec(),
            });
        }
        for manual in self.manual.status(now) {
            list.push(Inhibitor {
                id: manual.id,
                reason: manual.reason,
                scope: manual.scope,
            });
        }
        list
    }
    fn status(&mut self, now: u64) -> Status {
        let inhibitors = self.inhibitors(now);
        let timers = self.policy.timers(now, &self.c, &inhibitors);
        let mut health = self.health.clone();
        health.extend(self.sensor_health.clone());
        Status {
            idle_hint: self.idle_hint,
            dry_run: self.dry,
            idle_seconds: self.policy.elapsed(now),
            inhibitors,
            timers,
            health,
            last_action: self.last_action.clone(),
            manual_inhibitors: self.manual.status(now),
        }
    }
    async fn action(&mut self, a: Action) -> Result<(), String> {
        if self.dry {
            self.last_action = Some(format!("dry-run: would {a:?}"));
            return Ok(());
        }
        let c = &self.c.commands;
        let result = match a {
            Action::Display => actions::output(&c.display_off, 5).await.map(|_| ()),
            Action::Lock => actions::lock(c, c.lock_timeout_seconds).await,
            Action::Suspend => {
                actions::lock_then_suspend(
                    || actions::lock(c, c.lock_timeout_seconds),
                    || async { actions::output(&c.suspend, 10).await.map(|_| ()) },
                )
                .await
            }
        };
        self.last_action = Some(match &result {
            Ok(()) => format!("Completed {a:?}"),
            Err(e) => format!("Failed {a:?}: {e}"),
        });
        result
    }
    async fn wake(&mut self) -> Result<(), String> {
        if !self.dry {
            actions::output(&self.c.commands.display_on, 5).await?;
        }
        self.blanked = false;
        Ok(())
    }
}
fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("vigil/config.toml")
}
fn socket_path() -> Result<PathBuf, String> {
    Ok(
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is required")?)
            .join("vigil/control.sock"),
    )
}
async fn serve(path: PathBuf, tx: mpsc::UnboundedSender<Event>) -> Result<(), String> {
    let parent = path.parent().ok_or("socket needs a parent directory")?;
    if parent.exists()
        && std::fs::symlink_metadata(parent)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("socket directory must not be a symlink".into());
    }
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(parent)
            .map_err(|e| e.to_string())?;
    }
    if std::fs::metadata(parent)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o077
        != 0
    {
        return Err("socket parent must be a private directory (mode 0700)".into());
    }
    if path.exists() {
        if !std::fs::symlink_metadata(&path)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_socket()
        {
            return Err("control path is not a socket".into());
        }
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return Err("Vigil already owns this control socket".into());
        }
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader.take(16_384));
                let mut text = String::new();
                if !matches!(
                    timeout(Duration::from_secs(3), reader.read_line(&mut text)).await,
                    Ok(Ok(_))
                ) {
                    return;
                }
                let response = match serde_json::from_str::<Request>(&text) {
                    Ok(request) => {
                        let (reply, receive) = oneshot::channel();
                        if tx.send(Event::Control(request, reply)).is_err() {
                            return;
                        }
                        match timeout(Duration::from_secs(30), receive).await {
                            Ok(Ok(r)) => r,
                            _ => return,
                        }
                    }
                    Err(e) => Response {
                        ok: false,
                        message: format!("invalid request: {e}"),
                        status: None,
                    },
                };
                if let Ok(mut out) = serde_json::to_vec(&response) {
                    out.push(b'\n');
                    let _ = writer.write_all(&out).await;
                }
            });
        }
    });
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(e) = start().await {
        eprintln!("vigild: {e}");
        std::process::exit(1);
    }
}
async fn start() -> Result<(), String> {
    let mut dry = false;
    let mut check = false;
    let mut path = config_path();
    let mut socket = None;
    let mut state_file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "vigild [--config PATH] [--socket PATH] [--state-file PATH] [--dry-run] [--check-config]\nNative Wayland session idle manager. Configuration: TOML."
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("vigild {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--dry-run" => dry = true,
            "--state-file" => {
                state_file = Some(PathBuf::from(
                    args.next().ok_or("--state-file requires a path")?,
                ))
            }
            "--check-config" => check = true,
            "--config" => path = PathBuf::from(args.next().ok_or("--config requires a path")?),
            "--socket" => {
                socket = Some(PathBuf::from(
                    args.next().ok_or("--socket requires a path")?,
                ))
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let c = Config::parse(
        &std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?,
    )?;
    if check {
        println!("Configuration valid: {}", path.display());
        return Ok(());
    }
    let config_file = path;
    let manual_store = if dry {
        None
    } else {
        // Custom sockets get their own state, avoiding interference with the session daemon.
        let state_path =
            state_file.or_else(|| socket.as_ref().map(|p| p.with_extension("manual.json")));
        Some(manual_store::Store::open(match state_path {
            Some(p) => p,
            None => manual_store::default_path()?,
        })?)
    };
    let manual = match &manual_store {
        Some(store) => store.load(0)?,
        None => ManualInhibitors::default(),
    };

    let path = match socket {
        Some(p) => p,
        None => socket_path()?,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    serve(path.clone(), tx.clone()).await?;
    let clock = Instant::now();
    let (config_tx, config_rx) = watch::channel((0, c.clone()));
    let (idle_hint_tx, idle_hint_rx) = watch::channel(None);
    let idle_hint_task = idle_hint::start(tx.clone(), idle_hint_rx, dry);
    let mut r = Runtime {
        c: c.clone(),
        config_file,
        config_tx,
        revision: 0,
        dry,
        policy: Policy::new(0),
        sensors: vec![],
        health: BTreeMap::from([
            ("wayland".into(), "initializing".into()),
            ("logind".into(), "initializing".into()),
            ("sensors".into(), "initializing".into()),
        ]),
        sensor_health: BTreeMap::new(),
        input_idle: false,
        effective_idle: false,
        sleeping: false,
        blanked: false,
        manual,
        manual_store,
        last_action: None,
        idle_hint: None,
        idle_hint_tx,
    };
    wayland::start(tx.clone());
    logind::start(tx.clone(), config_rx.clone(), dry);
    observers::start(tx.clone(), config_rx);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    eprintln!(
        "Vigil ready: {}{}",
        path.display(),
        if dry { " (dry-run)" } else { "" }
    );
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                r.event(event, clock.elapsed().as_secs()).await;
            },
            _ = tick.tick() => r.tick(clock.elapsed().as_secs()).await,
        }
    }

    if r.blanked {
        let _ = r.wake().await;
    }
    drop(r.idle_hint_tx);
    let _ = idle_hint_task.await;
    let _ = std::fs::remove_file(path);
    Ok(())
}

impl Runtime {
    fn input(&mut self, idle: bool, now: u64) {
        self.input_idle = idle;
        if !idle {
            self.policy.activity(now);
        }
    }
    async fn restore_display(&mut self) {
        match self.wake().await {
            Ok(()) => {
                self.health.remove("display");
            }
            Err(e) => {
                self.health.insert("display".into(), e);
            }
        }
    }
    async fn control(&mut self, request: Request, now: u64) -> Result<String, String> {
        let changes_manual = matches!(
            request,
            Request::Toggle
                | Request::KeepAwake { .. }
                | Request::Release { .. }
                | Request::Automatic
        );
        let previous = changes_manual.then(|| self.manual.clone());
        let result = self.control_inner(request, now).await;
        if let Some(previous) = previous
            && result.is_ok()
        {
            if let Some(store) = &self.manual_store
                && let Err(error) = store.save(&self.manual, now)
            {
                self.manual = previous;
                self.health.insert("manual-state".into(), error.clone());
                return Err(error);
            }
            self.health.remove("manual-state");
        }
        result
    }
    fn explain(&mut self, action: Action, now: u64) -> String {
        let status = self.status(now);
        let Some(timer) = status.timers.iter().find(|t| t.action == action) else {
            return format!("{action:?}: timer unavailable");
        };
        let state = if !timer.enabled {
            "disabled".to_string()
        } else if self.sleeping {
            "paused during sleep preparation".into()
        } else if timer.blocked {
            "inhibited".into()
        } else if timer.completed {
            "completed for this idle cycle".into()
        } else if !self.input_idle {
            "waiting for inactivity".into()
        } else if timer.remaining_seconds == 0 {
            "due on the next scheduler tick".into()
        } else {
            format!(
                "eligible in {}s ({})",
                timer.remaining_seconds,
                match timer.delay {
                    Some(vigil_core::TimerDelay::ReleaseGrace) => "release grace",
                    Some(vigil_core::TimerDelay::Retry) => "retry delay",
                    None => "idle timeout",
                }
            )
        };
        let mut lines = vec![format!(
            "{action:?}: {state}{}",
            if self.dry { " (dry-run)" } else { "" }
        )];
        for inhibitor in status
            .inhibitors
            .iter()
            .filter(|i| i.scope.contains(&action))
        {
            let expiry = status
                .manual_inhibitors
                .iter()
                .find(|m| m.id == inhibitor.id)
                .and_then(|m| m.remaining_seconds)
                .map_or(String::new(), |n| format!(" ({n}s remaining)"));
            lines.push(format!(
                "  {}: {}{expiry}",
                inhibitor.id,
                inhibitor.reason.escape_default()
            ));
        }
        for (backend, reason) in status.health {
            lines.push(format!(
                "  Warning: {}: {}",
                backend.escape_default(),
                reason.escape_default()
            ));
        }
        if let Some(last) = status.last_action {
            lines.push(format!("  Last action: {}", last.escape_default()));
        }
        lines.join("\n")
    }
    async fn control_inner(&mut self, request: Request, now: u64) -> Result<String, String> {
        match request {
            Request::Status => Ok("status".into()),
            Request::Explain { action } => Ok(self.explain(action, now)),
            Request::Reload => {
                let text = std::fs::read_to_string(&self.config_file).map_err(|e| e.to_string())?;
                let next = Config::parse(&text)?;
                if self.blanked {
                    self.wake().await?;
                }
                self.c = next;
                self.revision += 1;
                self.policy.activity(now);
                self.sensors.clear();
                self.sensor_health.clear();
                self.health.insert(
                    "sensors".into(),
                    "Refreshing after configuration reload".into(),
                );
                self.config_tx.send_replace((self.revision, self.c.clone()));
                Ok("Configuration reloaded; idle timers reset, manual inhibitors preserved".into())
            }
            Request::SetToggle { seconds } => {
                self.manual.set_toggle(now, seconds)?;
                Ok("Manual keep-awake enabled".into())
            }
            Request::Toggle => {
                self.manual.expire(now);
                if self.manual.toggle() {
                    Ok("Toggle keep-awake enabled (manual:toggle)".into())
                } else {
                    Ok("Toggle keep-awake released; other inhibitors preserved".into())
                }
            }
            Request::KeepAwake {
                seconds,
                scope,
                reason,
            } => {
                self.manual.expire(now);
                self.manual.add(now, seconds, scope, reason)
            }
            Request::Release { id } => {
                self.manual.release(&id, now)?;
                Ok(format!("Released {id}"))
            }
            Request::Automatic => {
                self.manual.clear();
                Ok("All manual inhibitors cleared".into())
            }
            Request::Lock => self
                .action(Action::Lock)
                .await
                .map(|_| "Lock requested".into()),
            Request::Suspend => self
                .action(Action::Suspend)
                .await
                .map(|_| "Suspend requested".into()),
            Request::Wake => {
                self.policy.activity(now);
                self.wake().await.map(|_| "Displays restored".into())
            }
        }
    }
    async fn event(&mut self, event: Event, now: u64) {
        match event {
            Event::IdleHint(value) => self.idle_hint = value,
            Event::InputIdle(idle) => {
                self.input(idle, now);
                if !idle && self.blanked {
                    self.restore_display().await;
                }
            }
            Event::EffectiveIdle(idle) => self.effective_idle = idle,
            Event::Health(name, error) => {
                if let Some(e) = error {
                    eprintln!("{name}: {e}");
                    self.health.insert(name, e);
                } else {
                    self.health.remove(&name);
                }
            }
            Event::Sensors(revision, list, health) => {
                if revision != self.revision {
                    return;
                }
                self.health.remove("sensors");
                self.sensors = list;
                self.sensor_health = health;
            }
            Event::Sleep(preparing) => {
                self.sleeping = preparing;
                if !preparing {
                    self.policy.activity(now);
                    self.restore_display().await;
                }
            }
            Event::LockRequested => {
                let _ = self.action(Action::Lock).await;
            }
            Event::Control(request, reply) => {
                let result = self.control(request, now).await;
                let status = self.status(now);
                let _ = reply.send(Response {
                    ok: result.is_ok(),
                    message: result.unwrap_or_else(|e| e),
                    status: Some(status),
                });
            }
        }
    }
    fn update_idle_hint(&mut self, now: u64) {
        let inhibited = !self.inhibitors(now).is_empty();
        let observed = self.input_idle
            && !self.sleeping
            && !self.health.contains_key("wayland")
            && !self.health.contains_key("sensors")
            && !self.health.contains_key("logind")
            && self.sensor_health.is_empty();
        let next = self
            .c
            .session
            .idle_hint(self.policy.elapsed(now), observed, inhibited);
        self.idle_hint_tx.send_if_modified(|value| {
            if *value == next {
                false
            } else {
                *value = next;
                true
            }
        });
    }
    async fn tick(&mut self, now: u64) {
        // Resumed means input is still active until the next one-second idle event.
        if !self.input_idle {
            self.policy.activity(now);
        }
        self.update_idle_hint(now);
        let status = self.status(now);
        if self.sleeping {
            return;
        }
        if self.blanked
            && status
                .timers
                .iter()
                .any(|t| t.action == Action::Display && t.blocked)
        {
            self.restore_display().await;
        }
        for t in status.timers {
            if t.enabled && !t.blocked && !t.completed && t.remaining_seconds == 0 {
                match self.action(t.action).await {
                    Ok(()) => {
                        self.policy.complete(t.action);
                        if t.action == Action::Display {
                            self.blanked = true;
                        }
                    }
                    Err(e) => {
                        eprintln!("action failed: {e}");
                        self.policy.failed(t.action, now, &self.c);
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> Runtime {
        Runtime {
            c: Config::default(),
            config_file: PathBuf::new(),
            config_tx: watch::channel((0, Config::default())).0,
            revision: 0,
            dry: true,
            policy: Policy::new(0),
            sensors: vec![],
            health: BTreeMap::new(),
            sensor_health: BTreeMap::new(),
            input_idle: false,
            effective_idle: true,
            sleeping: false,
            blanked: false,
            manual: ManualInhibitors::default(),
            manual_store: None,
            last_action: None,
            idle_hint: None,
            idle_hint_tx: watch::channel(None).0,
        }
    }
    #[tokio::test]
    async fn explain_reports_scopes_grace_retry_and_health_without_actions() {
        let mut r = runtime();
        r.input_idle = true;
        r.manual
            .add(0, Some(5000), vec![Action::Suspend], "Backup".into())
            .unwrap();
        let message = r
            .control(
                Request::Explain {
                    action: Action::Suspend,
                },
                2000,
            )
            .await
            .unwrap();
        assert!(message.contains("Suspend: inhibited"));
        assert!(message.contains("Backup (3000s remaining)"));
        assert!(!r.explain(Action::Display, 2000).contains("Backup"));
        r.manual.clear();
        assert!(
            r.explain(Action::Suspend, 2001)
                .contains("60s (release grace)")
        );
        r.policy.failed(Action::Display, 2001, &r.c);
        assert!(
            r.explain(Action::Display, 2001)
                .contains("30s (retry delay)")
        );
        r.c.timeouts.lock_seconds = 0;
        assert!(r.explain(Action::Lock, 2001).contains("disabled"));
        r.health.insert("test-backend".into(), "unavailable".into());
        assert!(
            r.explain(Action::Lock, 2001)
                .contains("Warning: test-backend: unavailable")
        );
        r.policy.complete(Action::Display);
        assert!(r.explain(Action::Display, 2100).contains("completed"));
        r.sleeping = true;
        assert!(
            r.explain(Action::Display, 2100)
                .contains("sleep preparation")
        );
        assert!(r.last_action.is_none());
    }
    #[tokio::test]
    async fn persistent_commands_restore_and_failed_writes_roll_back() {
        let dir = std::env::temp_dir().join(format!("vigil-runtime-state-{}", std::process::id()));
        let path = dir.join("manual.json");
        let mut r = runtime();
        // Fake action adapters: exercise real state I/O without desktop actions.
        r.manual_store = Some(manual_store::Store::open(path.clone()).unwrap());
        let id = r
            .control(
                Request::KeepAwake {
                    seconds: None,
                    scope: ACTIONS.to_vec(),
                    reason: "Persistent".into(),
                },
                0,
            )
            .await
            .unwrap();
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::create_dir(&temp).unwrap();
        assert!(r.control(Request::Automatic, 1).await.is_err());
        assert_eq!(r.status(1).manual_inhibitors[0].id, id);
        assert!(r.health.contains_key("manual-state"));
        std::fs::remove_dir(&temp).unwrap();
        drop(r);
        let mut r = runtime();
        let store = manual_store::Store::open(path.clone()).unwrap();
        r.manual = store.load(0).unwrap();
        r.manual_store = Some(store);
        assert_eq!(r.status(0).manual_inhibitors[0].id, id);
        r.control(Request::Release { id }, 0).await.unwrap();
        assert!(
            r.manual_store
                .as_ref()
                .unwrap()
                .load(0)
                .unwrap()
                .status(0)
                .is_empty()
        );
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn continuous_input_never_runs_idle_actions() {
        let mut r = runtime();
        r.tick(10000).await;
        assert!(r.last_action.is_none());
        r.input(true, 10001);
        r.tick(10900).await;
        assert!(r.blanked);
        r.event(Event::InputIdle(false), 10901).await;
        assert!(!r.blanked);
        assert_eq!(r.status(10901).idle_seconds, 0);
    }
    #[tokio::test]
    async fn manual_expiry_composes_with_audio_and_grace() {
        let mut r = runtime();
        r.input_idle = true;
        r.control(
            Request::KeepAwake {
                reason: "Presentation".into(),
                seconds: Some(10),
                scope: ACTIONS.to_vec(),
            },
            2000,
        )
        .await
        .unwrap();
        r.sensors.push(Inhibitor {
            id: "audio".into(),
            reason: "audio".into(),
            scope: vec![Action::Suspend],
        });
        assert!(r.status(2001).timers.iter().all(|t| t.blocked));
        let s = r.status(2010);
        assert_eq!(s.inhibitors.len(), 1);
        assert_eq!(s.timers[0].remaining_seconds, 60);
        assert!(s.timers[2].blocked);
    }
    #[tokio::test]
    async fn independent_release_and_expiry_preserve_scopes_and_grace() {
        let mut r = runtime();
        r.input_idle = true;
        let download = r
            .control(
                Request::KeepAwake {
                    seconds: None,
                    scope: vec![Action::Suspend],
                    reason: "Download".into(),
                },
                2000,
            )
            .await
            .unwrap();
        let presentation = r
            .control(
                Request::KeepAwake {
                    seconds: Some(10),
                    scope: ACTIONS.to_vec(),
                    reason: "Presentation".into(),
                },
                2000,
            )
            .await
            .unwrap();
        assert_ne!(download, presentation);
        assert!(r.status(2001).timers.iter().all(|t| t.blocked));
        let s = r.status(2010);
        assert_eq!(s.manual_inhibitors.len(), 1);
        assert_eq!(s.manual_inhibitors[0].id, download);
        assert_eq!(s.timers[0].remaining_seconds, 60);
        assert!(s.timers[2].blocked);
        assert!(
            r.control(Request::Release { id: presentation }, 2010)
                .await
                .is_err()
        );
        assert_eq!(r.status(2010).manual_inhibitors.len(), 1);
        r.control(Request::Release { id: download }, 2011)
            .await
            .unwrap();
        let s = r.status(2011);
        assert!(s.manual_inhibitors.is_empty());
        assert!(!s.timers[2].blocked);
        assert_eq!(s.timers[2].remaining_seconds, 60);
    }
    #[tokio::test]
    async fn targeted_release_and_clear_preserve_sensor_inhibitors() {
        let mut r = runtime();
        r.sensors.push(Inhibitor {
            id: "audio".into(),
            reason: "Audio".into(),
            scope: vec![Action::Suspend],
        });
        let first = r
            .manual
            .add(0, None, ACTIONS.to_vec(), "First".into())
            .unwrap();
        let second = r
            .manual
            .add(0, None, ACTIONS.to_vec(), "Second".into())
            .unwrap();
        r.control(Request::Release { id: first.clone() }, 1)
            .await
            .unwrap();
        assert_eq!(r.status(1).manual_inhibitors[0].id, second);
        assert!(r.status(1).timers.iter().all(|t| t.blocked));
        assert!(r.control(Request::Release { id: first }, 1).await.is_err());
        assert!(
            r.control(Request::Release { id: "audio".into() }, 1)
                .await
                .is_err()
        );
        r.control(Request::Automatic, 1).await.unwrap();
        let s = r.status(1);
        assert!(s.manual_inhibitors.is_empty());
        assert_eq!(s.inhibitors.len(), 1);
        assert_eq!(s.inhibitors[0].id, "audio");
    }
    #[tokio::test]
    async fn disconnect_blocks_and_reconnect_resets_idle_baseline() {
        let mut r = runtime();
        r.input_idle = true;
        r.event(
            Event::Health("wayland".into(), Some("disconnected".into())),
            2000,
        )
        .await;
        r.tick(2000).await;
        assert!(r.last_action.is_none());
        r.event(Event::InputIdle(false), 2001).await;
        r.event(Event::Health("wayland".into(), None), 2001).await;
        r.tick(2001).await;
        assert!(r.last_action.is_none());
        assert_eq!(r.status(2001).timers[0].remaining_seconds, 900);
    }
    #[tokio::test]
    async fn resume_and_explicit_commands_ignore_inhibitors() {
        let mut r = runtime();
        r.input_idle = true;
        r.control(
            Request::KeepAwake {
                reason: "Download".into(),
                seconds: None,
                scope: ACTIONS.to_vec(),
            },
            0,
        )
        .await
        .unwrap();
        r.control(Request::Suspend, 2000).await.unwrap();
        assert_eq!(r.last_action.as_deref(), Some("dry-run: would Suspend"));
        r.event(Event::Sleep(true), 2001).await;
        r.event(Event::Sleep(false), 3000).await;
        assert!(!r.sleeping);
        assert_eq!(r.status(3000).idle_seconds, 0);
        assert_eq!(r.inhibitors(3000).len(), 1);
    }
    #[tokio::test]
    async fn toggle_preserves_independent_requests() {
        let mut r = runtime();
        r.manual
            .add(0, None, vec![Action::Suspend], "Download".into())
            .unwrap();
        r.control(Request::Toggle, 3).await.unwrap();
        assert_eq!(r.status(3).manual_inhibitors.len(), 2);
        r.control(Request::Toggle, 3).await.unwrap();
        let status = r.status(3);
        assert_eq!(status.manual_inhibitors.len(), 1);
        assert_eq!(status.manual_inhibitors[0].reason, "Download");
    }
    #[tokio::test]
    async fn reload_rejects_invalid_config_and_preserves_manual_override() {
        let mut r = runtime();
        let path =
            std::env::temp_dir().join(format!("vigil-reload-test-{}.toml", std::process::id()));
        r.config_file = path.clone();
        r.manual
            .add(0, Some(5000), vec![Action::Suspend], "Download".into())
            .unwrap();
        std::fs::write(&path, "[timeouts]\nlock_seconds = 42").unwrap();
        r.control(Request::Reload, 2000).await.unwrap();
        assert_eq!(r.c.timeouts.lock_seconds, 42);
        assert_eq!(r.policy.elapsed(2000), 0);
        assert_eq!(
            r.status(2000).manual_inhibitors[0].remaining_seconds,
            Some(3000)
        );
        assert!(r.status(2000).timers.iter().all(|t| t.blocked));
        r.event(Event::Sensors(0, vec![], BTreeMap::new()), 2001)
            .await;
        assert!(r.health.contains_key("sensors"));
        r.event(Event::Sensors(1, vec![], BTreeMap::new()), 2002)
            .await;
        assert!(!r.health.contains_key("sensors"));
        std::fs::write(&path, "invalid = true").unwrap();
        assert!(r.control(Request::Reload, 2003).await.is_err());
        assert_eq!(r.revision, 1);
        assert_eq!(r.c.timeouts.lock_seconds, 42);
        std::fs::remove_file(path).unwrap();
    }
}
