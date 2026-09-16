//! Boot guard state on disk (this fork). See `lact_schema::boot_guard`.
//!
//! Two files under `/var/lib/lact/boot_guard/`:
//!
//! * `armed.json` — written (and fsynced) *before* settings are applied,
//!   removed on a clean shutdown. Carries the profile name and the kernel
//!   boot id, so a marker from the same boot is recognised as a daemon
//!   restart rather than a system crash.
//! * `engaged.json` — the trip record, kept until the user resumes so a
//!   daemon restart while engaged stays engaged.
//!
//! While engaged the daemon also drops a note into `/run/motd.d/`, which
//! `pam_motd` shows on tty / SSH logins and the shell snippets in `res/`
//! print in interactive terminals; the note is removed when the trip is
//! acknowledged or resumed.

use anyhow::Context;
use lact_schema::boot_guard::BootGuardTrip;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};

const STATE_DIR: &str = "/var/lib/lact/boot_guard";
const ARMED: &str = "armed.json";
const ENGAGED: &str = "engaged.json";
const MOTD_DIR: &str = "/run/motd.d";
const MOTD_FILE: &str = "lact-boot-guard";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct Marker {
    profile: Option<String>,
    boot_id: String,
    armed_at: u64,
}

/// What the daemon should do at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// Apply the saved configuration as usual.
    Normal,
    /// Apply the fallback instead; `trip` is not yet complete
    /// (`applied_fallback` / `engaged_at` are filled in by the handler).
    Engage(BootGuardTrip),
    /// A previous trip is still unresolved.
    StillEngaged(BootGuardTrip),
}

pub struct BootGuardStore {
    dir: PathBuf,
    motd: PathBuf,
}

impl Default for BootGuardStore {
    fn default() -> Self {
        Self::new(Path::new(STATE_DIR), Path::new(MOTD_DIR))
    }
}

impl BootGuardStore {
    pub fn new(dir: &Path, motd_dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            motd: motd_dir.join(MOTD_FILE),
        }
    }

    fn armed_path(&self) -> PathBuf {
        self.dir.join(ARMED)
    }

    fn engaged_path(&self) -> PathBuf {
        self.dir.join(ENGAGED)
    }

    pub fn is_armed(&self) -> bool {
        self.armed_path().exists()
    }

    /// Decide what to do at daemon start. `force` is the one-shot flag from
    /// the config; the caller clears it after this returns.
    pub fn startup(&self, force: bool) -> anyhow::Result<Startup> {
        if let Some(trip) = self.read_engaged()? {
            return Ok(Startup::StillEngaged(trip));
        }
        let marker = self.read_armed()?;
        let now = now_secs();
        let boot_id = current_boot_id();
        match (marker, force) {
            (Some(marker), _) => Ok(Startup::Engage(BootGuardTrip {
                profile: marker.profile,
                same_boot: boot_id.as_deref() == Some(marker.boot_id.as_str()),
                forced: false,
                applied_fallback: None,
                armed_at: marker.armed_at,
                engaged_at: now,
                acknowledged: false,
            })),
            (None, true) => Ok(Startup::Engage(BootGuardTrip {
                profile: None,
                same_boot: false,
                forced: true,
                applied_fallback: None,
                armed_at: now,
                engaged_at: now,
                acknowledged: false,
            })),
            (None, false) => Ok(Startup::Normal),
        }
    }

    /// Write the marker durably. Must complete before the first hardware
    /// write it is meant to cover.
    pub fn arm(&self, profile: Option<&str>) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("Could not create {}", self.dir.display()))?;
        let marker = Marker {
            profile: profile.map(str::to_owned),
            boot_id: current_boot_id().unwrap_or_default(),
            armed_at: now_secs(),
        };
        write_durable(&self.armed_path(), &serde_json::to_vec_pretty(&marker)?)
    }

    pub fn disarm(&self) -> anyhow::Result<()> {
        remove_if_present(&self.armed_path())
    }

    /// Record a trip and post the login notice.
    pub fn engage(&self, trip: &BootGuardTrip) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_durable(&self.engaged_path(), &serde_json::to_vec_pretty(trip)?)?;
        // The marker has done its job; a clean shutdown from here must not
        // trip again, and neither must a crash while on the fallback.
        self.disarm()?;
        if let Err(err) = self.write_motd(trip) {
            warn!("could not write the boot-guard login notice: {err:#}");
        }
        Ok(())
    }

    /// Keep the trip on record but drop the notice.
    pub fn acknowledge(&self, trip: &BootGuardTrip) -> anyhow::Result<()> {
        write_durable(&self.engaged_path(), &serde_json::to_vec_pretty(trip)?)?;
        remove_if_present(&self.motd)
    }

    /// The user resumed: forget the trip.
    pub fn clear(&self) -> anyhow::Result<()> {
        remove_if_present(&self.engaged_path())?;
        remove_if_present(&self.motd)
    }

    fn read_armed(&self) -> anyhow::Result<Option<Marker>> {
        read_json(&self.armed_path())
    }

    fn read_engaged(&self) -> anyhow::Result<Option<BootGuardTrip>> {
        read_json(&self.engaged_path())
    }

    fn write_motd(&self, trip: &BootGuardTrip) -> anyhow::Result<()> {
        let dir = self.motd.parent().unwrap();
        fs::create_dir_all(dir)?;
        fs::write(&self.motd, motd_text(trip))?;
        Ok(())
    }
}

