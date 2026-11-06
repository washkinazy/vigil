//! Session idle policy and state, independent of desktop integrations.
//!
//! Keep future policy evaluation deterministic: callers supply activity,
//! elapsed time, inhibitors, and action results. This crate must not access
//! the compositor, session buses, devices, or real clock directly.
//!
//! Policy implementation will follow the behavioral specification in README.md.

/// Automatic idle actions that policy will evaluate independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    /// Turn off session displays.
    BlankDisplays,
    /// Request session locking and await confirmation.
    LockSession,
    /// Request system suspension after lock confirmation.
    Suspend,
}
