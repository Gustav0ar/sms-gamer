//! Persistent observed battery intervals. Gaps never imply continued battery use.
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};

use crate::api::{
    BatteryHistoryStatus, BatterySession, BatterySessionEnd, BatterySessionEndReason,
};

const RETAINED_SESSIONS: usize = 10_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    version: u8,
    tracked_since_ms: i64,
    next_id: i64,
    revision: i64,
    sessions: VecDeque<BatterySession>,
}

pub struct Observation {
    pub at_ms: i64,
    pub boot_ms: u64,
    pub on_battery: bool,
    pub charge_percent: f64,
    pub mode_verified: bool,
    pub max_interval_ms: u64,
}

pub struct Tracker {
    checkpoint: Checkpoint,
    previous: Option<Observation>,
    save_error: Option<String>,
}

pub enum History {
    Tracking(Tracker),
    Unavailable(String),
}

pub struct Store {
    path: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(path: PathBuf) -> Result<Self> {
        let path = std::path::absolute(path)?;
        fs::create_dir_all(
            path.parent()
                .context("History file needs a parent directory")?,
        )?;
        let lock = File::from(open(
            path.with_extension("lock"),
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?);
        flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .context("Battery history is already owned by another daemon")?;
        Ok(Self { path, _lock: lock })
    }

    pub fn load(&self, now_ms: i64) -> History {
        let result = (|| -> Result<Checkpoint> {
            let metadata = match fs::metadata(&self.path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Checkpoint {
                        version: 1,
                        tracked_since_ms: now_ms,
                        next_id: 1,
                        revision: 0,
                        sessions: VecDeque::new(),
                    });
                }
                Err(error) => return Err(error.into()),
            };
            ensure!(
                metadata.len() <= 8 * 1024 * 1024,
                "Battery history exceeds 8 MiB"
            );
            let checkpoint: Checkpoint = serde_json::from_slice(&fs::read(&self.path)?)
                .context("Invalid battery history file")?;
            ensure!(
                checkpoint.version == 1,
                "Unsupported battery history version"
            );
            ensure!(
                checkpoint.sessions.len() <= RETAINED_SESSIONS,
                "Too many battery sessions"
            );
            ensure!(
                checkpoint.tracked_since_ms >= 0
                    && checkpoint.tracked_since_ms <= now_ms + 300_000
                    && checkpoint.next_id > 0
                    && checkpoint.next_id < i64::MAX
                    && checkpoint.revision >= 0,
                "Invalid battery history metadata"
            );
            let mut previous_id = 0;
            for (index, session) in checkpoint.sessions.iter().enumerate() {
                ensure!(
                    session.id > previous_id && session.id < checkpoint.next_id,
                    "Invalid battery session IDs"
                );
                previous_id = session.id;
                ensure!(
                    session.started_at_ms >= 0
                        && session.started_at_ms <= session.last_seen_at_ms
                        && session.last_seen_at_ms <= now_ms + 300_000
                        && session.duration_ms >= 0
                        && session.start_charge_percent.is_finite()
                        && (0.0..=100.0).contains(&session.start_charge_percent)
                        && session.last_charge_percent.is_finite()
                        && (0.0..=100.0).contains(&session.last_charge_percent),
                    "Invalid battery session readings"
                );
                let through = if let Some(end) = &session.end {
                    ensure!(
                        end.time_unix_ms >= session.last_seen_at_ms
                            && end.time_unix_ms <= now_ms + 300_000,
                        "Invalid battery session end"
                    );
                    end.time_unix_ms
                } else {
                    ensure!(
                        index + 1 == checkpoint.sessions.len(),
                        "Only the latest battery session can be active"
                    );
                    session.last_seen_at_ms
                };
                // Small clock adjustments may differ from CLOCK_BOOTTIME by up
                // to 250 ms per sample. Larger jumps interrupt the session.
                ensure!(
                    session.duration_ms
                        <= (through - session.started_at_ms).saturating_mul(2) + 1000,
                    "Invalid battery session duration"
                );
            }
            Ok(checkpoint)
        })();
        match result {
            Ok(checkpoint) => {
                let mut history = History::Tracking(Tracker {
                    checkpoint,
                    previous: None,
                    save_error: None,
                });
                // Neither a new boot nor a restart proves the UPS stayed on battery.
                history.interrupt(BatterySessionEndReason::monitoring_restarted);
                history
            }
            Err(error) => History::Unavailable(format!("{}: {error:#}", self.path.display())),
        }
    }

    pub fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        let parent = self.path.parent().expect("absolute battery history path");
        let bytes = serde_json::to_vec_pretty(checkpoint)?;
        if fs::read(&self.path).ok().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

