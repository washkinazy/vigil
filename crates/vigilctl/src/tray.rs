//! StatusNotifierItem client. The daemon owns every timer and policy decision.
use ksni::{
    TrayMethods,
    menu::{CheckmarkItem, StandardItem},
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
        self.status.as_ref().is_some_and(|s| s.caffeine.is_some())
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
        let state = crate::label(status);
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
        icons(
            self.disabled()
                || self
                    .status
                    .as_ref()
                    .is_some_and(|s| !s.inhibitors.is_empty()),
            self.status.is_none(),
        )
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.label(),
            description: self
                .error
                .as_deref()
                .map(markup)
                .unwrap_or_else(|| self.status.as_ref().map(crate::tooltip).unwrap_or_default()),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut menu = vec![
            CheckmarkItem {
                label: "Keep awake".into(),
                checked: self.disabled(),
                enabled: self.available(),
                activate: Box::new(|tray: &mut Self| tray.send(Request::Toggle)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
        ];
        menu.extend([30, 60, 90].into_iter().map(|minutes| {
            StandardItem {
                label: format!("Keep awake for {minutes} minutes"),
                enabled: self.available(),
                activate: Box::new(move |tray: &mut Self| {
                    tray.send(Request::KeepAwake {
                        seconds: Some(minutes * 60),
                    })
                }),
                ..Default::default()
            }
            .into()
        }));
        menu
    }
}

// Pre-rendered SVGs provide antialiased icons at native panel sizes without runtime dependencies.
fn icons(disabled: bool, unavailable: bool) -> Vec<ksni::Icon> {
    macro_rules! images {
        ($name:literal) => {
            [
                (
                    16,
                    include_bytes!(concat!("../../../assets/tray/", $name, "-16.argb")).as_slice(),
                ),
                (
                    22,
                    include_bytes!(concat!("../../../assets/tray/", $name, "-22.argb")).as_slice(),
                ),
                (
                    24,
                    include_bytes!(concat!("../../../assets/tray/", $name, "-24.argb")).as_slice(),
                ),
                (
                    32,
                    include_bytes!(concat!("../../../assets/tray/", $name, "-32.argb")).as_slice(),
                ),
                (
                    48,
                    include_bytes!(concat!("../../../assets/tray/", $name, "-48.argb")).as_slice(),
                ),
            ]
        };
    }
    let images = if unavailable {
        images!("unavailable")
    } else if disabled {
        images!("disabled")
    } else {
        images!("enabled")
    };
    images
        .into_iter()
        .map(|(size, data)| ksni::Icon {
            width: size,
            height: size,
            data: data.to_vec(),
        })
        .collect()
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
        assert!(
            !menu
                .iter()
                .any(|item| matches!(item, ksni::MenuItem::SubMenu(_)))
        );
        for (item, seconds) in menu[2..].iter().zip([1800, 3600, 5400]) {
            tray.pending = false;
            let ksni::MenuItem::Standard(item) = item else {
                panic!("missing duration")
            };
            (item.activate)(&mut tray);
            assert!(
                matches!(received.recv().await, Some(Request::KeepAwake { seconds: Some(n) }) if n == seconds)
            );
        }
    }
    #[test]
    fn icons_have_native_sizes_and_antialiased_edges() {
        for (disabled, unavailable) in [(false, false), (true, false), (false, true)] {
            let icons = icons(disabled, unavailable);
            assert_eq!(
                icons.iter().map(|i| i.width).collect::<Vec<_>>(),
                [16, 22, 24, 32, 48]
            );
            for icon in icons {
                assert_eq!(icon.width, icon.height);
                assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
                assert!(
                    icon.data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|p| p[0] > 0 && p[0] < 255)
                );
            }
        }
    }
    #[test]
    fn application_pause_is_visible_without_enabling_caffeine() {
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
        assert_eq!(tray.label(), "Vigil — Paused: Music");
        assert_eq!(tray.icon_pixmap()[0].data, icons(true, false)[0].data);
        assert!(tray.tool_tip().description.contains("Music"));
        let menu = tray.menu();
        let ksni::MenuItem::Checkmark(item) = &menu[0] else {
            panic!("missing Keep awake")
        };
        assert!(!item.checked);
        tray.status = None;
        tray.activate(0, 0);
        assert!(received.try_recv().is_err());
        assert!(!tray.available());
        assert_eq!(tray.label(), "Vigil — Unavailable");
    }
}