pub fn motd_text(trip: &BootGuardTrip) -> String {
    let what = if trip.forced {
        "the one-shot fallback flag was set".to_owned()
    } else {
        format!(
            "the system {} while profile '{}' was active",
            if trip.same_boot {
                "lost the LACT daemon"
            } else {
                "stopped uncleanly"
            },
            trip.profile.as_deref().unwrap_or("default")
        )
    };
    let running = trip
        .applied_fallback
        .as_deref()
        .map_or("STOCK settings (nothing applied)".to_owned(), |p| format!("fallback profile '{p}'"));
    format!(
        "\n*** LACT BOOT GUARD ENGAGED ***\n{what}.\nThe GPU is running {running}.\nOpen LACT > Advanced to resume the saved profile, or leave it until the next boot.\n\n"
    )
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("Could not parse {}", path.display()))?;
            Ok(Some(value))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Could not read {}", path.display())),
    }
}

/// Write via a temp file, fsync it, rename, then fsync the directory: after
/// this returns the marker survives a crash at any later instant.
fn write_durable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().context("marker path has no parent")?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp).with_context(|| format!("Could not create {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(dir) = path.parent()
                && let Ok(dir) = File::open(dir)
            {
                let _ = dir.sync_all();
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Could not remove {}", path.display())),
    }
}

fn current_boot_id() -> Option<String> {
    fs::read_to_string(BOOT_ID).ok().map(|s| s.trim().to_owned())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn log_trip(trip: &BootGuardTrip) {
    info!("{}", motd_text(trip).trim());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BootGuardStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = BootGuardStore::new(&tmp.path().join("state"), &tmp.path().join("motd.d"));
        (tmp, store)
    }

    #[test]
    fn clean_cycle_does_not_trip() {
        let (_tmp, store) = store();
        assert_eq!(store.startup(false).unwrap(), Startup::Normal);
        store.arm(Some("daily")).unwrap();
        assert!(store.is_armed());
        store.disarm().unwrap();
        assert_eq!(store.startup(false).unwrap(), Startup::Normal);
    }

    #[test]
    fn leftover_marker_trips_with_profile_and_same_boot() {
        let (_tmp, store) = store();
        store.arm(Some("bench")).unwrap();
        let Startup::Engage(trip) = store.startup(false).unwrap() else {
            panic!("expected a trip");
        };
        assert_eq!(trip.profile.as_deref(), Some("bench"));
        // The marker was written in this process, so it carries this boot id
        // (or an empty one where /proc is unavailable — then not same_boot).
        assert_eq!(trip.same_boot, current_boot_id().is_some());
        assert!(!trip.forced);
        assert!(!trip.acknowledged);
    }

    #[test]
    fn force_flag_trips_without_marker_and_engage_disarms() {
        let (_tmp, store) = store();
        let Startup::Engage(mut trip) = store.startup(true).unwrap() else {
            panic!("expected a forced trip");
        };
        assert!(trip.forced);
        trip.applied_fallback = None;
        store.arm(None).unwrap();
        store.engage(&trip).unwrap();
        assert!(!store.is_armed());
        assert!(store.motd.exists());
        assert!(fs::read_to_string(&store.motd).unwrap().contains("STOCK"));
        // A restart while engaged stays engaged.
        assert_eq!(store.startup(false).unwrap(), Startup::StillEngaged(trip.clone()));
        trip.acknowledged = true;
        store.acknowledge(&trip).unwrap();
        assert!(!store.motd.exists());
        assert_eq!(store.startup(false).unwrap(), Startup::StillEngaged(trip));
        store.clear().unwrap();
        assert_eq!(store.startup(false).unwrap(), Startup::Normal);
    }

    #[test]
    fn corrupt_marker_is_an_error_not_a_silent_pass() {
        let (_tmp, store) = store();
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(store.armed_path(), b"{not json").unwrap();
        assert!(store.startup(false).is_err());
    }
}
