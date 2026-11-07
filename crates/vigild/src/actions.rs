use std::time::Duration;
use tokio::{
    process::Command,
    time::{Instant, sleep, timeout},
};
use vigil_core::Commands;

pub async fn output(argv: &[String], seconds: u64) -> Result<String, String> {
    let (exe, args) = argv.split_first().ok_or("empty command")?;
    let mut cmd = Command::new(exe);
    cmd.args(args).kill_on_drop(true);
    let out = timeout(Duration::from_secs(seconds), cmd.output())
        .await
        .map_err(|_| format!("{exe}: timed out"))?
        .map_err(|e| format!("{exe}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{exe}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}
pub async fn run(args: &[&str], seconds: u64) -> Result<String, String> {
    output(
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        seconds,
    )
    .await
}

pub async fn lock(c: &Commands, limit: u64) -> Result<(), String> {
    let kind = run(
        &[
            "systemctl",
            "--user",
            "show",
            "--property=Type",
            "--value",
            &c.lock_unit,
        ],
        2,
    )
    .await?;
    if kind.trim() != "notify" {
        return Err("locker must use Type=notify to confirm lock completion".into());
    }
    run(
        &["systemctl", "--user", "start", "--no-block", &c.lock_unit],
        2,
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(limit);
    loop {
        let state = run(
            &[
                "systemctl",
                "--user",
                "show",
                "--property=ActiveState",
                "--value",
                &c.lock_unit,
            ],
            2,
        )
        .await?;
        if state.trim() == "active" {
            return Ok(());
        }
        if state.trim() == "failed" || Instant::now() >= deadline {
            return Err(format!(
                "locker did not confirm readiness (state {})",
                state.trim()
            ));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn subprocess_failure_and_timeout() {
        assert!(run(&["false"], 1).await.is_err());
        assert!(
            run(&["sleep", "5"], 1)
                .await
                .unwrap_err()
                .contains("timed out")
        );
    }
}
