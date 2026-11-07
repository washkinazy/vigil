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
    // Accepted for older configurations; release now restarts the ordinary timeout.
    #[serde(rename = "release_grace_seconds", skip_serializing)]
    pub legacy_release_grace_seconds: Option<u64>,
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
    #[serde(rename = "ignore_audio", skip_serializing)]
    _legacy_ignore_audio: Vec<String>,
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
            legacy_release_grace_seconds: None,
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
            audio_scope: ACTIONS.to_vec(),
            recording_scope: ACTIONS.to_vec(),
            media_scope: ACTIONS.to_vec(),
            _legacy_ignore_audio: vec![],
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
        if self.timeouts.retry_seconds == 0 {
            return Err("retry must be positive".into());
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
                    s.not_before = now.saturating_add(c.timeout(a));
                    s.delay = None;
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

/// A single session-scoped caffeine override, using monotonic deadlines.
#[derive(Debug, Default, Clone)]
pub struct Caffeine {
    until: Option<Option<u64>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaffeineStatus {
    pub remaining_seconds: Option<u64>,
}
impl Caffeine {
    pub fn set(&mut self, now: u64, seconds: Option<u64>) -> Result<(), String> {
        if seconds == Some(0) {
            return Err("duration must be positive".into());
        }
        let until = seconds
            .map(|n| now.checked_add(n).ok_or("duration is too large"))
            .transpose()?;
        self.until = Some(until);
        Ok(())
    }
    pub fn clear(&mut self) {
        self.until = None;
    }
    pub fn toggle(&mut self, now: u64) {
        if self.status(now).is_some() {
            self.clear();
        } else {
            self.until = Some(None);
        }
    }
    pub fn status(&self, now: u64) -> Option<CaffeineStatus> {
        self.until
            .filter(|until| until.is_none_or(|until| now < until))
            .map(|until| CaffeineStatus {
                remaining_seconds: until.map(|until| until - now),
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Status,
    Explain { action: Action },
    KeepAwake { seconds: Option<u64> },
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
    pub caffeine: Option<CaffeineStatus>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub status: Option<Status>,
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
    fn overlapping_inhibitors_restart_only_released_timers() {
        let c = Config::default();
        let mut p = Policy::new(0);
        let a = blocker("audio", vec![Action::Suspend]);
        let b = blocker("manual", ACTIONS.to_vec());
        assert!(p.timers(2000, &c, &[a.clone(), b])[2].blocked);
        assert!(p.timers(2001, &c, &[a])[2].blocked);
        assert_eq!(p.timers(2002, &c, &[])[2].remaining_seconds, 1800);
        assert!(!due(&p.timers(3801, &c, &[])[2]));
        assert!(due(&p.timers(3802, &c, &[])[2]));
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
mod caffeine_tests {
    use super::*;
    #[test]
    fn replaces_deadline_expires_and_rejects_invalid_updates() {
        let mut c = Caffeine::default();
        c.set(10, Some(20)).unwrap();
        assert_eq!(c.status(15).unwrap().remaining_seconds, Some(15));
        c.set(15, Some(40)).unwrap();
        assert!(c.set(15, Some(0)).is_err());
        assert!(c.set(15, Some(u64::MAX)).is_err());
        assert_eq!(c.status(30).unwrap().remaining_seconds, Some(25));
        assert!(c.status(55).is_none());
        c.toggle(55);
        assert_eq!(c.status(100).unwrap().remaining_seconds, None);
        c.toggle(100);
        assert!(c.status(100).is_none());
    }
}
