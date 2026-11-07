//! Long-lived activity subscriptions. Each reconnect starts from a fresh snapshot.
use crate::{Event, logind, sensors};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::{mpsc, watch},
    task::JoinSet,
    time::{sleep, timeout},
};
use vigil_core::{ACTIONS, ActivityConfig, Config, Fullscreen, Inhibitor};
use zbus::{Connection, MatchRule, MessageStream, Proxy, message::Type};

type Update = (&'static str, Vec<Inhibitor>, Option<String>);
type Sender = mpsc::Sender<Update>;
const LIMIT: usize = 8 * 1024 * 1024;

pub fn start(tx: mpsc::UnboundedSender<Event>, mut config: watch::Receiver<(u64, Config)>) {
    tokio::spawn(async move {
        loop {
            let (revision, c) = config.borrow_and_update().clone();
            let (updates, mut rx) = mpsc::channel(32);
            let mut tasks = JoinSet::new();
            let mut states = BTreeMap::new();
            for name in ["pipewire", "mpris", "fullscreen", "external-inhibitors"] {
                states.insert(name, (vec![], Some("initializing".to_string())));
                let updates = updates.clone();
                let activity = c.activity.clone();
                tasks.spawn(async move {
                    loop {
                        let result = match name {
                            "pipewire" if activity.audio || activity.recording => {
                                pipewire(&updates, &activity).await
                            }
                            "mpris" if activity.media => mpris(&updates, &activity).await,
                            "fullscreen" if activity.fullscreen == Fullscreen::Geometry => {
                                niri(&updates, &activity).await
                            }
                            "external-inhibitors" => external(&updates, &activity).await,
                            _ => {
                                let _ = updates.send((name, vec![], None)).await;
                                std::future::pending::<()>().await;
                                return;
                            }
                        };
                        if updates.send((name, vec![], result.err())).await.is_err() {
                            return;
                        }
                        sleep(Duration::from_secs(2)).await;
                    }
                });
            }
            drop(updates);
            loop {
                tokio::select! {
                    changed = config.changed() => {
                        if changed.is_err() { return; }
                        break;
                    }
                    update = rx.recv() => {
                        let Some((name, list, error)) = update else { return; };
                        states.insert(name, (list, error));
                        let mut all = Vec::new();
                        let mut health = BTreeMap::new();
                        for (name, (list, error)) in &states {
                            all.extend(list.clone());
                            if let Some(error) = error {
                                health.insert((*name).into(), error.clone());
                                if *name == "pipewire" && (c.activity.audio || c.activity.recording) {
                                    all.push(Inhibitor {
                                        id: "degraded:pipewire".into(),
                                        reason: "Audio state unavailable".into(),
                                        scope: ACTIONS.into_iter().filter(|a| (c.activity.audio && c.activity.audio_scope.contains(a)) || (c.activity.recording && c.activity.recording_scope.contains(a))).collect(),
                                    });
                                }
                            }
                        }
                        if tx.send(Event::Sensors(revision, all, health)).is_err() { return; }
                    }
                }
            }
            // JoinSet aborts all old-generation subscriptions before reconnecting.
            tasks.shutdown().await;
        }
    });
}

fn merge(current: &mut Value, update: &Value) {
    if let (Some(current), Some(update)) = (current.as_object_mut(), update.as_object()) {
        for (key, value) in update {
            if value.is_null() {
                current.remove(key);
            } else {
                merge(current.entry(key).or_insert(Value::Null), value);
            }
        }
    } else {
        *current = update.clone();
    }
}
#[derive(Default)]
struct PipeWireState(BTreeMap<u64, Value>);
impl PipeWireState {
    fn apply(&mut self, update: &Value) -> Result<(), String> {
        for object in update.as_array().ok_or("PipeWire update is not an array")? {
            let id = object["id"].as_u64().ok_or("PipeWire object has no id")?;
            if object.get("info") == Some(&Value::Null) {
                self.0.remove(&id);
                continue;
            }
            if self.0.contains_key(&id)
                || matches!(
                    object["type"].as_str(),
                    Some("PipeWire:Interface:Node" | "PipeWire:Interface:Link")
                )
            {
                merge(self.0.entry(id).or_insert(Value::Null), object);
            }
        }
        Ok(())
    }
    fn inhibitors(&self, c: &ActivityConfig) -> Vec<Inhibitor> {
        sensors::audio(&Value::Array(self.0.values().cloned().collect()), c)
    }
}
async fn pipewire(tx: &Sender, c: &ActivityConfig) -> Result<(), String> {
    let mut child = Command::new("pw-dump")
        .args(["--monitor", "--no-colors"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("pw-dump: {e}"))?;
    let mut output = child.stdout.take().ok_or("pw-dump stdout unavailable")?;
    let mut state = PipeWireState::default();
    let mut buffer = Vec::new();
    let mut chunk = [0; 8192];
    let mut initialized = false;
    loop {
        let read = output.read(&mut chunk);
        let n = if initialized {
            read.await
        } else {
            timeout(Duration::from_secs(5), read)
                .await
                .map_err(|_| "PipeWire initial snapshot timed out")?
        }
        .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("PipeWire monitor disconnected".into());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > LIMIT {
            return Err("PipeWire update exceeds size limit".into());
        }
        loop {
            let mut stream = serde_json::Deserializer::from_slice(&buffer).into_iter::<Value>();
            match stream.next() {
                Some(Ok(update)) => {
                    let consumed = stream.byte_offset();
                    state.apply(&update)?;
                    buffer.drain(..consumed);
                    initialized = true;
                    tx.send(("pipewire", state.inhibitors(c), None))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Some(Err(e)) if !e.is_eof() => return Err(format!("PipeWire JSON: {e}")),
                _ => break,
            }
        }
    }
}

async fn session_bus() -> Result<Connection, String> {
    zbus::connection::Builder::session()
        .map_err(|e| e.to_string())?
        .method_timeout(Duration::from_secs(3))
        .build()
        .await
        .map_err(|e| e.to_string())
}
async fn media_snapshot(
    bus: &Connection,
    c: &ActivityConfig,
) -> Result<(Vec<Inhibitor>, Option<String>), String> {
    let dbus = zbus::fdo::DBusProxy::new(bus)
        .await
        .map_err(|e| e.to_string())?;
    let names = dbus.list_names().await.map_err(|e| e.to_string())?;
    let mut list = Vec::new();
    let mut errors = Vec::new();
    for name in names {
        let name = name.as_str();
        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }
        let player = Proxy::new(
            bus,
            name,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
        )
        .await
        .map_err(|e| e.to_string())?;
        match player.get_property::<String>("PlaybackStatus").await {
            Ok(status) if status == "Playing" => list.push(Inhibitor {
                id: format!("mpris:{name}"),
                reason: format!(
                    "Media playing: {}",
                    name.trim_start_matches("org.mpris.MediaPlayer2.")
                ),
                scope: c.media_scope.clone(),
            }),
            Ok(_) => {}
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    Ok((list, (!errors.is_empty()).then(|| errors.join("; "))))
}
async fn mpris(tx: &Sender, c: &ActivityConfig) -> Result<(), String> {
    mpris_on_bus(tx, c, session_bus().await?).await
}
async fn mpris_on_bus(tx: &Sender, c: &ActivityConfig, bus: Connection) -> Result<(), String> {
    let properties = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .map_err(|e| e.to_string())?
        .member("PropertiesChanged")
        .map_err(|e| e.to_string())?
        .path("/org/mpris/MediaPlayer2")
        .map_err(|e| e.to_string())?
        .build();
    let mut properties = MessageStream::for_match_rule(properties, &bus, Some(64))
        .await
        .map_err(|e| e.to_string())?;
    let dbus = zbus::fdo::DBusProxy::new(&bus)
        .await
        .map_err(|e| e.to_string())?;
    let mut owners = dbus
        .receive_name_owner_changed()
        .await
        .map_err(|e| e.to_string())?;
    loop {
        let (list, error) = media_snapshot(&bus, c).await?;
        tx.send(("mpris", list, error))
            .await
            .map_err(|e| e.to_string())?;
        loop {
            tokio::select! {
                message = properties.next() => {
                    message.ok_or("MPRIS signal stream ended")?.map_err(|e| e.to_string())?;
                    break;
                }
                message = owners.next() => {
                    let message = message.ok_or("D-Bus owner signal stream ended")?;
                    if message.args().map_err(|e| e.to_string())?.name().as_str().starts_with("org.mpris.MediaPlayer2.") { break; }
                }
            }
        }
    }
}

async fn niri_socket() -> Result<UnixStream, String> {
    let path = std::env::var_os("NIRI_SOCKET").ok_or("NIRI_SOCKET is not set")?;
    UnixStream::connect(path).await.map_err(|e| e.to_string())
}
async fn line(reader: &mut BufReader<UnixStream>) -> Result<Value, String> {
    let mut bytes = Vec::new();
    let count = reader
        .take(LIMIT as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if count == 0 || count == LIMIT || bytes.last() != Some(&b'\n') {
        return Err("Niri stream ended or exceeded message limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
async fn outputs() -> Result<Value, String> {
    timeout(Duration::from_secs(3), async {
        let mut socket = niri_socket().await?;
        socket
            .write_all(b"\"Outputs\"\n")
            .await
            .map_err(|e| e.to_string())?;
        let reply = line(&mut BufReader::new(socket)).await?;
        reply["Ok"]
            .get("Outputs")
            .cloned()
            .ok_or_else(|| "Invalid Niri Outputs response".into())
    })
    .await
    .map_err(|_| "Niri Outputs request timed out")?
}
#[derive(Default)]
struct NiriState {
    windows: BTreeMap<u64, Value>,
    workspaces: BTreeMap<u64, Value>,
}
fn window(mut w: Value) -> Value {
    if let Some(object) = w.as_object_mut() {
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "id" | "workspace_id" | "app_id" | "layout" | "is_fullscreen"
            )
        });
    }
    w
}
impl NiriState {
    fn apply(&mut self, e: &Value) {
        if let Some(windows) = e["WindowsChanged"]["windows"].as_array() {
            self.windows = windows
                .iter()
                .filter_map(|w| Some((w["id"].as_u64()?, window(w.clone()))))
                .collect();
        }
        if let Some(id) = e["WindowOpenedOrChanged"]["window"]["id"].as_u64() {
            self.windows
                .insert(id, window(e["WindowOpenedOrChanged"]["window"].clone()));
        }
        if let Some(id) = e["WindowClosed"]["id"].as_u64() {
            self.windows.remove(&id);
        }
        if let Some(changes) = e["WindowLayoutsChanged"]["changes"].as_array() {
            for change in changes {
                if let Some(w) = change[0].as_u64().and_then(|id| self.windows.get_mut(&id)) {
                    w["layout"] = change[1].clone();
                }
            }
        }
        if let Some(workspaces) = e["WorkspacesChanged"]["workspaces"].as_array() {
            self.workspaces = workspaces
                .iter()
                .filter_map(|w| Some((w["id"].as_u64()?, w.clone())))
                .collect();
        }
        if let Some(id) = e["WorkspaceActivated"]["id"].as_u64() {
            let output = self.workspaces.get(&id).map(|w| w["output"].clone());
            if let Some(output) = output {
                for (ws_id, ws) in &mut self.workspaces {
                    if ws["output"] == output {
                        ws["is_active"] = json!(*ws_id == id);
                    }
                }
            }
        }
        if let Some(id) = e["WorkspaceActiveWindowChanged"]["workspace_id"].as_u64()
            && let Some(ws) = self.workspaces.get_mut(&id)
        {
            ws["active_window_id"] = e["WorkspaceActiveWindowChanged"]["active_window_id"].clone();
        }
    }
    fn inhibitors(&self, outputs: &Value) -> Vec<Inhibitor> {
        sensors::fullscreen(
            &json!(self.windows.values().collect::<Vec<_>>()),
            &json!(self.workspaces.values().collect::<Vec<_>>()),
            outputs,
        )
    }
}
async fn niri(tx: &Sender, c: &ActivityConfig) -> Result<(), String> {
    let mut socket = niri_socket().await?;
    socket
        .write_all(b"\"EventStream\"\n")
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(socket);
    let reply = timeout(Duration::from_secs(3), line(&mut reader))
        .await
        .map_err(|_| "Niri event subscription timed out")??;
    if reply.get("Ok").is_none() {
        return Err("Niri rejected event subscription".into());
    }
    let mut state = NiriState::default();
    let mut outputs = outputs().await?;
    let mut refresh = tokio::time::interval(Duration::from_secs(c.poll_seconds));
    let mut windows_ready = false;
    let mut workspaces_ready = false;
    // Persistent reader task avoids losing a partially read line when the timer wins select.
    let (events, mut received) = mpsc::channel(32);
    let mut reads = JoinSet::new();
    reads.spawn(async move {
        loop {
            let event = line(&mut reader).await;
            let failed = event.is_err();
            if events.send(event).await.is_err() || failed {
                break;
            }
        }
    });
    loop {
        tokio::select! {
            event = received.recv() => {
                let event = event.ok_or("Niri event reader ended")??;
                windows_ready |= event.get("WindowsChanged").is_some();
                workspaces_ready |= event.get("WorkspacesChanged").is_some();
                state.apply(&event);
            }
            _ = refresh.tick() => { outputs = self::outputs().await?; }
        }
        if windows_ready && workspaces_ready {
            tx.send(("fullscreen", state.inhibitors(&outputs), None))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
}
async fn external(tx: &Sender, c: &ActivityConfig) -> Result<(), String> {
    let bus = zbus::connection::Builder::system()
        .map_err(|e| e.to_string())?
        .method_timeout(Duration::from_secs(3))
        .build()
        .await
        .map_err(|e| e.to_string())?;
    // ListInhibitors has no per-inhibitor event API; retain a bounded reconciliation.
    loop {
        let list = logind::inhibitors(&bus).await?;
        tx.send(("external-inhibitors", list, None))
            .await
            .map_err(|e| e.to_string())?;
        sleep(Duration::from_secs(c.poll_seconds)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pipewire_partial_updates_and_removal_do_not_keep_stale_playback() {
        let mut s = PipeWireState::default();
        let c = ActivityConfig::default();
        s.apply(&json!([
            {"id":1,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"media.class":"Stream/Output/Audio","application.name":"music"}}},
            {"id":2,"type":"PipeWire:Interface:Link","info":{"state":"active","input-node-id":3,"output-node-id":1}}
        ])).unwrap();
        assert_eq!(s.inhibitors(&c).len(), 1);
        s.apply(&json!([{"id":1,"info":{"params":{"Props":[{"mute":true}]}}}]))
            .unwrap();
        assert!(s.inhibitors(&c).is_empty());
        s.apply(&json!([{"id":1,"info":{"params":{"Props":[{"mute":false}]}}}]))
            .unwrap();
        assert_eq!(s.inhibitors(&c).len(), 1);
        s.apply(&json!([{"id":2,"info":null}])).unwrap();
        assert!(s.inhibitors(&c).is_empty());
        assert!(PipeWireState::default().inhibitors(&c).is_empty());
    }
    #[test]
    fn niri_events_follow_layout_visibility_and_close_without_retaining_titles() {
        let mut s = NiriState::default();
        let o = json!({"DP-1":{"logical":{"width":1920,"height":1080}}});
        s.apply(&json!({"WorkspacesChanged":{"workspaces":[{"id":1,"output":"DP-1","is_active":true},{"id":2,"output":"DP-1","is_active":false}]}}));
        s.apply(&json!({"WindowOpenedOrChanged":{"window":{"id":3,"title":"private","workspace_id":1,"layout":{"window_size":[1920,1080],"tile_pos_in_workspace_view":[0,0]}}}}));
        assert!(s.windows[&3].get("title").is_none());
        assert_eq!(s.inhibitors(&o).len(), 1);
        s.apply(&json!({"WorkspaceActivated":{"id":2,"focused":true}}));
        assert!(s.inhibitors(&o).is_empty());
        s.apply(&json!({"WorkspaceActivated":{"id":1,"focused":true}}));
        s.apply(&json!({"WindowLayoutsChanged":{"changes":[[3,{"window_size":[100,100]}]]}}));
        assert!(s.inhibitors(&o).is_empty());
        s.apply(&json!({"WindowClosed":{"id":3}}));
        assert!(s.windows.is_empty());
    }
    struct Player {
        status: String,
    }
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        #[zbus(property)]
        fn playback_status(&self) -> &str {
            &self.status
        }
    }
    struct BrokenPlayer;
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl BrokenPlayer {
        #[zbus(property)]
        fn playback_status(&self) -> zbus::fdo::Result<String> {
            Err(zbus::fdo::Error::Failed("player unavailable".into()))
        }
    }
    #[tokio::test]
    #[ignore = "requires dbus-daemon and permission to create a private bus socket"]
    async fn mpris_tracks_play_pause_and_player_removal() {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .await
            .unwrap();
        let address = address.trim();
        let service = zbus::connection::Builder::address(address)
            .unwrap()
            .name("org.mpris.MediaPlayer2.VigilTest")
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                Player {
                    status: "Playing".into(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let broken = zbus::connection::Builder::address(address)
            .unwrap()
            .name("org.mpris.MediaPlayer2.Broken")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", BrokenPlayer)
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let monitor =
            tokio::spawn(
                async move { mpris_on_bus(&tx, &ActivityConfig::default(), client).await },
            );
        let (_, list, error) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(error.unwrap().contains("Broken"));
        assert_eq!(list.len(), 1);
        let player = service
            .object_server()
            .interface::<_, Player>("/org/mpris/MediaPlayer2")
            .await
            .unwrap();
        player.get_mut().await.status = "Paused".into();
        player
            .get()
            .await
            .playback_status_changed(player.signal_emitter())
            .await
            .unwrap();
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(list.is_empty());
        player.get_mut().await.status = "Playing".into();
        player
            .get()
            .await
            .playback_status_changed(player.signal_emitter())
            .await
            .unwrap();
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(list.len(), 1);
        service
            .release_name("org.mpris.MediaPlayer2.VigilTest")
            .await
            .unwrap();
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(list.is_empty());
        broken
            .release_name("org.mpris.MediaPlayer2.Broken")
            .await
            .unwrap();
        let (_, list, error) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(list.is_empty());
        assert!(error.is_none());
        monitor.abort();
        let _ = monitor.await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }
}
