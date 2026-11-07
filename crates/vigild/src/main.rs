mod actions;
mod idle_hint;
mod logind;
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
use vigil_core::{ACTIONS, Action, Caffeine, Config, Inhibitor, Policy, Request, Response, Status};

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
    caffeine: Caffeine,
    pending: Option<Pending>,
    cycle: u64,
    wake_retry_at: u64,
    wake_requested: bool,
    lock_requested: bool,
    last_action: Option<String>,
    idle_hint: Option<bool>,
    idle_hint_tx: watch::Sender<Option<bool>>,
}
impl Runtime {
    fn inhibitors(&mut self, now: u64) -> Vec<Inhibitor> {
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
        if self.caffeine.status(now).is_some() {
            list.push(Inhibitor {
                id: "caffeine".into(),
                reason: "Keep awake".into(),
                scope: ACTIONS.to_vec(),
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
            caffeine: self.caffeine.status(now),
        }
    }
    fn begin(&mut self, work: Work, automatic: bool, reply: Option<oneshot::Sender<Response>>) {
        let commands = self.c.commands.clone();
        let dry = self.dry;
        let task = tokio::spawn(async move {
            if dry {
                return Ok(());
            }
            match work {
                Work::Action(Action::Display) => {
                    actions::output(&commands.display_off, 5).await.map(|_| ())
                }
                Work::Action(Action::Lock) | Work::PrepareSuspend => {
                    actions::lock(&commands, commands.lock_timeout_seconds).await
                }
                Work::Action(Action::Suspend) => {
                    actions::output(&commands.suspend, 10).await.map(|_| ())
                }
                Work::Wake => actions::output(&commands.display_on, 5).await.map(|_| ()),
            }
        });
        self.pending = Some(Pending {
            work,
            automatic,
            cycle: self.cycle,
            reply,
            task,
        });
    }
    fn eligible(&mut self, action: Action, now: u64) -> bool {
        self.input_idle
            && !self.sleeping
            && self.status(now).timers.iter().any(|t| {
                t.action == action
                    && t.enabled
                    && !t.blocked
                    && !t.completed
                    && t.remaining_seconds == 0
            })
    }
    fn finished(&mut self, result: Result<(), String>, now: u64) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if result.is_ok() && matches!(pending.work, Work::PrepareSuspend) {
            if !pending.automatic
                || (pending.cycle == self.cycle && self.eligible(Action::Suspend, now))
            {
                self.begin(
                    Work::Action(Action::Suspend),
                    pending.automatic,
                    pending.reply,
                );
            }
            return;
        }
        let action = match pending.work {
            Work::Action(action) => Some(action),
            Work::PrepareSuspend => Some(Action::Suspend),
            Work::Wake => None,
        };
        match &result {
            Ok(()) => {
                if let Some(action) = action {
                    if pending.automatic
                        && pending.cycle == self.cycle
                        && self.eligible(action, now)
                    {
                        self.policy.complete(action);
                    }
                    if action == Action::Display {
                        self.blanked = true;
                        self.wake_requested |= pending.cycle != self.cycle;
                    }
                } else {
                    self.blanked = false;
                    self.wake_requested = false;
                    self.health.remove("display");
                }
            }
            Err(error) => {
                if let Some(action) = action {
                    self.policy.failed(action, now, &self.c);
                } else {
                    self.health.insert("display".into(), error.clone());
                    self.wake_retry_at = now.saturating_add(self.c.timeouts.retry_seconds);
                }
            }
        }
        self.last_action = Some(match &result {
            Ok(()) => format!(
                "{} {:?}",
                if self.dry {
                    "dry-run: would"
                } else {
                    "Completed"
                },
                pending.work
            ),
            Err(error) => format!("Failed {:?}: {error}", pending.work),
        });
        if let Some(reply) = pending.reply {
            let _ = reply.send(Response {
                ok: result.is_ok(),
                message: result.map_or_else(|e| e, |_| "Action completed".into()),
                status: Some(self.status(now)),
            });
        }
    }
}
#[derive(Debug, Clone, Copy)]
enum Work {
    Action(Action),
    PrepareSuspend,
    Wake,
}
struct Pending {
    work: Work,
    automatic: bool,
    cycle: u64,
    reply: Option<oneshot::Sender<Response>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
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
                        match timeout(Duration::from_secs(90), receive).await {
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
        caffeine: Caffeine::default(),
        pending: None,
        cycle: 0,
        wake_retry_at: 0,
        wake_requested: false,
        lock_requested: false,
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
            biased;
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                r.event(event, clock.elapsed().as_secs()).await;
            },
            result = async {
                match &mut r.pending {
                    Some(pending) => (&mut pending.task).await.map_err(|e| format!("action task: {e}")).and_then(|r| r),
                    None => std::future::pending().await,
                }
            } => r.finished(result, clock.elapsed().as_secs()),
            _ = tick.tick() => r.tick(clock.elapsed().as_secs()).await,
        }
    }

    if let Some(pending) = r.pending.take() {
        pending.task.abort();
        let _ = pending.task.await;
        // An interrupted display command may already have switched the outputs off.
        r.blanked |= matches!(pending.work, Work::Action(Action::Display));
    }
    if r.blanked && !r.dry {
        let _ = actions::output(&r.c.commands.display_on, 5).await;
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
            self.cycle += 1;
        }
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
            lines.push(format!(
                "  {}: {}",
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
    async fn control(&mut self, request: Request, now: u64) -> Result<String, String> {
        match request {
            Request::Status => Ok("status".into()),
            Request::Explain { action } => Ok(self.explain(action, now)),
            Request::Reload => {
                let text = std::fs::read_to_string(&self.config_file).map_err(|e| e.to_string())?;
                let next = Config::parse(&text)?;
                if self.pending.is_some() || self.blanked {
                    return Err(
                        "Wait for the current action or wake displays before reloading".into(),
                    );
                }
                self.c = next;
                self.revision += 1;
                self.cycle += 1;
                self.policy.activity(now);
                self.sensors.clear();
                self.sensor_health.clear();
                self.health.insert(
                    "sensors".into(),
                    "Refreshing after configuration reload".into(),
                );
                self.config_tx.send_replace((self.revision, self.c.clone()));
                Ok("Configuration reloaded; idle timers reset, keep-awake preserved".into())
            }
            Request::KeepAwake { seconds } => {
                self.caffeine.set(now, seconds)?;
                Ok("Keep awake enabled".into())
            }
            Request::Toggle => {
                self.caffeine.toggle(now);
                Ok(if self.caffeine.status(now).is_some() {
                    "Keep awake enabled"
                } else {
                    "Automatic idle handling"
                }
                .into())
            }
            Request::Automatic => {
                self.caffeine.clear();
                Ok("Automatic idle handling".into())
            }
            Request::Lock | Request::Suspend | Request::Wake => {
                Err("Action must be dispatched through the event loop".into())
            }
        }
    }
    async fn event(&mut self, event: Event, now: u64) {
        match event {
            Event::IdleHint(value) => self.idle_hint = value,
            Event::InputIdle(idle) => {
                self.input(idle, now);
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
                    self.cycle += 1;
                    self.wake_requested = true;
                }
            }
            Event::LockRequested => {
                self.lock_requested = true;
            }
            Event::Control(request, reply) => {
                let work = match request {
                    Request::Lock => Some(Work::Action(Action::Lock)),
                    Request::Suspend => Some(Work::PrepareSuspend),
                    Request::Wake => Some(Work::Wake),
                    _ => None,
                };
                if let Some(work) = work {
                    if self.pending.is_none() {
                        if matches!(work, Work::Wake) {
                            self.policy.activity(now);
                            self.cycle += 1;
                        }
                        self.begin(work, false, Some(reply));
                    } else {
                        let _ = reply.send(Response {
                            ok: false,
                            message: "An action is already in progress".into(),
                            status: Some(self.status(now)),
                        });
                    }
                    return;
                }
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
        if self.pending.is_some() {
            return;
        }
        if self.lock_requested {
            self.lock_requested = false;
            self.begin(Work::Action(Action::Lock), false, None);
            return;
        }
        if self.wake_requested
            || (self.blanked
                && (!self.input_idle
                    || status
                        .timers
                        .iter()
                        .any(|t| t.action == Action::Display && t.blocked)))
        {
            if now >= self.wake_retry_at {
                self.begin(Work::Wake, false, None);
            }
            return;
        }
        if let Some(timer) = status
            .timers
            .iter()
            .find(|t| t.enabled && !t.blocked && !t.completed && t.remaining_seconds == 0)
        {
            let work = if timer.action == Action::Suspend {
                Work::PrepareSuspend
            } else {
                Work::Action(timer.action)
            };
            self.begin(work, true, None);
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
            input_idle: true,
            effective_idle: true,
            sleeping: false,
            blanked: false,
            caffeine: Caffeine::default(),
            pending: None,
            cycle: 0,
            wake_retry_at: 0,
            wake_requested: false,
            lock_requested: false,
            last_action: None,
            idle_hint: None,
            idle_hint_tx: watch::channel(None).0,
        }
    }
    async fn finish(r: &mut Runtime, now: u64) {
        let result = (&mut r.pending.as_mut().unwrap().task).await.unwrap();
        r.finished(result, now);
    }
    fn only_suspend(r: &mut Runtime) {
        r.c.timeouts.display_seconds = 0;
        r.c.timeouts.lock_seconds = 0;
    }
    #[tokio::test]
    async fn caffeine_replaces_expires_and_preserves_playback() {
        let mut r = runtime();
        r.control(Request::KeepAwake { seconds: Some(10) }, 2000)
            .await
            .unwrap();
        r.control(Request::KeepAwake { seconds: Some(30) }, 2005)
            .await
            .unwrap();
        assert_eq!(r.status(2010).caffeine.unwrap().remaining_seconds, Some(25));
        r.sensors.push(Inhibitor {
            id: "audio".into(),
            reason: "Playback".into(),
            scope: ACTIONS.to_vec(),
        });
        assert!(r.status(2035).caffeine.is_none());
        assert!(r.status(2035).timers.iter().all(|t| t.blocked));
        r.control(Request::Automatic, 2036).await.unwrap();
        assert_eq!(r.status(2036).inhibitors.len(), 1);
        r.sensors.clear();
        let status = r.status(2037);
        assert_eq!(status.timers[0].remaining_seconds, 900);
        assert_eq!(status.timers[2].remaining_seconds, 1800);
        r.tick(2037).await;
        assert!(r.pending.is_none());
    }
    #[tokio::test]
    async fn slow_lock_keeps_control_responsive_and_input_cancels_auto_suspend() {
        let mut r = runtime();
        only_suspend(&mut r);
        let (done, wait) = oneshot::channel();
        r.pending = Some(Pending {
            work: Work::PrepareSuspend,
            automatic: true,
            cycle: 0,
            reply: None,
            task: tokio::spawn(async move {
                wait.await.unwrap();
                Ok(())
            }),
        });
        let (reply, response) = oneshot::channel();
        r.event(Event::Control(Request::Status, reply), 2000).await;
        assert!(response.await.unwrap().ok);
        r.event(Event::InputIdle(false), 2001).await;
        r.event(Event::InputIdle(true), 2002).await;
        done.send(()).unwrap();
        finish(&mut r, 2003).await;
        assert!(r.pending.is_none());
        r.tick(2003).await;
        assert!(r.pending.is_none());
    }
    #[tokio::test]
    async fn new_inhibitor_or_sleep_prevents_suspend_after_lock() {
        for sleeping in [false, true] {
            let mut r = runtime();
            only_suspend(&mut r);
            r.tick(2000).await;
            assert!(matches!(
                r.pending.as_ref().unwrap().work,
                Work::PrepareSuspend
            ));
            if sleeping {
                r.event(Event::Sleep(true), 2001).await;
            } else {
                r.control(Request::KeepAwake { seconds: None }, 2001)
                    .await
                    .unwrap();
            }
            finish(&mut r, 2002).await;
            assert!(r.pending.is_none());
        }
    }
    #[tokio::test]
    async fn failed_lock_never_suspends_and_retries_from_completion_time() {
        let mut r = runtime();
        only_suspend(&mut r);
        r.tick(2000).await;
        (&mut r.pending.as_mut().unwrap().task)
            .await
            .unwrap()
            .unwrap();
        r.finished(Err("locker failed".into()), 2010);
        assert!(r.pending.is_none());
        assert_eq!(r.status(2011).timers[2].remaining_seconds, 29);
    }
    #[tokio::test]
    async fn suspend_requires_lock_completion_and_explicit_request_bypasses_caffeine() {
        let mut r = runtime();
        r.caffeine.set(0, None).unwrap();
        let (reply, response) = oneshot::channel();
        r.event(Event::Control(Request::Suspend, reply), 2000).await;
        assert!(matches!(
            r.pending.as_ref().unwrap().work,
            Work::PrepareSuspend
        ));
        finish(&mut r, 2001).await;
        assert!(matches!(
            r.pending.as_ref().unwrap().work,
            Work::Action(Action::Suspend)
        ));
        finish(&mut r, 2002).await;
        assert!(response.await.unwrap().ok);
    }
    #[tokio::test]
    async fn activity_during_blanking_restores_display_without_completing_new_cycle() {
        let mut r = runtime();
        r.tick(1000).await;
        r.event(Event::InputIdle(false), 1001).await;
        r.event(Event::InputIdle(true), 1002).await;
        finish(&mut r, 1002).await;
        assert!(r.blanked);
        r.tick(1002).await;
        assert!(matches!(r.pending.as_ref().unwrap().work, Work::Wake));
        finish(&mut r, 1003).await;
        assert!(!r.blanked);
        assert!(!r.status(1003).timers[0].completed);
    }
    #[tokio::test]
    async fn continuous_input_disconnect_and_resume_reset_idle() {
        let mut r = runtime();
        r.input_idle = false;
        r.tick(10000).await;
        assert!(r.pending.is_none());
        r.event(Event::InputIdle(true), 10001).await;
        r.event(
            Event::Health("wayland".into(), Some("disconnected".into())),
            12000,
        )
        .await;
        r.tick(12000).await;
        assert!(r.pending.is_none());
        r.event(Event::InputIdle(false), 12001).await;
        r.event(Event::Health("wayland".into(), None), 12001).await;
        assert_eq!(r.status(12001).idle_seconds, 0);
        r.input_idle = true;
        r.event(Event::Sleep(true), 12002).await;
        r.event(Event::Sleep(false), 14000).await;
        assert_eq!(r.status(14000).idle_seconds, 0);
        r.tick(14000).await;
        finish(&mut r, 14001).await;
        assert!(!r.blanked);
    }
    #[tokio::test]
    async fn reload_preserves_caffeine_rejects_invalid_config_and_stale_observations() {
        let mut r = runtime();
        r.config_file =
            std::env::temp_dir().join(format!("vigil-reload-{}.toml", std::process::id()));
        r.caffeine.set(0, None).unwrap();
        std::fs::write(&r.config_file, "[timeouts]\nlock_seconds = 42").unwrap();
        r.control(Request::Reload, 2000).await.unwrap();
        assert_eq!(r.c.timeouts.lock_seconds, 42);
        assert!(r.status(2000).caffeine.is_some());
        r.event(Event::Sensors(0, vec![], BTreeMap::new()), 2001)
            .await;
        assert!(r.health.contains_key("sensors"));
        r.event(Event::Sensors(1, vec![], BTreeMap::new()), 2002)
            .await;
        assert!(!r.health.contains_key("sensors"));
        std::fs::write(&r.config_file, "typo = true").unwrap();
        assert!(r.control(Request::Reload, 2003).await.is_err());
        assert_eq!(r.c.timeouts.lock_seconds, 42);
        assert!(r.explain(Action::Lock, 2003).contains("inhibited"));
        std::fs::remove_file(&r.config_file).unwrap();
    }
}
