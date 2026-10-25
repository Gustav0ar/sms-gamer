//! Host protection policy. All times use CLOCK_BOOTTIME, including time spent suspended.
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::api::{ProtectionConfig, ProtectionPhase, ProtectionStatus, RuntimeConfig};

pub fn now_ms() -> u64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    boot_id: String,
    profile: RuntimeConfig,
    deadline_ms: u64,
    warned: bool,
    requested: bool,
}

struct Discharge {
    deadline_ms: u64,
    restored: bool,
}

#[derive(Default)]
enum Shutdown {
    #[default]
    Ready,
    Requesting,
    Requested,
    Failed {
        reason: String,
        retry_at_ms: u64,
    },
}

#[derive(Default)]
pub struct Engine {
    discharge: Option<Discharge>,
    candidate_at: Option<u64>,
    warned: bool,
    shutdown: Shutdown,
}

impl Engine {
    /// Two battery samples establish an episode; one valid mains sample cancels it.
    pub fn observe(&mut self, battery: bool, remaining: i64, now: u64) {
        if !battery {
            self.discharge = None;
            self.candidate_at = None;
            self.warned = false;
            if !matches!(self.shutdown, Shutdown::Requested | Shutdown::Requesting) {
                self.shutdown = Shutdown::Ready;
            }
            return;
        }
        let deadline = now.saturating_add(remaining.max(0) as u64 * 1000);
        if let Some(discharge) = &mut self.discharge {
            discharge.deadline_ms = if self.warned || discharge.restored {
                discharge.deadline_ms.min(deadline)
            } else {
                deadline
            };
        } else if self
            .candidate_at
            .is_some_and(|at| (500..=6000).contains(&now.saturating_sub(at)))
        {
            self.discharge = Some(Discharge {
                deadline_ms: deadline,
                restored: false,
            });
            self.candidate_at = None;
        } else {
            self.candidate_at = Some(now);
        }
    }

    pub fn reset(&mut self) {
        if !matches!(self.shutdown, Shutdown::Requested | Shutdown::Requesting) {
            *self = Self::default();
        }
    }

    pub fn tick(
        &mut self,
        config: &ProtectionConfig,
        stale: bool,
        dry_run: bool,
        now: u64,
    ) -> ProtectionStatus {
        if !config.enabled {
            self.reset();
        }
        let remaining = self
            .discharge
            .as_ref()
            .map(|discharge| discharge.deadline_ms.saturating_sub(now).div_ceil(1000) as i64);
        if config.enabled && remaining.is_some_and(|value| value <= config.warning_seconds) {
            self.warned = true;
        }
        let (phase, error) = match &self.shutdown {
            Shutdown::Requested | Shutdown::Requesting => {
                (ProtectionPhase::shutdown_requested, None)
            }
            Shutdown::Failed { reason, .. } => {
                (ProtectionPhase::shutdown_failed, Some(reason.clone()))
            }
            Shutdown::Ready if !config.enabled => (ProtectionPhase::disabled, None),
            Shutdown::Ready if !config.battery_mode_verified => (
                ProtectionPhase::unverified,
                Some(
                    "Verify UPS battery-mode detection before enabling automatic PC shutdown"
                        .into(),
                ),
            ),
            Shutdown::Ready if self.warned => (ProtectionPhase::warning, None),
            Shutdown::Ready if remaining.is_some() => (ProtectionPhase::monitoring, None),
            Shutdown::Ready => (ProtectionPhase::waiting, None),
        };
        ProtectionStatus {
            phase,
            remaining_seconds: remaining,
            shutdown_in_seconds: remaining.map(|value| (value - config.shutdown_seconds).max(0)),
            warning_active: self.warned,
            telemetry_stale: stale && self.discharge.is_some(),
            last_error: error,
            dry_run,
        }
    }

    /// Latch before the external call. A failed call retries after five seconds;
    /// a successful call is never repeated, even if mains returns during shutdown.
    pub fn request_shutdown(&mut self, config: &ProtectionConfig, manual: bool, now: u64) -> bool {
        if !config.enabled || matches!(self.shutdown, Shutdown::Requested | Shutdown::Requesting) {
            return false;
        }
        if let Shutdown::Failed { retry_at_ms, .. } = self.shutdown {
            if !manual && now < retry_at_ms {
                return false;
            }
        }
        let Some(discharge) = &self.discharge else {
            return false;
        };
        let due =
            discharge.deadline_ms.saturating_sub(now) <= config.shutdown_seconds as u64 * 1000;
        if (manual && self.warned) || (config.battery_mode_verified && due) {
            self.shutdown = Shutdown::Requesting;
            true
        } else {
            false
        }
    }

    pub fn failed(&mut self, reason: String, now: u64) {
        self.shutdown = Shutdown::Failed {
            reason,
            retry_at_ms: now.saturating_add(5000),
        };
    }

    pub fn accepted(&mut self) {
        self.shutdown = Shutdown::Requested;
    }

