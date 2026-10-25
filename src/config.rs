use std::{
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::api::{Config, ProtectionConfig, RuntimeConfig};

impl Default for ProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            warning_seconds: 90,
            shutdown_seconds: 50,
            battery_mode_verified: false,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            rated_output_w: 1400.0,
            battery_capacity_ah: 9.0,
            battery_health_percent: 100.0,
            inverter_efficiency_percent: 85.0,
            idle_power_w: 15.0,
            peukert_exponent: 1.25,
            capacity_rating_hours: 20.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: None,
            poll_interval_ms: 2_000,
            request_timeout_ms: 2_000,
            reconnect_interval_ms: 3_000,
            history_samples: 1_800,
            low_charge_percent: 33.0,
            high_load_percent: 90.0,
            high_temperature_c: 60.0,
            runtime: RuntimeConfig::default(),
            protection: Some(ProtectionConfig::default()),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(protection) = &self.protection {
            ensure!(
                (10..=3600).contains(&protection.shutdown_seconds)
                    && (11..=7200).contains(&protection.warning_seconds)
                    && protection.warning_seconds > protection.shutdown_seconds,
                "protection: warning must be 11..7200 seconds, shutdown 10..3600 seconds, and warning must precede shutdown"
            );
        }
        if let Some(device) = &self.device {
            ensure!(
                Path::new(device).is_absolute() && !device.contains('\0'),
                "device must be an absolute path"
            );
        }
        for (name, value, min, max) in [
            ("poll_interval_ms", self.poll_interval_ms, 1_000, 60_000),
            ("request_timeout_ms", self.request_timeout_ms, 100, 10_000),
            (
                "reconnect_interval_ms",
                self.reconnect_interval_ms,
                1_000,
                60_000,
            ),
            ("history_samples", self.history_samples, 1, 10_000),
        ] {
            ensure!(
                (min..=max).contains(&value),
                "{name} must be between {min} and {max}"
            );
        }
        for (name, value, max) in [
            ("low_charge_percent", self.low_charge_percent, 100.0),
            ("high_load_percent", self.high_load_percent, 200.0),
            ("high_temperature_c", self.high_temperature_c, 150.0),
        ] {
            ensure!(
                value.is_finite() && (0.0..=max).contains(&value),
                "{name} must be finite and between 0 and {max}"
            );
        }
        let runtime = &self.runtime;
        for (name, value, min, max) in [
            ("rated_output_w", runtime.rated_output_w, 1.0, 100_000.0),
            (
                "battery_capacity_ah",
                runtime.battery_capacity_ah,
                0.1,
                2_000.0,
            ),
            (
                "battery_health_percent",
                runtime.battery_health_percent,
                0.0,
                100.0,
            ),
            (
                "inverter_efficiency_percent",
                runtime.inverter_efficiency_percent,
                1.0,
                100.0,
            ),
            ("idle_power_w", runtime.idle_power_w, 1.0, 1_000.0),
            ("peukert_exponent", runtime.peukert_exponent, 1.0, 2.0),
            (
                "capacity_rating_hours",
                runtime.capacity_rating_hours,
                1.0,
                100.0,
            ),
        ] {
            ensure!(
                value.is_finite() && (min..=max).contains(&value),
                "runtime.{name} must be finite and between {min} and {max}"
            );
        }
        Ok(())
    }
}

pub fn default_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required; pass --config")?;
    Ok(base.join("sms-gamer/config.json"))
}

pub fn load(path: &Path) -> Result<Config> {
    let mut config: Config = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid config {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };
    config
        .protection
        .get_or_insert_with(ProtectionConfig::default);
    config.validate()?;
    Ok(config)
}

/// Persist before publishing a setting change; a failed write leaves live settings intact.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(config)?;
    if fs::read(path).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validates_and_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/config.json");
        let mut config = Config {
            high_load_percent: 85.0,
            ..Config::default()
        };
        save(&path, &config).unwrap();
        assert_eq!(load(&path).unwrap(), config);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        config.poll_interval_ms = 0;
        assert!(config.validate().is_err());
        config.poll_interval_ms = 2_000;
        config.high_load_percent = f64::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn legacy_config_keeps_protection_off_and_thresholds_are_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut legacy = serde_json::to_value(Config::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("protection");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let mut config = load(&path).unwrap();
        assert!(!config.protection.as_ref().unwrap().enabled);
        let mut protection = config.protection.take().unwrap();
        for (warning, shutdown) in [(50, 90), (50, 50), (90, 0), (7201, 50)] {
            protection.warning_seconds = warning;
            protection.shutdown_seconds = shutdown;
            config.protection = Some(protection.clone());
            assert!(config.validate().is_err());
        }
    }
}
