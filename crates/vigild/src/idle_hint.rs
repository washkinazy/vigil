//! Opt-in logind IdleHint ownership, separate from the policy event loop.
use crate::Event;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use zbus::{Connection, Proxy, zvariant::OwnedObjectPath};

trait Backend {
    async fn set(&mut self, idle: bool) -> Result<(), String>;
}
#[derive(Default)]
struct Logind {
    connection: Option<(Connection, OwnedObjectPath)>,
}
impl Backend for Logind {
    async fn set(&mut self, idle: bool) -> Result<(), String> {
        let result = async {
            if self.connection.is_none() {
                let c = zbus::connection::Builder::system()
                    .map_err(|e| e.to_string())?
                    .method_timeout(Duration::from_secs(2))
                    .build()
                    .await
                    .map_err(|e| e.to_string())?;
                let manager = Proxy::new(
                    &c,
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                )
                .await
                .map_err(|e| e.to_string())?;
                let path = manager
                    .call("GetSession", &("auto",))
                    .await
                    .map_err(|e| e.to_string())?;
                self.connection = Some((c, path));
            }
            let (c, path) = self
                .connection
                .as_ref()
                .ok_or("logind connection unavailable")?;
            let session = Proxy::new(
                c,
                "org.freedesktop.login1",
                path.as_str(),
                "org.freedesktop.login1.Session",
            )
            .await
            .map_err(|e| e.to_string())?;
            session
                .call::<_, _, ()>("SetIdleHint", &(idle,))
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        if result.is_err() {
            self.connection = None;
        }
        result
    }
}
async fn run(
    tx: mpsc::UnboundedSender<Event>,
    mut desired: watch::Receiver<Option<bool>>,
    dry: bool,
    mut backend: impl Backend,
) {
    let mut applied = None;
    let mut touched = false;
    let mut known = true;
    loop {
        let next = *desired.borrow_and_update();
        if !dry && (next != applied || !known) {
            touched = true;
            match backend.set(next.unwrap_or(false)).await {
                Ok(()) => {
                    applied = next;
                    known = true;
                    let _ = tx.send(Event::IdleHint(applied));
                    let _ = tx.send(Event::Health("idle-hint".into(), None));
                }
                Err(e) => {
                    known = false;
                    let _ = tx.send(Event::IdleHint(None));
                    let _ = tx.send(Event::Health("idle-hint".into(), Some(e)));
                }
            }
        }
        tokio::select! {
            changed = desired.changed() => if changed.is_err() { break; },
            _ = tokio::time::sleep(Duration::from_secs(2)), if (next != applied || !known) && !dry => {},
        }
    }
    if touched && let Err(e) = backend.set(false).await {
        eprintln!("idle-hint cleanup: {e}");
    }
}
pub fn start(
    tx: mpsc::UnboundedSender<Event>,
    desired: watch::Receiver<Option<bool>>,
    dry: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(tx, desired, dry, Logind::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake(mpsc::UnboundedSender<bool>);
    impl Backend for Fake {
        async fn set(&mut self, idle: bool) -> Result<(), String> {
            self.0.send(idle).map_err(|e| e.to_string())
        }
    }
    #[tokio::test]
    async fn disabled_and_dry_run_never_write() {
        for (desired, dry) in [(None, false), (Some(true), true)] {
            let (updates, rx) = watch::channel(desired);
            let (calls, mut received) = mpsc::unbounded_channel();
            let (events, _) = mpsc::unbounded_channel();
            drop(updates);
            run(events, rx, dry, Fake(calls)).await;
            assert!(received.try_recv().is_err());
        }
    }
    #[tokio::test]
    async fn idle_activity_disable_and_shutdown_clear_owned_hint() {
        let (updates, rx) = watch::channel(Some(true));
        let (calls, mut received) = mpsc::unbounded_channel();
        let (events, _) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(events, rx, false, Fake(calls)));
        assert_eq!(received.recv().await, Some(true));
        updates.send_replace(Some(false));
        assert_eq!(received.recv().await, Some(false));
        updates.send_replace(None);
        assert_eq!(received.recv().await, Some(false));
        updates.send_replace(Some(true));
        assert_eq!(received.recv().await, Some(true));
        drop(updates);
        task.await.unwrap();
        assert_eq!(received.recv().await, Some(false));
    }
}
