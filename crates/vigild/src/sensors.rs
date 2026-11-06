use crate::actions;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use vigil_core::{ACTIONS, Action, ActivityConfig, Fullscreen, Inhibitor};

pub fn audio(objects: &Value, c: &ActivityConfig) -> Vec<Inhibitor> {
    let Some(objects) = objects.as_array() else {
        return vec![];
    };
    let mut linked = BTreeSet::new();
    for obj in objects {
        if obj["type"] == "PipeWire:Interface:Link" && obj["info"]["state"] == "active" {
            if let Some(n) = obj["info"]["input-node-id"].as_u64() {
                linked.insert(n);
            }
            if let Some(n) = obj["info"]["output-node-id"].as_u64() {
                linked.insert(n);
            }
        }
    }
    let mut result = Vec::new();
    for obj in objects {
        let info = &obj["info"];
        let p = &info["props"];
        if obj["type"] != "PipeWire:Interface:Node"
            || info["state"] != "running"
            || !linked.contains(&obj["id"].as_u64().unwrap_or(u64::MAX))
        {
            continue;
        }
        let class = p["media.class"].as_str().unwrap_or("");
        let recording = class == "Stream/Input/Audio";
        if !((class == "Stream/Output/Audio" && c.audio) || (recording && c.recording)) {
            continue;
        }
        if p["stream.monitor"] == true || p["stream.monitor"] == "true" {
            continue;
        }
        if [
            "application.name",
            "application.process.binary",
            "node.name",
        ]
        .iter()
        .any(|k| {
            p[*k]
                .as_str()
                .is_some_and(|v| c.ignore_audio.iter().any(|i| i == v))
        }) {
            continue;
        }
        let muted = info["params"]["Props"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v["mute"] == true));
        if muted && !recording {
            continue;
        }
        let name = p["application.name"]
            .as_str()
            .or(p["node.description"].as_str())
            .or(p["node.name"].as_str())
            .unwrap_or("Unknown audio app");
        result.push(Inhibitor {
            id: format!("pipewire:{}", obj["id"]),
            reason: format!(
                "{}: {name}",
                if recording { "Recording" } else { "Playback" }
            ),
            scope: if recording {
                c.recording_scope.clone()
            } else {
                c.audio_scope.clone()
            },
        });
    }
    result
}
pub fn fullscreen(windows: &Value, workspaces: &Value, outputs: &Value) -> Vec<Inhibitor> {
    let mut result = Vec::new();
    let Some(windows) = windows.as_array() else {
        return result;
    };
    for w in windows {
        let ws = workspaces.as_array().and_then(|all| {
            all.iter()
                .find(|ws| ws["id"] == w["workspace_id"] && ws["is_active"] == true)
        });
        let Some(ws) = ws else { continue };
        let Some(out) = ws["output"].as_str().and_then(|o| outputs.get(o)) else {
            continue;
        };
        let l = &w["layout"];
        let size = &l["window_size"];
        let pos = &l["tile_pos_in_workspace_view"];
        // Niri snapshot IPC can omit positions even for the active fullscreen window.
        let at_origin = pos[0].as_f64().is_some_and(|x| x.abs() < 1.0)
            && pos[1].as_f64().is_some_and(|y| y.abs() < 1.0);
        let active_without_position =
            pos.is_null() && w["id"].is_u64() && ws["active_window_id"] == w["id"];
        let geometry = size[0].as_f64().is_some()
            && size[1].as_f64().is_some()
            && size[0].as_f64() == out["logical"]["width"].as_f64()
            && size[1].as_f64() == out["logical"]["height"].as_f64()
            && (at_origin || active_without_position);
        if w["is_fullscreen"].as_bool().unwrap_or(geometry) {
            result.push(Inhibitor {
                id: format!("fullscreen:{}", w["id"]),
                reason: format!(
                    "Fullscreen geometry: {}",
                    w["app_id"].as_str().unwrap_or("Unknown app")
                ),
                scope: ACTIONS.to_vec(),
            });
        }
    }
    result
}
async fn json(args: &[&str]) -> Result<Value, String> {
    serde_json::from_str(&actions::run(args, 3).await?).map_err(|e| e.to_string())
}
pub async fn gather(c: &ActivityConfig) -> (Vec<Inhibitor>, BTreeMap<String, String>) {
    let mut all = Vec::new();
    let mut health = BTreeMap::new();
    if c.audio || c.recording {
        match json(&["pw-dump"]).await {
            Ok(v) => all.extend(audio(&v, c)),
            Err(e) => {
                health.insert("pipewire".into(), e);
                all.push(Inhibitor {
                    id: "degraded:pipewire".into(),
                    reason: "Audio state unavailable".into(),
                    scope: vec![Action::Suspend],
                });
            }
        }
    }
    if c.media {
        match actions::run(
            &[
                "playerctl",
                "--all-players",
                "--format",
                "{{playerName}}\t{{status}}",
                "metadata",
            ],
            3,
        )
        .await
        {
            Ok(lines) => {
                for line in lines.lines() {
                    if let Some((name, "Playing")) = line.split_once('\t') {
                        all.push(Inhibitor {
                            id: format!("mpris:{name}"),
                            reason: format!("Media playing: {name}"),
                            scope: c.media_scope.clone(),
                        });
                    }
                }
            }
            Err(e) if e.contains("No players found") => {}
            Err(e) => {
                health.insert("mpris".into(), e);
            }
        }
    }
    if c.fullscreen == Fullscreen::Geometry {
        let (w, s, o) = tokio::join!(
            json(&["niri", "msg", "--json", "windows"]),
            json(&["niri", "msg", "--json", "workspaces"]),
            json(&["niri", "msg", "--json", "outputs"])
        );
        match (w, s, o) {
            (Ok(w), Ok(s), Ok(o)) => {
                all.extend(fullscreen(&w, &s, &o));
                health.insert(
                    "fullscreen".into(),
                    "Geometry approximation enabled; no authoritative fullscreen flag".into(),
                );
            }
            _ => {
                health.insert("fullscreen".into(), "Niri state unavailable".into());
            }
        }
    }
    (all, health)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn stream(state: &str, name: &str, muted: bool) -> Value {
        json!([{"id":1,"type":"PipeWire:Interface:Node","info":{"state":state,"props":{"media.class":"Stream/Output/Audio","application.name":name},"params":{"Props":[{"mute":muted}]}}},{"type":"PipeWire:Interface:Link","info":{"state":"active","input-node-id":2,"output-node-id":1}}])
    }
    #[test]
    fn stale_dummy_paused_muted_streams_do_not_inhibit() {
        let c = ActivityConfig::default();
        assert!(audio(&stream("suspended", "browser", false), &c).is_empty());
        assert!(audio(&stream("running", "speech-dispatcher-dummy", false), &c).is_empty());
        assert!(audio(&stream("running", "browser", true), &c).is_empty());
        assert_eq!(audio(&stream("running", "browser", false), &c).len(), 1);
    }
    #[test]
    fn requires_active_link() {
        let mut v = stream("running", "browser", false);
        v[1]["info"]["state"] = json!("paused");
        assert!(audio(&v, &ActivityConfig::default()).is_empty());
    }
    #[test]
    fn fullscreen_must_be_visible_and_output_sized() {
        let w = json!([{"id":1,"workspace_id":2,"app_id":"game","layout":{"window_size":[1920,1080],"tile_pos_in_workspace_view":[0.0,0.0]}}]);
        let mut s = json!([{"id":2,"is_active":false,"output":"DP-1"}]);
        let o = json!({"DP-1":{"logical":{"width":1920,"height":1080}}});
        assert!(fullscreen(&w, &s, &o).is_empty());
        s[0]["is_active"] = json!(true);
        assert_eq!(fullscreen(&w, &s, &o).len(), 1);
    }
    #[test]
    fn missing_position_requires_active_window_and_valid_dimensions() {
        let mut w = json!([{"id":7,"workspace_id":2,"layout":{"window_size":[1920,1080],"tile_pos_in_workspace_view":null}}]);
        let mut s = json!([{"id":2,"is_active":true,"output":"DP-1","active_window_id":8}]);
        let o = json!({"DP-1":{"logical":{"width":1920,"height":1080}}});
        assert!(fullscreen(&w, &s, &o).is_empty());
        s[0]["active_window_id"] = json!(7);
        assert_eq!(fullscreen(&w, &s, &o).len(), 1);
        w[0]["layout"]["window_size"] = json!(null);
        assert!(fullscreen(&w, &s, &o).is_empty());
    }
}
