mod tray;
mod watcher;
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};
use vigil_core::{ACTIONS, Request, Response, Status, duration, markup};
async fn request(path: &PathBuf, r: Request) -> Result<Response, String> {
    timeout(Duration::from_secs(35), async {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut data = serde_json::to_vec(&r).map_err(|e| e.to_string())?;
        data.push(b'\n');
        stream.write_all(&data).await.map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(stream.take(1_048_576))
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        let response: Response = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if !response.ok {
            return Err(response.message);
        }
        Ok(response)
    })
    .await
    .map_err(|_| "Vigil did not respond within 35 seconds".to_string())?
}
fn waybar(s: &Status) -> serde_json::Value {
    let toggled = s.manual_inhibitors.iter().any(|i| i.id == "manual:toggle");
    let (icon, class) = if toggled {
        ("", "manual")
    } else if !s.inhibitors.is_empty() {
        ("", "inhibited")
    } else {
        ("", "automatic")
    };
    let state = if toggled {
        "Keep awake"
    } else if s.inhibitors.is_empty() {
        "Automatic"
    } else {
        "Inhibited"
    };
    let mut lines = vec![format!(
        "Vigil — {state}{}",
        if s.dry_run { " (dry-run)" } else { "" }
    )];
    let mut reasons = std::collections::BTreeMap::new();
    for i in &s.inhibitors {
        if i.id == "compositor" || i.id == "manual:toggle" {
            continue;
        }
        let reason = if let Some(app) = i
            .reason
            .strip_prefix("Playback: ")
            .or_else(|| i.reason.strip_prefix("Media playing: "))
        {
            format!("Media: {app}")
        } else if let Some(app) = i.reason.strip_prefix("Fullscreen geometry: ") {
            format!("Fullscreen: {app}")
        } else {
            i.reason.clone()
        };
        reasons.entry(reason.to_lowercase()).or_insert(reason);
    }
    if reasons.is_empty() && s.inhibitors.iter().any(|i| i.id == "compositor") {
        reasons.insert("application".into(), "Application inhibition".into());
    }
    lines.extend(reasons.values().take(4).cloned());
    if reasons.len() > 4 {
        lines.push(format!("And {} more", reasons.len() - 4));
    }
    for manual in &s.manual_inhibitors {
        if let Some(n) = manual.remaining_seconds {
            lines.push(format!(
                "{}: {} min remaining",
                manual.reason,
                n.div_ceil(60)
            ));
        }
    }
    serde_json::json!({"text":icon,"class":class,"tooltip":markup(&lines.join("\n"))})
}
#[tokio::main]
async fn main() {
    if let Err(e) = start().await {
        eprintln!("vigilctl: {e}");
        std::process::exit(1);
    }
}
async fn start() -> Result<(), String> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut socket = None;
    if args.first().is_some_and(|s| s == "--socket") {
        if args.len() < 3 {
            return Err("--socket requires PATH and command".into());
        }
        socket = Some(PathBuf::from(args.remove(1)));
        args.remove(0);
    }
    if args == ["--help"] || args == ["-h"] {
        println!(
            "vigilctl [--socket PATH] COMMAND\n\nstatus [--json]\nexplain display|lock|suspend\nwaybar [--watch]\ntray\nkeep-awake [DURATION] [--scope display,lock,suspend] [--reason TEXT]\nrelease ID\nautomatic\ntoggle\nreload\nlock\nsuspend\nwake\n\nDurations: 30s, 20m, 1h, 1d. Each keep-awake prints its ID. Omitted duration lasts until released.\nrelease removes one request; automatic clears all manual requests.\ntoggle controls its own all-action request and preserves other requests.\nExplicit lock/suspend commands bypass activity inhibitors; suspend requires confirmed lock."
        );
        return Ok(());
    }
    if args == ["--version"] {
        println!("vigilctl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let cmd = args
        .first()
        .map(String::as_str)
        .ok_or("a command is required; see --help")?;
    let request_kind = match cmd {
        "status" | "waybar"
            if args.len() == 1
                || args.len() == 2
                    && args[1] == if cmd == "status" { "--json" } else { "--watch" } =>
        {
            Request::Status
        }
        "explain" if args.len() == 2 => Request::Explain {
            action: match args[1].as_str() {
                "display" => vigil_core::Action::Display,
                "lock" => vigil_core::Action::Lock,
                "suspend" => vigil_core::Action::Suspend,
                _ => return Err("explain requires display, lock, or suspend".into()),
            },
        },
        "tray" if args.len() == 1 => Request::Status,
        "automatic" if args.len() == 1 => Request::Automatic,
        "release" if args.len() == 2 => Request::Release {
            id: args[1].clone(),
        },
        "lock" if args.len() == 1 => Request::Lock,
        "suspend" if args.len() == 1 => Request::Suspend,
        "wake" if args.len() == 1 => Request::Wake,
        "toggle" if args.len() == 1 => Request::Toggle,
        "reload" if args.len() == 1 => Request::Reload,
        "keep-awake" => {
            let mut seconds = None;
            let mut scope = ACTIONS.to_vec();
            let mut reason = "Manual keep-awake".to_string();
            let mut tail = args.iter().skip(1);
            while let Some(arg) = tail.next() {
                if arg == "--scope" {
                    scope = tail
                        .next()
                        .ok_or("--scope requires comma separated actions")?
                        .split(',')
                        .map(|s| match s {
                            "display" => Ok(vigil_core::Action::Display),
                            "lock" => Ok(vigil_core::Action::Lock),
                            "suspend" => Ok(vigil_core::Action::Suspend),
                            _ => Err("scope must contain display,lock,suspend"),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                } else if arg == "--reason" {
                    reason = tail.next().ok_or("--reason requires text")?.clone();
                } else if seconds.is_none() {
                    seconds = Some(duration(arg)?);
                } else {
                    return Err("unexpected argument".into());
                }
            }
            Request::KeepAwake {
                seconds,
                scope,
                reason,
            }
        }
        _ => return Err("unsupported command or arguments; see --help".into()),
    };
    let path = match socket {
        Some(path) => path,
        None => {
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is required")?)
                .join("vigil/control.sock")
        }
    };
    if cmd == "tray" {
        return tray::run(path).await;
    }
    let watcher = if cmd == "waybar" && args.len() == 2 {
        Some(watcher::Executable::current()?)
    } else {
        None
    };
    loop {
        if watcher.as_ref().is_some_and(watcher::Executable::replaced) {
            return Ok(());
        }
        let response = request(&path, request_kind.clone()).await;
        if cmd == "waybar" {
            match response {
                Ok(r) => println!("{}", waybar(&r.status.ok_or("missing status")?)),
                Err(e) => println!(
                    "{}",
                    serde_json::json!({"text":"","class":"error","tooltip":markup(&format!("Vigil unavailable: {e}"))})
                ),
            };
            if args.len() == 2 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            return Ok(());
        }
        let response = response?;
        if cmd == "status" {
            let s = response.status.ok_or("missing status")?;
            if args.len() == 2 {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "Vigil{} — idle {}s",
                    if s.dry_run { " (dry-run)" } else { "" },
                    s.idle_seconds
                );
                for i in &s.inhibitors {
                    println!("  {}: {} [{:?}]", i.id, i.reason, i.scope);
                }
                for manual in &s.manual_inhibitors {
                    println!(
                        "  {}: {}",
                        manual.id,
                        manual
                            .remaining_seconds
                            .map_or_else(|| "until released".into(), |n| format!("{n}s remaining"))
                    );
                }
                for t in &s.timers {
                    println!(
                        "  {:?}: enabled={} inhibited={} completed={} remaining={}s",
                        t.action, t.enabled, t.blocked, t.completed, t.remaining_seconds
                    );
                }
                for (k, v) in s.health {
                    println!("  Warning: {k}: {v}");
                }
            }
        } else {
            println!("{}", response.message);
        }
        return Ok(());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn status() -> Status {
        Status {
            idle_hint: None,
            dry_run: false,
            idle_seconds: 0,
            inhibitors: vec![],
            timers: vec![],
            health: Default::default(),
            last_action: None,
            manual_inhibitors: vec![],
        }
    }
    #[test]
    fn automatic_button_stays_visible_with_concise_status() {
        let v = waybar(&status());
        assert!(!v["text"].as_str().unwrap().is_empty());
        assert_eq!(v["class"], "automatic");
        assert_eq!(v["tooltip"], "Vigil — Automatic");
    }
    #[test]
    fn only_dedicated_toggle_marks_button_manual() {
        let mut s = status();
        s.inhibitors.push(vigil_core::Inhibitor {
            id: "manual:1".into(),
            reason: "Download".into(),
            scope: vec![vigil_core::Action::Suspend],
        });
        s.manual_inhibitors.push(vigil_core::ManualStatus {
            id: "manual:1".into(),
            reason: "Download".into(),
            scope: vec![vigil_core::Action::Suspend],
            remaining_seconds: Some(90),
        });
        let v = waybar(&s);
        assert_eq!(v["class"], "inhibited");
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .starts_with("Vigil — Inhibited")
        );
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .contains("Download: 2 min remaining")
        );
        s.manual_inhibitors.push(vigil_core::ManualStatus {
            id: "manual:toggle".into(),
            reason: "Toggle".into(),
            scope: ACTIONS.to_vec(),
            remaining_seconds: None,
        });
        let v = waybar(&s);
        assert_eq!(v["class"], "manual");
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .starts_with("Vigil — Keep awake")
        );
        s.manual_inhibitors.pop();
        assert_eq!(waybar(&s)["class"], "inhibited");
    }
    #[test]
    fn application_inhibition_does_not_claim_toggle_is_on() {
        let mut s = status();
        s.inhibitors.push(vigil_core::Inhibitor {
            id: "compositor".into(),
            reason: "Application".into(),
            scope: ACTIONS.to_vec(),
        });
        let v = waybar(&s);
        assert_eq!(v["class"], "inhibited");
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .starts_with("Vigil — Inhibited")
        );
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .contains("Application inhibition")
        );
    }
    #[test]
    fn tooltip_escapes_external_labels() {
        let s = Status {
            idle_hint: None,
            dry_run: false,
            idle_seconds: 0,
            inhibitors: vec![vigil_core::Inhibitor {
                id: "x".into(),
                reason: "<player & app>".into(),
                scope: ACTIONS.to_vec(),
            }],
            timers: vec![],
            health: Default::default(),
            last_action: None,
            manual_inhibitors: vec![],
        };
        let v = waybar(&s);
        assert!(
            v["tooltip"]
                .as_str()
                .unwrap()
                .contains("&lt;player &amp; app&gt;")
        );
    }
}
