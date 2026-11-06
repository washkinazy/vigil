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
    for i in &s.inhibitors {
        lines.push(format!("{} — {:?}", i.reason, i.scope));
    }
    if let Some(n) = s.manual_remaining_seconds {
        lines.push(format!("Manual override expires in {n}s"));
    }
    for (name, error) in &s.health {
        lines.push(format!("{name}: {error}"));
    }
    for t in &s.timers {
        lines.push(format!(
            "{:?}: {}",
            t.action,
            if !t.enabled {
                "disabled".into()
            } else if t.blocked {
                "inhibited".into()
            } else if t.completed {
                "completed".into()
            } else {
                format!("{}s remaining", t.remaining_seconds)
            }
        ));
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
            "vigilctl [--socket PATH] COMMAND\n\nstatus [--json]\nwaybar [--watch]\nkeep-awake [DURATION] [--scope display,lock,suspend]\nautomatic\ntoggle\nlock\nsuspend\nwake\n\nDurations: 30s, 20m, 1h, 1d. Omitted duration lasts until automatic or restart.\nExplicit lock/suspend commands bypass activity inhibitors; suspend requires confirmed lock."
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
        "lock" if args.len() == 1 => Request::Lock,
        "suspend" if args.len() == 1 => Request::Suspend,
        "wake" if args.len() == 1 => Request::Wake,
        "toggle" if args.len() == 1 => Request::Status,
        "keep-awake" => {
            let mut seconds = None;
            let mut scope = ACTIONS.to_vec();
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
                } else if seconds.is_none() {
                    seconds = Some(duration(arg)?);
                } else {
                    return Err("unexpected argument".into());
                }
            }
            Request::KeepAwake { seconds, scope }
        }
        _ => return Err("unsupported command or arguments; see --help".into()),
    };
    let path = socket.unwrap_or(
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is required")?)
            .join("vigil/control.sock"),
    );
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
        let mut response = response?;
        if cmd == "toggle" {
            let active = response
                .status
                .as_ref()
                .is_some_and(|s| s.inhibitors.iter().any(|i| i.id == "manual"));
            response = request(
                &path,
                if active {
                    Request::Automatic
                } else {
                    Request::KeepAwake {
                        seconds: None,
                        scope: ACTIONS.to_vec(),
                    }
                },
            )
            .await?;
        }
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
                    println!("  {} [{:?}]", i.reason, i.scope);
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
            manual_remaining_seconds: None,
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
