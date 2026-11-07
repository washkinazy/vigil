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
    let mut lines = vec![if s.dry_run {
        "Vigil (dry-run)".into()
    } else {
        "Vigil".into()
    }];
    let mut reasons = std::collections::BTreeMap::new();
    for i in &s.inhibitors {
        if i.id == "compositor" {
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
        reasons.insert(
            "application".into(),
            "An application is keeping the session awake".into(),
        );
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
    serde_json::json!({"text":if s.inhibitors.is_empty(){""}else{""},"class":if s.inhibitors.is_empty(){"automatic"}else{"inhibited"},"tooltip":markup(&lines.join("\n"))})
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
            "vigilctl [--socket PATH] COMMAND\n\nstatus [--json]\nwaybar [--watch]\nkeep-awake [DURATION] [--scope display,lock,suspend] [--reason TEXT]\nrelease ID\nautomatic\ntoggle\nreload\nlock\nsuspend\nwake\n\nDurations: 30s, 20m, 1h, 1d. Each keep-awake prints its ID. Omitted duration lasts until released or restart.\nrelease removes one request; automatic clears all manual requests.\ntoggle controls its own all-action request and preserves other requests.\nExplicit lock/suspend commands bypass activity inhibitors; suspend requires confirmed lock."
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
    loop {
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
                        manual.remaining_seconds.map_or_else(
                            || "until released or restart".into(),
                            |n| format!("{n}s remaining")
                        )
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
