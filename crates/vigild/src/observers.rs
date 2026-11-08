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
type MediaProperties = (
    String,
    std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    Vec<String>,
);

struct MediaPlayer {
    owner: String,
    playback: Result<String, String>,
}
async fn playback(bus: &Connection, owner: &str) -> Result<String, String> {
    let player = Proxy::new(
        bus,
        owner,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await
    .map_err(|e| e.to_string())?;
    player
        .get_property::<String>("PlaybackStatus")
        .await
        .map_err(|e| e.to_string())
}
fn media_state(
    players: &BTreeMap<String, MediaPlayer>,
    c: &ActivityConfig,
) -> (Vec<Inhibitor>, Option<String>) {
    let mut list = Vec::new();
    let mut errors = Vec::new();
    for (name, player) in players {
        match &player.playback {
            Ok(status) if status == "Playing" => list.push(Inhibitor {
                id: format!("mpris:{name}"),
                reason: format!(
                    "Media playing: {}",
                    name.trim_start_matches("org.mpris.MediaPlayer2.")
                ),
                scope: c.media_scope.clone(),
            }),
            Ok(_) => {}
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    (list, (!errors.is_empty()).then(|| errors.join("; ")))
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
    // Subscribe before the initial snapshot so lifecycle changes are not lost.
    let mut players = BTreeMap::new();
    for name in dbus.list_names().await.map_err(|e| e.to_string())? {
        if !name.as_str().starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }
        // A player may disappear between listing names and resolving its owner.
        let owner = match dbus.get_name_owner(name.clone().into()).await {
            Ok(owner) => owner.to_string(),
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let playback = playback(&bus, &owner).await;
        players.insert(name.to_string(), MediaPlayer { owner, playback });
    }
    loop {
        let (list, error) = media_state(&players, c);
        tx.send(("mpris", list, error))
            .await
            .map_err(|e| e.to_string())?;
        loop {
            tokio::select! {
                message = properties.next() => {
                    let message = message.ok_or("MPRIS signal stream ended")?.map_err(|e| e.to_string())?;
                    let header = message.header();
                    let Some(sender) = header.sender() else { continue; };
                    if !players.values().any(|p| p.owner == sender.as_str()) { continue; }
                    let (interface, mut changed, invalidated): MediaProperties =
                        message.body().deserialize().map_err(|e| e.to_string())?;
                    if interface != "org.mpris.MediaPlayer2.Player" { continue; }
                    let next = if let Some(value) = changed.remove("PlaybackStatus") {
                        String::try_from(value).map_err(|e| e.to_string())
                    } else if invalidated.iter().any(|key| key == "PlaybackStatus") {
                        playback(&bus, sender.as_str()).await
                    } else { continue; };
                    for player in players.values_mut().filter(|p| p.owner == sender.as_str()) {
                        player.playback = next.clone();
                    }
                    break;
                }
                message = owners.next() => {
                    let message = message.ok_or("D-Bus owner signal stream ended")?;
                    let args = message.args().map_err(|e| e.to_string())?;
                    let name = args.name().as_str();
                    if !name.starts_with("org.mpris.MediaPlayer2.") { continue; }
                    let old = args.old_owner().as_ref().map(|o| o.as_str());
                    let new = args.new_owner().as_ref().map(|o| o.as_str());
                    if let Some(player) = players.get(name) {
                        // Ignore events already reflected in the snapshot or from an older owner.
                        if Some(player.owner.as_str()) == new || Some(player.owner.as_str()) != old {
                            continue;
                        }
                    }
                    if let Some(owner) = new {
                        let playback = playback(&bus, owner).await;
                        players.insert(name.to_string(), MediaPlayer {
                            owner: owner.into(), playback,
                        });
                    } else { players.remove(name); }
                    break;
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
    struct BrokenPlayer(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl BrokenPlayer {
        #[zbus(property)]
        fn playback_status(&self) -> zbus::fdo::Result<String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        let broken_reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let broken = zbus::connection::Builder::address(address)
            .unwrap()
            .name("org.mpris.MediaPlayer2.Broken")
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                BrokenPlayer(broken_reads.clone()),
            )
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
        let initial_broken_reads = broken_reads.load(std::sync::atomic::Ordering::SeqCst);
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
        // Invalidations refresh only the affected player; unrelated signals do no work.
        player.get_mut().await.status = "Paused".into();
        service
            .emit_signal(
                None::<&str>,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    "org.mpris.MediaPlayer2.Player",
                    std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    vec!["PlaybackStatus"],
                ),
            )
            .await
            .unwrap();
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(list.is_empty());
        assert_eq!(
            broken_reads.load(std::sync::atomic::Ordering::SeqCst),
            initial_broken_reads
        );
        service
            .emit_signal(
                None::<&str>,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    "org.mpris.MediaPlayer2.Player",
                    std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    vec!["Metadata"],
                ),
            )
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );
        service
            .release_name("org.mpris.MediaPlayer2.VigilTest")
            .await
            .unwrap();
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(list.is_empty());
        let replacement = zbus::connection::Builder::address(address)
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
        let (_, list, _) = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(list.len(), 1);
        // The old owner's Paused signal must not overwrite the replacement's Playing state.
        player
            .get()
            .await
            .playback_status_changed(player.signal_emitter())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );
        replacement
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
