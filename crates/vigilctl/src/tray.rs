//! StatusNotifierItem client. The daemon owns every timer and policy decision.
use ksni::{
    TrayMethods,
    menu::{CheckmarkItem, StandardItem, SubMenu},
};
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use vigil_core::{Request, Status, markup};

struct Indicator {
    status: Option<Status>,
    error: Option<String>,
    pending: bool,
    commands: mpsc::Sender<Request>,
}
impl Indicator {
    fn disabled(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.manual_inhibitors.iter().any(|m| m.id == "manual:toggle"))
    }
    fn available(&self) -> bool {
        self.status.is_some() && !self.pending
    }
    fn send(&mut self, command: Request) {
        if self.available() {
            match self.commands.try_send(command) {
                Ok(()) => self.pending = true,
                Err(_) => self.error = Some("Tray command queue unavailable".into()),
            }
        }
    }
    fn label(&self) -> String {
        let Some(status) = &self.status else {
            return "Vigil — Unavailable".into();
        };
        let state = match status
            .manual_inhibitors
            .iter()
            .find(|m| m.id == "manual:toggle")
        {
            Some(manual) => manual.remaining_seconds.map_or_else(
                || "Disabled".into(),
                |n| format!("Disabled — {} min", n.div_ceil(60)),
            ),
            None => "Enabled".into(),
        };
        format!(
            "Vigil — {state}{}",
            if status.dry_run { " (dry-run)" } else { "" }
        )
    }
}
impl ksni::Tray for Indicator {
    fn id(&self) -> String {
        "vigil".into()
    }
    fn title(&self) -> String {
        self.label()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Request::Toggle);
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![icon(self.disabled(), self.status.is_none())]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.label(),
            description: self.error.as_deref().map(markup).unwrap_or_default(),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            CheckmarkItem {
                label: "Enabled".into(),
                checked: self.status.is_some() && !self.disabled(),
                enabled: self.available(),
                activate: Box::new(|tray: &mut Self| tray.send(Request::Toggle)),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Disable for".into(),
                enabled: self.available(),
                submenu: [30, 60, 90]
                    .into_iter()
                    .map(|minutes| {
                        StandardItem {
                            label: format!("{minutes} minutes"),
                            activate: Box::new(move |tray: &mut Self| {
                                tray.send(Request::SetToggle {
                                    seconds: Some(minutes * 60),
                                })
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
        ]
    }
}

// Embedded line-art avoids dependencies on an installed icon theme or font.
fn icon(disabled: bool, unavailable: bool) -> ksni::Icon {
    let rows: [u16; 16] = if unavailable {
        [
            0, 0x0180, 0x03c0, 0x0660, 0x0c30, 0x1998, 0x319c, 0x619e, 0x619e, 0xc183, 0xc003,
            0xc183, 0xffff, 0, 0, 0,
        ]
    } else if disabled {
        [
            0, 0x1200, 0x0900, 0x1200, 0, 0x3ff8, 0x200e, 0x2009, 0x200e, 0x2008, 0x1010, 0x0fe0,
            0, 0, 0x3ffc, 0,
        ]
    } else {
        [
            0, 0x07e0, 0x1818, 0x2004, 0x4182, 0x4182, 0x8181, 0x8181, 0x81f1, 0x8001, 0x4002,
            0x4002, 0x2004, 0x1818, 0x07e0, 0,
        ]
    };
    let rgb = if unavailable {
        [235, 90, 90]
    } else if disabled {
        [235, 190, 95]
    } else {
        [216, 222, 233]
    };
    let mut data = Vec::with_capacity(16 * 16 * 4);
    for row in rows {
        for x in 0..16 {
            let pixel = if row & (1 << (15 - x)) != 0 {
                [255, rgb[0], rgb[1], rgb[2]]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&pixel);
        }
    }
    ksni::Icon {
        width: 16,
        height: 16,
        data,
    }
}

pub async fn run(path: PathBuf) -> Result<(), String> {
    let executable = crate::watcher::Executable::current()?;
    let (commands, mut received) = mpsc::channel(8);
    let handle = Indicator {
        status: None,
        error: None,
        pending: false,
        commands,
    }
    .assume_sni_available(true)
    .spawn()
    .await
    .map_err(|e| format!("start tray: {e}"))?;
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    loop {
        let command = tokio::select! {
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            command = received.recv() => match command { Some(c) => c, None => break },
            _ = interval.tick() => {
                if executable.replaced() { break; }
                if handle.is_closed() { return Err("tray connection closed".into()); }
                Request::Status
            }
        };
        let response = crate::request(&path, command).await;
        handle
            .update(move |tray| {
                tray.pending = false;
                match response {
                    Ok(response) => {
                        tray.status = response.status;
                        tray.error = None;
                    }
                    Err(error) => {
                        tray.status = None;
                        tray.error = Some(error);
                    }
                }
            })
            .await;
    }
    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray;
    fn indicator() -> (Indicator, mpsc::Receiver<Request>) {
        let (commands, received) = mpsc::channel(8);
        (
            Indicator {
                status: Some(crate::tests::status()),
                error: None,
                pending: false,
                commands,
            },
            received,
        )
    }
    #[tokio::test]
    async fn activation_and_timed_menu_send_daemon_commands_without_optimism() {
        let (mut tray, mut received) = indicator();
        tray.activate(0, 0);
        assert!(matches!(received.recv().await, Some(Request::Toggle)));
        assert!(tray.pending);
        assert!(!tray.disabled());
        tray.pending = false;
        let menu = tray.menu();
        let ksni::MenuItem::SubMenu(submenu) = &menu[1] else {
            panic!("missing duration submenu")
        };
        for (item, seconds) in submenu.submenu.iter().zip([1800, 3600, 5400]) {
            tray.pending = false;
            let ksni::MenuItem::Standard(item) = item else {
                panic!("missing duration")
            };
            (item.activate)(&mut tray);
            assert!(
                matches!(received.recv().await, Some(Request::SetToggle { seconds: Some(n) }) if n == seconds)
            );
        }
    }
    #[test]
    fn unavailable_controls_are_disabled_and_other_inhibitors_leave_enabled_checked() {
        let (mut tray, mut received) = indicator();
        tray.status
            .as_mut()
            .unwrap()
            .inhibitors
            .push(vigil_core::Inhibitor {
                id: "audio".into(),
                reason: "Music".into(),
                scope: vigil_core::ACTIONS.to_vec(),
            });
        assert_eq!(tray.label(), "Vigil — Enabled");
        let menu = tray.menu();
        let ksni::MenuItem::Checkmark(item) = &menu[0] else {
            panic!("missing Enabled")
        };
        assert!(item.checked);
        tray.status = None;
        tray.activate(0, 0);
        assert!(received.try_recv().is_err());
        assert!(!tray.available());
        assert_eq!(tray.label(), "Vigil — Unavailable");
    }
}
