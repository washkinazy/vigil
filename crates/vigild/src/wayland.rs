use crate::Event;
use std::{thread, time::Duration};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};
struct State {
    tx: UnboundedSender<Event>,
    notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1>,
    seat: Option<wl_seat::WlSeat>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "ext_idle_notifier_v1" if version >= 2 => s.notifier = Some(r.bind(name, 2, q, ())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(r.bind(name, 1, q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, bool> for State {
    fn event(
        s: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        e: ext_idle_notification_v1::Event,
        input: &bool,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match e {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        let _ = s.tx.send(if *input {
            Event::InputIdle(idle)
        } else {
            Event::EffectiveIdle(idle)
        });
    }
}
delegate_noop!(State:ignore wl_seat::WlSeat);
delegate_noop!(State:ignore ext_idle_notifier_v1::ExtIdleNotifierV1);
fn listen(tx: UnboundedSender<Event>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue();
    let q = queue.handle();
    let mut state = State {
        tx: tx.clone(),
        notifier: None,
        seat: None,
    };
    conn.display().get_registry(&q, ());
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let notifier = state
        .notifier
        .as_ref()
        .ok_or("ext-idle-notify v2 is required for input-idle and compositor inhibition")?;
    let seat = state.seat.as_ref().ok_or("no Wayland seat")?;
    let _input = notifier.get_input_idle_notification(1000, seat, &q, true);
    let _effective = notifier.get_idle_notification(1000, seat, &q, false);
    let _ = tx.send(Event::InputIdle(false));
    let _ = tx.send(Event::EffectiveIdle(false));
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let _ = tx.send(Event::Health("wayland".into(), None));
    loop {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| e.to_string())?;
    }
}
pub fn start(tx: UnboundedSender<Event>) {
    thread::spawn(move || {
        loop {
            if let Err(e) = listen(tx.clone())
                && tx.send(Event::Health("wayland".into(), Some(e))).is_err()
            {
                break;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}