impl Tracker {
    fn active_mut(&mut self) -> Option<&mut BatterySession> {
        self.checkpoint
            .sessions
            .back_mut()
            .filter(|session| session.end.is_none())
    }

    fn finish(&mut self, reason: BatterySessionEndReason, at_ms: Option<i64>) {
        if let Some(session) = self.active_mut() {
            session.end = Some(BatterySessionEnd {
                reason,
                time_unix_ms: at_ms.unwrap_or(session.last_seen_at_ms),
            });
            self.checkpoint.revision += 1;
        }
    }
}

impl History {
    pub fn interrupt(&mut self, reason: BatterySessionEndReason) {
        if let Self::Tracking(tracker) = self {
            tracker.finish(reason, None);
            tracker.previous = None;
        }
    }

    pub fn observe(&mut self, reading: Observation) {
        let Self::Tracking(tracker) = self else {
            return;
        };
        if tracker
            .previous
            .as_ref()
            .is_some_and(|old| old.at_ms == reading.at_ms && old.boot_ms == reading.boot_ms)
        {
            return;
        }
        let elapsed = tracker.previous.as_ref().and_then(|old| {
            let wall = u64::try_from(reading.at_ms - old.at_ms).ok()?;
            let boot = reading.boot_ms.checked_sub(old.boot_ms)?;
            (wall > 0
                && wall <= old.max_interval_ms.min(reading.max_interval_ms)
                && boot <= old.max_interval_ms.min(reading.max_interval_ms)
                && wall.abs_diff(boot) <= 250)
                .then_some(boot)
        });
        if elapsed.is_none() && tracker.previous.is_some() {
            let clock_changed = tracker.previous.as_ref().is_some_and(|old| {
                reading.at_ms <= old.at_ms
                    || reading.boot_ms < old.boot_ms
                    || (reading.at_ms - old.at_ms).abs_diff((reading.boot_ms - old.boot_ms) as i64)
                        > 250
            });
            tracker.finish(
                if clock_changed {
                    BatterySessionEndReason::clock_changed
                } else {
                    BatterySessionEndReason::telemetry_lost
                },
                None,
            );
        }
        if let Some(session) = tracker.active_mut() {
            session.duration_ms += elapsed.unwrap_or(0) as i64;
            if reading.on_battery {
                session.last_seen_at_ms = reading.at_ms;
                session.last_charge_percent = reading.charge_percent;
                session.mode_verified &= reading.mode_verified;
            } else {
                tracker.finish(BatterySessionEndReason::mains_restored, Some(reading.at_ms));
            }
        } else if reading.on_battery {
            let start_observed =
                elapsed.is_some() && tracker.previous.as_ref().is_some_and(|old| !old.on_battery);
            tracker.checkpoint.sessions.push_back(BatterySession {
                id: tracker.checkpoint.next_id,
                started_at_ms: reading.at_ms,
                last_seen_at_ms: reading.at_ms,
                duration_ms: 0,
                start_charge_percent: reading.charge_percent,
                last_charge_percent: reading.charge_percent,
                start_observed,
                mode_verified: reading.mode_verified,
                end: None,
            });
            tracker.checkpoint.next_id += 1;
            tracker.checkpoint.revision += 1;
            while tracker.checkpoint.sessions.len() > RETAINED_SESSIONS {
                tracker.checkpoint.sessions.pop_front();
            }
        }
        tracker.previous = Some(reading);
    }

    pub fn tick(&mut self, now_ms: i64, boot_ms: u64) -> bool {
        let Self::Tracking(tracker) = self else {
            return false;
        };
        let expired = tracker.previous.as_ref().is_some_and(|old| {
            now_ms < old.at_ms
                || boot_ms < old.boot_ms
                || now_ms - old.at_ms > old.max_interval_ms as i64
                || boot_ms - old.boot_ms > old.max_interval_ms
        });
        if !expired {
            return false;
        }
        let revision = tracker.checkpoint.revision;
        let clock_changed = tracker
            .previous
            .as_ref()
            .is_some_and(|old| now_ms < old.at_ms || boot_ms < old.boot_ms);
        tracker.finish(
            if clock_changed {
                BatterySessionEndReason::clock_changed
            } else {
                BatterySessionEndReason::telemetry_lost
            },
            None,
        );
        tracker.previous = None;
        tracker.checkpoint.revision != revision
    }

