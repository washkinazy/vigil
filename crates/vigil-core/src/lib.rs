//! Deterministic idle policy, configuration, and local control protocol.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Display,
    Lock,
    Suspend,
}
pub const ACTIONS: [Action; 3] = [Action::Display, Action::Lock, Action::Suspend];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub timeouts: Timeouts,
    pub activity: ActivityConfig,
    pub commands: Commands,
    pub session: SessionConfig,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Zero disables logind IdleHint export.
    pub idle_hint_seconds: u64,
}
impl SessionConfig {
    pub fn idle_hint(&self, elapsed: u64, observed: bool, inhibited: bool) -> Option<bool> {
        (self.idle_hint_seconds > 0)
            .then_some(observed && !inhibited && elapsed >= self.idle_hint_seconds)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub display_seconds: u64,
    pub lock_seconds: u64,
    pub suspend_seconds: u64,
    pub release_grace_seconds: u64,
    pub retry_seconds: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityConfig {
    pub poll_seconds: u64,
    pub audio: bool,
    pub recording: bool,
    pub media: bool,
    pub fullscreen: Fullscreen,
    pub audio_scope: Vec<Action>,
    pub recording_scope: Vec<Action>,
    pub media_scope: Vec<Action>,
    pub ignore_audio: Vec<String>,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fullscreen {
    Disabled,
    Geometry,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Commands {
    pub lock_unit: String,
    pub lock_timeout_seconds: u64,
    pub display_off: Vec<String>,
    pub display_on: Vec<String>,
    pub suspend: Vec<String>,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            display_seconds: 900,
            lock_seconds: 1200,
            suspend_seconds: 1800,
            release_grace_seconds: 60,
            retry_seconds: 30,
        }
    }
}
impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            poll_seconds: 2,
            audio: true,
            recording: true,
            media: true,
            fullscreen: Fullscreen::Disabled,
            audio_scope: vec![Action::Suspend],
            recording_scope: vec![Action::Suspend],
            media_scope: ACTIONS.to_vec(),
            ignore_audio: vec!["speech-dispatcher-dummy".into(), "sd_dummy".into()],
        }
    }
}
impl Default for Commands {
    fn default() -> Self {
        Self {
            lock_unit: "wm-lock.service".into(),
            lock_timeout_seconds: 10,
            display_off: vec![
                "niri".into(),
                "msg".into(),
                "action".into(),
                "power-off-monitors".into(),
            ],
            display_on: vec![
                "niri".into(),
                "msg".into(),
                "action".into(),
                "power-on-monitors".into(),
            ],
            suspend: vec!["systemctl".into(), "suspend".into()],
        }
    }
}
impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let c: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=60).contains(&self.activity.poll_seconds) {
            return Err("activity.poll_seconds must be 1..60".into());
        }
        if !(1..=60).contains(&self.commands.lock_timeout_seconds) {
            return Err("commands.lock_timeout_seconds must be 1..60".into());
        }
        if self.timeouts.retry_seconds == 0 || self.timeouts.release_grace_seconds == 0 {
            return Err("retry and release grace must be positive".into());
        }
        if self.commands.lock_unit.is_empty()
            || self.commands.lock_unit.starts_with('-')
            || !self.commands.lock_unit.ends_with(".service")
        {
            return Err("lock_unit must name a systemd notify service".into());
        }
        for argv in [
            &self.commands.display_on,
            &self.commands.display_off,
            &self.commands.suspend,
        ] {
            if argv.is_empty() || argv[0].is_empty() {
                return Err("action commands must be nonempty argument arrays".into());
            }
        }
        Ok(())
    }
    pub fn timeout(&self, a: Action) -> u64 {
        match a {
            Action::Display => self.timeouts.display_seconds,
            Action::Lock => self.timeouts.lock_seconds,
            Action::Suspend => self.timeouts.suspend_seconds,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Inhibitor {
    pub id: String,
    pub reason: String,
    pub scope: Vec<Action>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timer {
    pub action: Action,
    pub enabled: bool,
    pub blocked: bool,
    pub completed: bool,
    pub remaining_seconds: u64,
    pub delay: Option<TimerDelay>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimerDelay {
    ReleaseGrace,
    Retry,
}
#[derive(Debug, Clone)]
struct ActionState {
    completed: bool,
    blocked: bool,
    not_before: u64,
    delay: Option<TimerDelay>,
}
#[derive(Debug, Clone)]
pub struct Policy {
    last_activity: u64,
    states: BTreeMap<Action, ActionState>,
}
impl Policy {
    pub fn new(now: u64) -> Self {
        Self {
            last_activity: now,
            states: ACTIONS
                .into_iter()
                .map(|a| {
                    (
                        a,
                        ActionState {
                            completed: false,
                            blocked: false,
                            not_before: now,
                            delay: None,
                        },
                    )
                })
                .collect(),
        }
    }
    pub fn activity(&mut self, now: u64) {
        self.last_activity = now;
        for s in self.states.values_mut() {
            s.completed = false;
            s.not_before = now;
            s.delay = None;
        }
    }
    pub fn elapsed(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_activity)
    }
    pub fn timers(&mut self, now: u64, c: &Config, inhibitors: &[Inhibitor]) -> Vec<Timer> {
        ACTIONS
            .into_iter()
            .map(|a| {
                let s = self.states.get_mut(&a).expect("all action states exist");
                let blocked = inhibitors.iter().any(|i| i.scope.contains(&a));
                if s.blocked && !blocked {
                    s.not_before = now.saturating_add(c.timeouts.release_grace_seconds);
                    s.delay = Some(TimerDelay::ReleaseGrace);
                }
                if blocked {
                    s.completed = false;
                }
                s.blocked = blocked;
                let deadline = self
                    .last_activity
                    .saturating_add(c.timeout(a))
                    .max(s.not_before);
                Timer {
                    action: a,
                    enabled: c.timeout(a) > 0,
                    blocked,
                    completed: s.completed,
                    remaining_seconds: deadline.saturating_sub(now),
                    delay: if s.not_before > now
                        && s.not_before >= self.last_activity.saturating_add(c.timeout(a))
                    {
                        s.delay
                    } else {
                        None
                    },
                }
            })
            .collect()
    }
    pub fn complete(&mut self, a: Action) {
        self.states.get_mut(&a).expect("action exists").completed = true;
    }
    pub fn failed(&mut self, a: Action, now: u64, c: &Config) {
        let state = self.states.get_mut(&a).expect("action exists");
        state.not_before = now.saturating_add(c.timeouts.retry_seconds);
        state.delay = Some(TimerDelay::Retry);
    }
}

/// Manual inhibitors with deadlines on the daemon's monotonic clock.
#[derive(Debug, Default, Clone)]
pub struct ManualInhibitors {
    next_id: u64,
    entries: BTreeMap<String, (Option<u64>, String, Vec<Action>)>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualStatus {
    pub id: String,
    pub reason: String,
    pub scope: Vec<Action>,
    pub remaining_seconds: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualSnapshot {
    version: u32,
    next_id: u64,
    entries: Vec<SavedManual>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedManual {
    id: String,
    reason: String,
    scope: Vec<Action>,
    expires_at: Option<u64>,
}
impl ManualSnapshot {
    /// Existing IDs retain their original absolute expiry, regardless of clock adjustments.
    pub fn preserve_expirations(&mut self, previous: &Self) {
        let deadlines: BTreeMap<_, _> = previous
            .entries
            .iter()
            .map(|m| (&m.id, m.expires_at))
            .collect();
        for entry in &mut self.entries {
            if let Some(expiry) = deadlines.get(&entry.id) {
                entry.expires_at = *expiry;
            }
        }
    }
}
impl ManualInhibitors {
    pub fn snapshot(&self, now: u64, unix_now: u64) -> Result<ManualSnapshot, String> {
        let entries = self
            .status(now)
            .into_iter()
            .map(|m| {
                Ok(SavedManual {
                    id: m.id,
                    reason: m.reason,
                    scope: m.scope,
                    expires_at: m
                        .remaining_seconds
                        .map(|n| unix_now.checked_add(n).ok_or("expiration overflow"))
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(ManualSnapshot {
            version: 1,
            next_id: self.next_id,
            entries,
        })
    }
    pub fn restore(snapshot: ManualSnapshot, now: u64, unix_now: u64) -> Result<Self, String> {
        if snapshot.version != 1 {
            return Err("unsupported manual state version".into());
        }
        let mut result = Self {
            next_id: snapshot.next_id,
            entries: BTreeMap::new(),
        };
        for saved in snapshot.entries {
            let valid_id = saved.id == "manual:toggle"
                || saved
                    .id
                    .strip_prefix("manual:")
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|n| {
                        n > 0 && n <= result.next_id && saved.id == format!("manual:{n}")
                    });
            if !valid_id
                || saved.scope.is_empty()
                || saved.reason.trim().is_empty()
                || saved.reason.len() > 256
                || saved.reason.chars().any(char::is_control)
                || (saved.id == "manual:toggle"
                    && (saved.expires_at.is_some() || saved.scope != ACTIONS))
            {
                return Err("invalid saved manual inhibitor".into());
            }
            if result.entries.contains_key(&saved.id) {
                return Err("duplicate saved manual inhibitor".into());
            }
            let until = match saved.expires_at {
                Some(expiry) if expiry <= unix_now => continue,
                Some(expiry) => Some(
                    now.checked_add(expiry - unix_now)
                        .ok_or("expiration overflow")?,
                ),
                None => None,
            };
            result
                .entries
                .insert(saved.id, (until, saved.reason, saved.scope));
        }
        Ok(result)
    }
    pub fn expire(&mut self, now: u64) {
        self.entries
            .retain(|_, (until, _, _)| until.is_none_or(|until| now < until));
    }
    pub fn add(
        &mut self,
        now: u64,
        seconds: Option<u64>,
        scope: Vec<Action>,
        reason: String,
    ) -> Result<String, String> {
        if scope.is_empty() || seconds == Some(0) {
            return Err("scope and duration must be nonempty/positive".into());
        }
        if reason.trim().is_empty() || reason.len() > 256 || reason.chars().any(char::is_control) {
            return Err("reason must be 1–256 bytes with no control characters".into());
        }
        let until = seconds
            .map(|s| now.checked_add(s).ok_or("duration is too large"))
            .transpose()?;
        let next = self
            .next_id
            .checked_add(1)
            .ok_or("manual inhibitor IDs exhausted")?;
        self.next_id = next;
        let id = format!("manual:{next}");
        self.entries.insert(id.clone(), (until, reason, scope));
        Ok(id)
    }
    pub fn release(&mut self, id: &str, now: u64) -> Result<(), String> {
        self.expire(now);
        self.entries
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| format!("No active manual inhibitor: {id}"))
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn toggle(&mut self) -> bool {
        if self.entries.remove("manual:toggle").is_some() {
            false
        } else {
            self.entries.insert(
                "manual:toggle".into(),
                (None, "Manual keep-awake (toggle)".into(), ACTIONS.to_vec()),
            );
            true
        }
    }
    pub fn status(&self, now: u64) -> Vec<ManualStatus> {
        self.entries
            .iter()
            .filter(|(_, (until, _, _))| until.is_none_or(|until| now < until))
            .map(|(id, (until, reason, scope))| ManualStatus {
                id: id.clone(),
                reason: reason.clone(),
                scope: scope.clone(),
                remaining_seconds: until.map(|until| until.saturating_sub(now)),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Status,
    Explain {
        action: Action,
    },
    KeepAwake {
        seconds: Option<u64>,
        scope: Vec<Action>,
        #[serde(default = "default_manual_reason")]
        reason: String,
    },
    Release {
        id: String,
    },
    Automatic,
    Toggle,
    Reload,
    Lock,
    Suspend,
    Wake,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub idle_hint: Option<bool>,
    pub dry_run: bool,
    pub idle_seconds: u64,
    pub inhibitors: Vec<Inhibitor>,
    pub timers: Vec<Timer>,
    pub health: BTreeMap<String, String>,
    pub last_action: Option<String>,
    pub manual_inhibitors: Vec<ManualStatus>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub status: Option<Status>,
}

fn default_manual_reason() -> String {
    "Manual keep-awake".into()
}

pub fn duration(value: &str) -> Result<u64, String> {
    let (digits, factor) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 3600),
        Some('d') => (&value[..value.len() - 1], 86400),
        _ => (value, 1),
    };
    let n = digits
        .parse::<u64>()
        .map_err(|_| "duration must be a positive integer with optional s/m/h/d suffix")?;
    n.checked_mul(factor)
        .filter(|n| *n > 0)
        .ok_or_else(|| "duration is zero or too large".into())
}
pub fn markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn blocker(id: &str, scope: Vec<Action>) -> Inhibitor {
        Inhibitor {
            id: id.into(),
            reason: id.into(),
            scope,
        }
    }
    fn due(t: &Timer) -> bool {
        t.enabled && !t.blocked && !t.completed && t.remaining_seconds == 0
    }
    #[test]
    fn timeouts_and_activity() {
        let c = Config::default();
        let mut p = Policy::new(100);
        assert!(!due(&p.timers(999, &c, &[])[0]));
        assert!(due(&p.timers(1000, &c, &[])[0]));
        p.complete(Action::Display);
        assert!(!due(&p.timers(1001, &c, &[])[0]));
        p.activity(1002);
        assert_eq!(p.timers(1002, &c, &[])[0].remaining_seconds, 900);
    }
    #[test]
    fn overlapping_inhibitors_and_release_grace() {
        let c = Config::default();
        let mut p = Policy::new(0);
        let a = blocker("audio", vec![Action::Suspend]);
        let b = blocker("manual", ACTIONS.to_vec());
        assert!(p.timers(2000, &c, &[a.clone(), b])[2].blocked);
        assert!(p.timers(2001, &c, &[a])[2].blocked);
        assert_eq!(p.timers(2002, &c, &[])[2].remaining_seconds, 60);
        assert!(!due(&p.timers(2061, &c, &[])[2]));
        assert!(due(&p.timers(2062, &c, &[])[2]));
    }
    #[test]
    fn action_scopes_and_failures() {
        let c = Config::default();
        let mut p = Policy::new(0);
        let t = p.timers(2000, &c, &[blocker("audio", vec![Action::Suspend])]);
        assert!(due(&t[0]) && due(&t[1]) && !due(&t[2]));
        p.failed(Action::Lock, 2000, &c);
        assert_eq!(p.timers(2001, &c, &[])[1].remaining_seconds, 29);
    }
    #[test]
    fn disabled_and_clock_regression() {
        let mut c = Config::default();
        c.timeouts.display_seconds = 0;
        let mut p = Policy::new(100);
        assert_eq!(p.elapsed(1), 0);
        assert!(!due(&p.timers(9999, &c, &[])[0]));
    }
    #[test]
    fn strict_config_and_duration() {
        assert!(Config::parse("[activity]\npoll_second = 1").is_err());
        assert!(Config::parse("[activity]\npoll_seconds = 0").is_err());
        assert!(Config::parse("[commands]\nsuspend = []").is_err());
        assert_eq!(duration("2h"), Ok(7200));
        assert!(duration("0").is_err());
        assert!(duration("18446744073709551615d").is_err());
    }
}

#[cfg(test)]
mod idle_hint_tests {
    use super::*;
    #[test]
    fn opt_in_hint_requires_observed_uninhibited_idle() {
        let mut c = SessionConfig::default();
        assert_eq!(c.idle_hint(1000, true, false), None);
        c.idle_hint_seconds = 60;
        assert_eq!(c.idle_hint(59, true, false), Some(false));
        assert_eq!(c.idle_hint(60, true, false), Some(true));
        assert_eq!(c.idle_hint(60, false, false), Some(false));
        assert_eq!(c.idle_hint(60, true, true), Some(false));
        assert_eq!(c.idle_hint(0, true, false), Some(false));
    }
}

#[cfg(test)]
mod manual_tests {
    use super::*;
    #[test]
    fn rejects_invalid_requests_without_replacing_existing_state() {
        let mut m = ManualInhibitors::default();
        let id = m
            .add(10, Some(20), vec![Action::Suspend], "Download".into())
            .unwrap();
        for reason in [
            "".to_string(),
            "  ".into(),
            "bad\nlabel".into(),
            "x".repeat(257),
        ] {
            assert!(m.add(10, None, ACTIONS.to_vec(), reason).is_err());
        }
        assert!(m.add(10, Some(0), ACTIONS.to_vec(), "Zero".into()).is_err());
        assert!(m.add(10, None, vec![], "Empty".into()).is_err());
        assert!(
            m.add(10, Some(u64::MAX), ACTIONS.to_vec(), "Overflow".into())
                .is_err()
        );
        let status = m.status(15);
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].id, id);
        assert_eq!(status[0].remaining_seconds, Some(15));
        m.expire(30);
        assert!(m.status(30).is_empty());
        let next = m.add(30, None, ACTIONS.to_vec(), "Next".into()).unwrap();
        assert_ne!(next, id);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn restart_preserves_ids_deadlines_toggle_and_counter() {
        let mut manual = ManualInhibitors::default();
        let timed = manual
            .add(10, Some(60), vec![Action::Suspend], "Backup".into())
            .unwrap();
        let indefinite = manual
            .add(10, None, ACTIONS.to_vec(), "Presentation".into())
            .unwrap();
        manual.toggle();
        let snapshot = manual.snapshot(20, 1000).unwrap();
        let mut restored = ManualInhibitors::restore(snapshot, 0, 1020).unwrap();
        let status = restored.status(0);
        assert_eq!(
            status
                .iter()
                .find(|m| m.id == timed)
                .unwrap()
                .remaining_seconds,
            Some(30)
        );
        assert!(status.iter().any(|m| m.id == indefinite));
        assert!(status.iter().any(|m| m.id == "manual:toggle"));
        // A second save/restart must not grant another 60 seconds.
        restored =
            ManualInhibitors::restore(restored.snapshot(10, 1030).unwrap(), 0, 1040).unwrap();
        assert_eq!(
            restored
                .status(0)
                .iter()
                .find(|m| m.id == timed)
                .unwrap()
                .remaining_seconds,
            Some(10)
        );
        restored.expire(10);
        assert!(!restored.status(10).iter().any(|m| m.id == timed));
        restored.clear();
        let mut restored =
            ManualInhibitors::restore(restored.snapshot(10, 1050).unwrap(), 0, 1100).unwrap();
        assert!(restored.status(0).is_empty());
        assert_eq!(
            restored
                .add(0, None, ACTIONS.to_vec(), "New".into())
                .unwrap(),
            "manual:3"
        );
    }
    #[test]
    fn saving_other_requests_does_not_move_existing_expirations() {
        let mut manual = ManualInhibitors::default();
        manual
            .add(0, Some(100), ACTIONS.to_vec(), "Original".into())
            .unwrap();
        let previous = manual.snapshot(0, 1000).unwrap();
        // Simulate suspend/clock movement before a new request is created.
        manual
            .add(10, Some(100), ACTIONS.to_vec(), "New".into())
            .unwrap();
        let mut next = manual.snapshot(10, 2000).unwrap();
        next.preserve_expirations(&previous);
        assert_eq!(
            next.entries
                .iter()
                .find(|m| m.id == "manual:1")
                .unwrap()
                .expires_at,
            Some(1100)
        );
        assert_eq!(
            next.entries
                .iter()
                .find(|m| m.id == "manual:2")
                .unwrap()
                .expires_at,
            Some(2100)
        );
    }
    #[test]
    fn expired_while_stopped_is_not_restored_and_invalid_state_is_rejected() {
        let mut manual = ManualInhibitors::default();
        manual
            .add(0, Some(10), ACTIONS.to_vec(), "Timed".into())
            .unwrap();
        let restored = ManualInhibitors::restore(manual.snapshot(0, 100).unwrap(), 0, 111).unwrap();
        assert!(restored.status(0).is_empty());
        let mut snapshot = manual.snapshot(0, 100).unwrap();
        snapshot.version = 99;
        assert!(ManualInhibitors::restore(snapshot, 0, 100).is_err());
        let mut snapshot = manual.snapshot(0, 100).unwrap();
        snapshot.entries[0].id = "audio:1".into();
        assert!(ManualInhibitors::restore(snapshot, 0, 100).is_err());
    }
}
