use crate::{Event, actions};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::{
    sync::{mpsc::UnboundedSender, watch},
    time::{sleep, timeout},
};
use vigil_core::{ACTIONS, Config, Inhibitor};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedFd, OwnedObjectPath},
};

async fn manager(c: &Connection) -> Result<Proxy<'_>, String> {
    Proxy::new(
        c,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .map_err(|e| e.to_string())
}
pub async fn inhibitors(c: &Connection) -> Result<Vec<Inhibitor>, String> {
    let m = manager(c).await?;
    let list: Vec<(String, String, String, String, u32, u32)> = m
        .call("ListInhibitors", &())
        .await
        .map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .filter(|(what, _, _, mode, _, _)| {
            what.split(':').any(|w| w == "idle") && mode.starts_with("block")
        })
        .map(|(_, who, why, _, uid, pid)| Inhibitor {
            id: format!("logind:{uid}:{pid}:{who}"),
            reason: format!("{who}: {why}"),
            scope: ACTIONS.to_vec(),
        })
        .collect())
}
async fn delay(m: &Proxy<'_>) -> Result<OwnedFd, String> {
    m.call(
        "Inhibit",
        &("sleep", "Vigil", "Lock session before sleep", "delay"),
    )
    .await
    .map_err(|e| e.to_string())
}
pub fn start(tx: UnboundedSender<Event>, config: watch::Receiver<(u64, Config)>, dry: bool) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = monitor(tx.clone(), config.clone(), dry).await {
                let _ = tx.send(Event::Health("logind".into(), Some(e)));
            }
            sleep(Duration::from_secs(2)).await;
        }
    });
}
async fn monitor(
    tx: UnboundedSender<Event>,
    config: watch::Receiver<(u64, Config)>,
    dry: bool,
) -> Result<(), String> {
    let c = Connection::system().await.map_err(|e| e.to_string())?;
    let m = manager(&c).await?;
    let path: OwnedObjectPath = m
        .call("GetSession", &("auto",))
        .await
        .map_err(|e| e.to_string())?;
    let session = Proxy::new(
        &c,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut sleeping = m
        .receive_signal("PrepareForSleep")
        .await
        .map_err(|e| e.to_string())?;
    let mut locking = session
        .receive_signal("Lock")
        .await
        .map_err(|e| e.to_string())?;
    let mut unlocking = session
        .receive_signal("Unlock")
        .await
        .map_err(|e| e.to_string())?;
    let max_us: u64 = m
        .get_property("InhibitDelayMaxUSec")
        .await
        .map_err(|e| e.to_string())?;
    let budget = Duration::from_micros(max_us.saturating_sub(500_000).max(100_000));
    let mut fd = if dry { None } else { Some(delay(&m).await?) };
    let _ = tx.send(Event::Health("logind".into(), None));
    loop {
        tokio::select! {
         event=sleeping.next()=>{
          let message=event.ok_or("logind sleep signal stream ended")?;
          let (preparing,):(bool,)=message.body().deserialize().map_err(|e|e.to_string())?;
          let _=tx.send(Event::Sleep(preparing));
          if preparing {
           if !dry {
            let commands = config.borrow().1.commands.clone();
            let result=timeout(budget,actions::lock(&commands,commands.lock_timeout_seconds)).await;
            let error=match result {Ok(Ok(()))=>None,Ok(Err(e))=>Some(e),Err(_)=>Some("external sleep lock deadline expired; logind cannot be delayed indefinitely".into())};
            let _=tx.send(Event::Health("sleep-lock".into(),error));
           }
           fd.take();
          } else if !dry {fd=Some(delay(&m).await?);}
         },
         event=locking.next()=>{event.ok_or("logind lock signal stream ended")?;let _=tx.send(Event::LockRequested);},
         event=unlocking.next()=>{event.ok_or("logind unlock signal stream ended")?;let _=tx.send(Event::Health("unlock".into(),Some("Unlock requested externally; authenticate through the locker".into())));}
        }
    }
}