    pub fn status(&self) -> BatteryHistoryStatus {
        match self {
            Self::Tracking(tracker) => BatteryHistoryStatus {
                tracked_since_ms: Some(tracker.checkpoint.tracked_since_ms),
                active_session: tracker
                    .checkpoint
                    .sessions
                    .back()
                    .filter(|session| session.end.is_none())
                    .cloned(),
                retained_sessions: tracker.checkpoint.sessions.len() as i64,
                revision: tracker.checkpoint.revision,
                last_error: tracker.save_error.clone(),
            },
            Self::Unavailable(error) => BatteryHistoryStatus {
                tracked_since_ms: None,
                active_session: None,
                retained_sessions: 0,
                revision: 0,
                last_error: Some(error.clone()),
            },
        }
    }

    pub fn page(&self, limit: usize, before_id: Option<i64>) -> (Vec<BatterySession>, bool) {
        let Self::Tracking(tracker) = self else {
            return (Vec::new(), false);
        };
        let mut entries = tracker
            .checkpoint
            .sessions
            .iter()
            .rev()
            .filter(|session| before_id.is_none_or(|id| session.id < id));
        let page = entries.by_ref().take(limit).cloned().collect();
        (page, entries.next().is_some())
    }

    pub fn checkpoint(&self) -> Option<Checkpoint> {
        match self {
            Self::Tracking(tracker) => Some(tracker.checkpoint.clone()),
            Self::Unavailable(_) => None,
        }
    }

    pub fn save_error(&mut self, error: Option<String>) -> bool {
        if let Self::Tracking(tracker) = self {
            if tracker.save_error != error {
                tracker.save_error = error;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_790_000_000_000;

    fn reading(seconds: u64, battery: bool, charge: f64) -> Observation {
        Observation {
            at_ms: START + seconds as i64 * 1000,
            boot_ms: seconds * 1000,
            on_battery: battery,
            charge_percent: charge,
            mode_verified: true,
            max_interval_ms: 4100,
        }
    }

    fn fresh() -> History {
        History::Tracking(Tracker {
            checkpoint: Checkpoint {
                version: 1,
                tracked_since_ms: START,
                next_id: 1,
                revision: 0,
                sessions: VecDeque::new(),
            },
            previous: None,
            save_error: None,
        })
    }

    #[test]
    fn sessions_record_transitions_duration_charge_and_verification_without_duplicates() {
        let mut history = fresh();
        history.observe(reading(0, false, 100.0));
        history.observe(reading(2, true, 100.0));
        history.observe(reading(4, true, 98.0));
        history.observe(reading(4, true, 98.0));
        let active = history.status().active_session.unwrap();
        assert_eq!(active.duration_ms, 2000);
        assert!(active.start_observed && active.mode_verified);
        history.observe(Observation {
            mode_verified: false,
            ..reading(6, true, 95.0)
        });
        history.observe(reading(8, false, 96.0));
        let (sessions, more) = history.page(100, None);
        assert!(!more);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.started_at_ms, START + 2000);
        assert_eq!(session.last_seen_at_ms, START + 6000);
        assert_eq!(session.duration_ms, 6000);
        assert_eq!(session.start_charge_percent, 100.0);
        assert_eq!(session.last_charge_percent, 95.0);
        assert_eq!(
            session.end.as_ref().unwrap().reason,
            BatterySessionEndReason::mains_restored
        );
        assert_eq!(session.end.as_ref().unwrap().time_unix_ms, START + 8000);
        assert!(!session.mode_verified);
        assert!(history.status().active_session.is_none());
    }

    #[test]
    fn gaps_retries_and_suspend_split_sessions_without_inventing_battery_time() {
        let mut history = fresh();
        history.observe(reading(0, true, 100.0));
        history.observe(reading(2, true, 99.0));
        history.interrupt(BatterySessionEndReason::telemetry_lost);
        history.interrupt(BatterySessionEndReason::telemetry_lost);
        history.observe(reading(3, true, 99.0));
        history.observe(reading(5, true, 98.0));
        assert!(history.tick(START + 30_000, 30_000));
        assert!(!history.tick(START + 31_000, 31_000));
        history.observe(reading(32, true, 90.0));
        history.observe(reading(34, false, 90.0));
        let (sessions, _) = history.page(100, None);
        assert_eq!(sessions.len(), 3);
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.duration_ms)
                .sum::<i64>(),
            6000
        );
        assert!(sessions.iter().all(|session| !session.start_observed));
        for session in &sessions[1..] {
            assert_eq!(
                session.end.as_ref().unwrap().reason,
                BatterySessionEndReason::telemetry_lost
            );
            assert_eq!(
                session.end.as_ref().unwrap().time_unix_ms,
                session.last_seen_at_ms
            );
        }
    }

