mod tray;
mod watcher;
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};
use vigil_core::{Request, Response, Status, duration, markup};
async fn request(path: &PathBuf, r: Request) -> Result<Response, String> {
    timeout(Duration::from_secs(95), async {
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
    .map_err(|_| "Vigil did not respond within 95 seconds".to_string())?
}
fn label(s: &Status) -> String {
    if let Some(caffeine) = &s.caffeine {
        caffeine.remaining_seconds.map_or_else(
            || "Keep awake".into(),
            |n| format!("Keep awake — {} min", n.div_ceil(60)),
        )
    } else if let Some(reason) = s.inhibitors.first() {
        format!("Paused: {}", reason.reason)
    } else {
        "Automatic".into()
    }
}
fn tooltip(s: &Status) -> String {
    let mut lines = vec![format!(
        "Vigil — {}{}",
        label(s),
        if s.dry_run { " (dry-run)" } else { "" }
    )];
    lines.extend(s.inhibitors.iter().map(|i| i.reason.clone()));
    lines.extend(
        s.health
            .iter()
            .map(|(backend, error)| format!("{backend}: {error}")),
    );
    lines.dedup();
    markup(&lines.join("\n"))
}
fn waybar(s: &Status) -> serde_json::Value {
    let (icon, class) = if s.caffeine.is_some() {
        ("", "manual")
    } else if !s.inhibitors.is_empty() {
        ("", "inhibited")
    } else {
        ("", "automatic")
    };
    serde_json::json!({"text":icon,"class":class,"tooltip":tooltip(s)})
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
            "vigilctl [--socket PATH] COMMAND\n\nstatus [--json]\nexplain display|lock|suspend\nwaybar [--watch]\ntray\nkeep-awake [DURATION]\nautomatic\ntoggle\nreload\nlock\nsuspend\nwake\n\nDurations: 30s, 20m, 1h, 1d. Keep-awake replaces the current caffeine override.\nOmitted duration lasts until toggled off, automatic, or daemon restart.\nExplicit lock/suspend bypass activity inhibitors; suspend requires confirmed lock."
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
        "lock" if args.len() == 1 => Request::Lock,
        "suspend" if args.len() == 1 => Request::Suspend,
        "wake" if args.len() == 1 => Request::Wake,
        "toggle" if args.len() == 1 => Request::Toggle,
        "reload" if args.len() == 1 => Request::Reload,
        "keep-awake" if args.len() <= 2 => Request::KeepAwake {
            seconds: args.get(1).map(|s| duration(s)).transpose()?,
        },
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
                if let Some(caffeine) = &s.caffeine {
                    println!(
                        "  Keep awake: {}",
                        caffeine.remaining_seconds.map_or_else(
                            || "until disabled or restarted".into(),
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
    pub(super) fn status() -> Status {
        Status {
            idle_hint: None,
            dry_run: false,
            idle_seconds: 0,
            inhibitors: vec![],
            timers: vec![],
            health: Default::default(),
            last_action: None,
            caffeine: None,
        }
    }
    #[test]
    fn status_distinguishes_automatic_caffeine_and_application_pause() {
        let mut s = status();
        assert_eq!(waybar(&s)["class"], "automatic");
        s.inhibitors.push(vigil_core::Inhibitor {
            id: "media".into(),
            reason: "Music <&>".into(),
            scope: vigil_core::ACTIONS.to_vec(),
        });
        assert_eq!(label(&s), "Paused: Music <&>");
        assert!(tooltip(&s).contains("Music &lt;&amp;&gt;"));
        assert_eq!(waybar(&s)["class"], "inhibited");
        s.caffeine = Some(vigil_core::CaffeineStatus {
            remaining_seconds: Some(90),
        });
        assert_eq!(label(&s), "Keep awake — 2 min");
        assert_eq!(waybar(&s)["class"], "manual");
        s.health.insert("mpris".into(), "Disconnected <&>".into());
        assert!(tooltip(&s).contains("Disconnected &lt;&amp;&gt;"));
    }
}
