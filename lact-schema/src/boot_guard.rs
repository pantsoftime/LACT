//! Boot guard (this fork): keep a tuned profile from being re-applied after
//! the system stopped uncleanly with it active.
//!
//! The daemon writes an *armed* marker before it applies settings and removes
//! it on a clean shutdown. A marker found at startup means the last session
//! ended in a crash, hang or power loss, so the daemon *engages*: it applies
//! the fallback (stock, or a named profile) instead of the saved profile and
//! suspends automatic profile switching until the user resumes.

use serde::{Deserialize, Serialize};

/// Saved settings, part of the daemon config.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BootGuardConfig {
    /// Arm the marker whenever settings are applied.
    pub enabled: bool,
    /// Profile to apply when engaged; `None` is stock (nothing applied).
    pub fallback: Option<String>,
    /// One-shot: engage at the next daemon start regardless of the marker.
    pub force_next_boot: bool,
}

/// Why the guard engaged.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BootGuardTrip {
    /// The profile that was active when the marker was written (`None` = the
    /// default profile).
    pub profile: Option<String>,
    /// The marker came from this same kernel boot: the daemon itself died or
    /// was killed, not the whole system.
    pub same_boot: bool,
    /// Engaged by the one-shot flag, not by a marker.
    pub forced: bool,
    /// What was applied instead (`None` = stock).
    pub applied_fallback: Option<String>,
    /// Unix seconds when the marker was written and when the guard engaged.
    pub armed_at: u64,
    pub engaged_at: u64,
    /// The user has seen the trip; the fallback stays until they resume or
    /// the next boot.
    pub acknowledged: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct BootGuardStatus {
    pub config: BootGuardConfig,
    /// The marker is currently on disk.
    pub armed: bool,
    pub engaged: Option<BootGuardTrip>,
    /// The daemon could not use its state directory; the guard cannot work.
    pub error: Option<String>,
}