    #[test]
    fn clock_jumps_and_device_changes_preserve_old_intervals() {
        let mut history = fresh();
        history.observe(reading(0, true, 100.0));
        history.observe(reading(2, true, 99.0));
        history.observe(Observation {
            at_ms: START + 3_600_000,
            ..reading(4, true, 98.0)
        });
        history.interrupt(BatterySessionEndReason::device_changed);
        history.observe(reading(6, false, 100.0));
        let (sessions, _) = history.page(100, None);
        assert_eq!(
            sessions[0].end.as_ref().unwrap().reason,
            BatterySessionEndReason::device_changed
        );
        assert_eq!(
            sessions[1].end.as_ref().unwrap().reason,
            BatterySessionEndReason::clock_changed
        );
        assert_eq!(sessions[1].duration_ms, 2000);
        assert_eq!(sessions[0].duration_ms, 0);
    }

    #[test]
    fn saved_history_survives_restarts_and_preserves_corrupt_files_and_writer_locks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("battery-history.json");
        let store = Store::open(path.clone()).unwrap();
        assert!(Store::open(path.clone()).is_err());
        let mut history = store.load(START);
        history.observe(reading(0, true, 100.0));
        history.observe(reading(2, true, 99.0));
        store.save(&history.checkpoint().unwrap()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        let mut resumed = store.load(START + 60_000);
        assert!(resumed.status().active_session.is_none());
        resumed.observe(reading(60, true, 80.0));
        let (sessions, _) = resumed.page(100, None);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[1].duration_ms, 2000);
        assert_eq!(
            sessions[1].end.as_ref().unwrap().reason,
            BatterySessionEndReason::monitoring_restarted
        );
        assert!(!sessions[0].start_observed);
        resumed.interrupt(BatterySessionEndReason::daemon_stopped);
        store.save(&resumed.checkpoint().unwrap()).unwrap();
        assert_eq!(
            store.load(START + 70_000).page(100, None).0[0]
                .end
                .as_ref()
                .unwrap()
                .reason,
            BatterySessionEndReason::daemon_stopped
        );
        let mut invalid = resumed.checkpoint().unwrap();
        invalid.sessions[0].duration_ms = i64::MAX;
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let unavailable = store.load(START + 70_000);
        assert!(unavailable.status().last_error.is_some());
        assert!(unavailable.checkpoint().is_none());
        fs::write(&path, "broken history").unwrap();
        assert!(store.load(START + 70_000).status().last_error.is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken history");
    }

    #[test]
    fn retained_history_is_bounded_and_cursor_pages_are_stable_as_new_sessions_arrive() {
        let mut history = fresh();
        for index in 0..RETAINED_SESSIONS + 2 {
            let seconds = index as u64 * 4;
            history.observe(reading(seconds, true, 100.0));
            history.observe(reading(seconds + 2, false, 99.0));
        }
        assert_eq!(history.status().retained_sessions, RETAINED_SESSIONS as i64);
        let (page, more) = history.page(2, None);
        assert!(more);
        assert_eq!(page[0].id, RETAINED_SESSIONS as i64 + 2);
        let cursor = page.last().unwrap().id;
        history.observe(reading((RETAINED_SESSIONS as u64 + 3) * 4, true, 100.0));
        let (older, _) = history.page(2, Some(cursor));
        assert_eq!(older[0].id, cursor - 1);
        assert_eq!(older[1].id, cursor - 2);
        assert!(history.page(100, Some(1)).0.is_empty());
    }
}
