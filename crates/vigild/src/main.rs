mod actions;
mod logind;
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
use vigil_core::{ACTIONS, Action, Config, Inhibitor, Policy, Request, Response, Status};

pub enum Event {
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
    manual: Option<(Option<u64>, Vec<Action>)>,
    last_action: Option<String>,
}
impl Runtime {
    fn inhibitors(&mut self, now: u64) -> Vec<Inhibitor> {
        if self
            .manual
            .as_ref()
            .and_then(|m| m.0)
            .is_some_and(|until| now >= until)
        {
            self.manual = None;
        }
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
        if let Some((_, scope)) = &self.manual {
            list.push(Inhibitor {
                id: "manual".into(),
                reason: "Manual keep-awake".into(),
                scope: scope.clone(),
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
            dry_run: self.dry,
            idle_seconds: self.policy.elapsed(now),
            inhibitors,
            timers,
            health,
            last_action: self.last_action.clone(),
            manual_remaining_seconds: self
                .manual
                .as_ref()
                .and_then(|m| m.0)
                .map(|until| until.saturating_sub(now)),
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
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "vigild [--config PATH] [--socket PATH] [--dry-run] [--check-config]\nNative Wayland session idle manager. Configuration: TOML."
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("vigild {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--dry-run" => dry = true,
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
    let path = match socket {
        Some(p) => p,
        None => socket_path()?,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    serve(path.clone(), tx.clone()).await?;
    let clock = Instant::now();
    let (config_tx, config_rx) = watch::channel((0, c.clone()));
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
        manual: None,
        last_action: None,
    };
    wayland::start(tx.clone());
    logind::start(tx.clone(), config_rx.clone(), dry);
    let sensor_tx = tx.clone();
    let mut config_rx = config_rx;
    tokio::spawn(async move {
        let mut bus = None;
        loop {
            let (revision, config) = config_rx.borrow_and_update().clone();
            let activity = config.activity;
            let gathered = tokio::select! {
                result = sensors::gather(&activity) => result,
                _ = config_rx.changed() => continue,
            };
            let (mut list, mut health) = gathered;
            if bus.is_none() {
                bus = zbus::Connection::system().await.ok();
            }
            if let Some(conn) = &bus {
                match timeout(Duration::from_secs(3), logind::inhibitors(conn)).await {
                    Ok(Ok(found)) => list.extend(found),
                    _ => {
                        health.insert(
                            "external-inhibitors".into(),
                            "Cannot read logind inhibitors".into(),
                        );
                        bus = None;
                    }
                }
            } else {
                health.insert(
                    "external-inhibitors".into(),
                    "System bus unavailable".into(),
                );
            }
            if sensor_tx
                .send(Event::Sensors(revision, list, health))
                .is_err()
            {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(activity.poll_seconds)) => {},
                _ = config_rx.changed() => {},
            }
        }
    });
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
        match request {
            Request::Status => Ok("status".into()),
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
                Ok("Configuration reloaded; idle timers reset, manual override preserved".into())
            }
            Request::Toggle => {
                self.inhibitors(now);
                if self.manual.take().is_some() {
                    Ok("Automatic policy restored".into())
                } else {
                    self.manual = Some((None, ACTIONS.to_vec()));
                    Ok("Manual keep-awake enabled".into())
                }
            }
            Request::KeepAwake { seconds, scope } => {
                if scope.is_empty() || seconds == Some(0) {
                    return Err("scope and duration must be nonempty/positive".into());
                }
                self.manual = Some((seconds.map(|s| now.saturating_add(s)), scope));
                Ok("Manual keep-awake enabled".into())
            }
            Request::Automatic => {
                self.manual = None;
                Ok("Automatic policy restored".into())
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
    async fn tick(&mut self, now: u64) {
        // Resumed means input is still active until the next one-second idle event.
        if !self.input_idle {
            self.policy.activity(now);
        }
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
            manual: None,
            last_action: None,
        }
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
    async fn toggle_expires_old_override_before_deciding() {
        let mut r = runtime();
        r.manual = Some((Some(2), vec![Action::Suspend]));
        r.control(Request::Toggle, 3).await.unwrap();
        assert_eq!(r.manual, Some((None, ACTIONS.to_vec())));
        r.control(Request::Toggle, 3).await.unwrap();
        assert!(r.manual.is_none());
    }
    #[tokio::test]
    async fn reload_rejects_invalid_config_and_preserves_manual_override() {
        let mut r = runtime();
        let path =
            std::env::temp_dir().join(format!("vigil-reload-test-{}.toml", std::process::id()));
        r.config_file = path.clone();
        r.manual = Some((Some(5000), vec![Action::Suspend]));
        std::fs::write(&path, "[timeouts]\nlock_seconds = 42").unwrap();
        r.control(Request::Reload, 2000).await.unwrap();
        assert_eq!(r.c.timeouts.lock_seconds, 42);
        assert_eq!(r.policy.elapsed(2000), 0);
        assert_eq!(r.manual, Some((Some(5000), vec![Action::Suspend])));
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
