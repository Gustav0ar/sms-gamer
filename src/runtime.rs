use std::time::Instant;

use crate::api::{RuntimeConfig, RuntimeEstimate};

pub fn estimated_load_w(config: &RuntimeConfig, load_percent: f64) -> f64 {
    config.rated_output_w * load_percent / 100.0
}

#[derive(Default)]
pub struct Tracker {
    discharge: Option<Discharge>,
}

struct Discharge {
    at: Instant,
    fraction: f64,
    full_seconds: f64,
}

impl Tracker {
    pub fn reset(&mut self) {
        self.discharge = None;
    }

    /// Integrate modeled discharge between samples so a flat battery percentage
    /// does not freeze the countdown. Reported charge can lower the estimate.
    pub fn estimate(
        &mut self,
        config: &RuntimeConfig,
        nominal_v: f64,
        load_percent: f64,
        charge_percent: f64,
        battery_mode: bool,
        now: Instant,
    ) -> Option<RuntimeEstimate> {
        if nominal_v <= 0.0 {
            self.reset();
            return None;
        }
        let load_w = estimated_load_w(config, load_percent);
        let battery_power_w =
            load_w / (config.inverter_efficiency_percent / 100.0) + config.idle_power_w;
        let current_a = battery_power_w / nominal_v;
        let effective_ah = config.battery_capacity_ah * config.battery_health_percent / 100.0;
        let reference_a = effective_ah / config.capacity_rating_hours;
        // Peukert at the configured capacity-rating discharge rate. Do not award
        // extra capacity below that rate, where this model extrapolates poorly.
        let full_seconds = (config.capacity_rating_hours
            * (reference_a / current_a).powf(config.peukert_exponent)
            * 3600.0)
            .min(effective_ah / current_a * 3600.0);
        let reported_fraction = (charge_percent / 100.0).clamp(0.0, 1.0);
        let mut fraction = reported_fraction;
        if battery_mode {
            if let Some(previous) = &self.discharge {
                if full_seconds > 0.0 && previous.full_seconds > 0.0 {
                    let elapsed = now.saturating_duration_since(previous.at).as_secs_f64();
                    let used = elapsed * (1.0 / previous.full_seconds + 1.0 / full_seconds) / 2.0;
                    fraction = (previous.fraction - used).max(0.0).min(reported_fraction);
                }
            }
            self.discharge = Some(Discharge {
                at: now,
                fraction,
                full_seconds,
            });
        } else {
            self.reset();
        }
        Some(RuntimeEstimate {
            remaining_seconds: (full_seconds * fraction).round() as i64,
            estimated_load_w: load_w,
            estimated_battery_power_w: battery_power_w,
            nominal_capacity_wh: nominal_v * config.battery_capacity_ah,
            estimated_remaining_charge_percent: fraction * 100.0,
            battery_mode,
            mode_verified: false,
            model: "peukert_with_integrated_discharge".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn countdown_decreases_even_when_reported_charge_stays_full() {
        let config = RuntimeConfig::default();
        let mut tracker = Tracker::default();
        let now = Instant::now();
        let start = tracker
            .estimate(&config, 24.0, 25.0, 100.0, true, now)
            .unwrap();
        let later = tracker
            .estimate(
                &config,
                24.0,
                25.0,
                100.0,
                true,
                now + Duration::from_secs(60),
            )
            .unwrap();
        assert!((start.remaining_seconds - later.remaining_seconds - 60).abs() <= 1);
        assert!(later.estimated_remaining_charge_percent < 100.0);
        assert!(!later.mode_verified);
        let heavy = Tracker::default()
            .estimate(&config, 24.0, 50.0, 100.0, true, now)
            .unwrap();
        assert!(heavy.remaining_seconds < start.remaining_seconds / 2);
    }

    #[test]
    fn energy_limit_charge_aging_and_mains_reset_are_consistent() {
        let config = RuntimeConfig {
            peukert_exponent: 1.0,
            inverter_efficiency_percent: 100.0,
            idle_power_w: 10.0,
            rated_output_w: 1000.0,
            ..RuntimeConfig::default()
        };
        let mut tracker = Tracker::default();
        let now = Instant::now();
        let half = tracker
            .estimate(&config, 24.0, 20.0, 50.0, true, now)
            .unwrap();
        assert_eq!(
            half.remaining_seconds,
            (24.0_f64 * 9.0 * 0.5 / 210.0 * 3600.0).round() as i64
        );
        let old = RuntimeConfig {
            battery_health_percent: 50.0,
            ..config.clone()
        };
        let aged = Tracker::default()
            .estimate(&old, 24.0, 20.0, 50.0, true, now)
            .unwrap();
        assert!((aged.remaining_seconds * 2 - half.remaining_seconds).abs() <= 1);
        let empty = tracker
            .estimate(&config, 24.0, 20.0, 0.0, true, now)
            .unwrap();
        assert_eq!(empty.remaining_seconds, 0);
        let mains = tracker
            .estimate(&config, 24.0, 20.0, 100.0, false, now)
            .unwrap();
        assert!(!mains.battery_mode);
        let resumed = tracker
            .estimate(
                &config,
                24.0,
                20.0,
                100.0,
                true,
                now + Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(resumed.remaining_seconds, mains.remaining_seconds);
        assert!(
            tracker
                .estimate(&config, 0.0, 0.0, 100.0, true, now)
                .is_none()
        );
    }
}