    pub fn checkpoint(&self, boot_id: &str, profile: &RuntimeConfig) -> Option<Checkpoint> {
        self.discharge.as_ref().map(|discharge| Checkpoint {
            boot_id: boot_id.into(),
            profile: profile.clone(),
            deadline_ms: discharge.deadline_ms,
            warned: self.warned,
            requested: matches!(self.shutdown, Shutdown::Requested),
        })
    }

    pub fn restore(&mut self, checkpoint: Checkpoint, boot_id: &str, profile: &RuntimeConfig) {
        if checkpoint.boot_id == boot_id && checkpoint.profile == *profile {
            self.discharge = Some(Discharge {
                deadline_ms: checkpoint.deadline_ms,
                restored: true,
            });
            self.warned = checkpoint.warned;
            self.shutdown = if checkpoint.requested {
                Shutdown::Requested
            } else {
                Shutdown::Ready
            };
        }
    }
}

pub fn load(path: &Path) -> Result<Option<Checkpoint>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: &Path, checkpoint: Option<&Checkpoint>) -> Result<()> {
    let Some(checkpoint) = checkpoint else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    };
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(checkpoint)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn enabled() -> ProtectionConfig {
        ProtectionConfig {
            enabled: true,
            battery_mode_verified: true,
            ..ProtectionConfig::default()
        }
    }
    fn battery(engine: &mut Engine, seconds: i64) {
        engine.observe(true, seconds + 2, 0);
        engine.observe(true, seconds, 2000);
    }
    #[test]
    fn warns_at_ninety_and_requests_once_at_fifty_even_without_more_serial_reads() {
        let config = enabled();
        let mut engine = Engine::default();
        battery(&mut engine, 100);
        assert!(!engine.tick(&config, false, true, 2000).warning_active);
        assert!(engine.tick(&config, true, true, 12000).warning_active);
        assert!(!engine.request_shutdown(&config, false, 51000));
        assert!(engine.request_shutdown(&config, false, 52000));
        assert!(!engine.request_shutdown(&config, false, 53000));
        assert_eq!(
            engine.tick(&config, true, true, 53000).phase,
            ProtectionPhase::shutdown_requested
        );
    }
    #[test]
    fn requires_two_battery_samples_and_cancels_before_commit_when_mains_returns() {
        let config = enabled();
        let mut engine = Engine::default();
        engine.observe(true, 20, 0);
        assert!(!engine.request_shutdown(&config, false, 1000));
        engine.observe(false, 100, 2000);
        assert_eq!(
            engine.tick(&config, false, true, 2000).phase,
            ProtectionPhase::waiting
        );
        battery(&mut engine, 90);
        engine.tick(&config, false, true, 2000);
        engine.observe(false, 100, 42000);
        assert!(!engine.request_shutdown(&config, false, 42000));
        assert!(!engine.tick(&config, false, true, 42000).warning_active);
    }
    #[test]
    fn unverified_mode_warns_but_never_automatically_powers_off() {
        let config = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        let mut engine = Engine::default();
        battery(&mut engine, 10);
        let status = engine.tick(&config, false, true, 2000);
        assert!(status.warning_active);
        assert_eq!(status.phase, ProtectionPhase::unverified);
        assert!(!engine.request_shutdown(&config, false, 2000));
        assert!(engine.request_shutdown(&config, true, 2000));
    }
    #[test]
    fn warning_countdown_never_extends_and_survives_same_boot_restart() {
        let config = enabled();
        let mut engine = Engine::default();
        battery(&mut engine, 90);
        engine.tick(&config, false, true, 2000);
        engine.observe(true, 150, 4000);
        assert_eq!(
            engine.tick(&config, false, true, 4000).remaining_seconds,
            Some(88)
        );
        let profile = RuntimeConfig::default();
        let checkpoint = engine.checkpoint("boot", &profile).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("protection.json");
        save(&file, Some(&checkpoint)).unwrap();
        let mut resumed = Engine::default();
        resumed.restore(load(&file).unwrap().unwrap(), "boot", &profile);
        resumed.observe(true, 200, 42000);
        assert!(resumed.request_shutdown(&config, false, 42000));
        let mut rebooted = Engine::default();
        rebooted.restore(checkpoint, "different-boot", &profile);
        assert!(!rebooted.request_shutdown(&config, false, 42000));
    }
    #[test]
    fn failure_retries_with_delay_and_disable_clears_an_armed_episode() {
        let config = enabled();
        let mut engine = Engine::default();
        battery(&mut engine, 50);
        assert!(engine.request_shutdown(&config, false, 2000));
        engine.failed("Not authorized".into(), 2000);
        assert_eq!(
            engine.tick(&config, false, true, 3000).phase,
            ProtectionPhase::shutdown_failed
        );
        assert!(!engine.request_shutdown(&config, false, 6999));
        assert!(engine.request_shutdown(&config, false, 7000));
        engine.failed("Bus unavailable".into(), 7000);
        engine.tick(&ProtectionConfig::default(), false, true, 8000);
        assert!(!engine.request_shutdown(&config, false, 9000));
    }
}
