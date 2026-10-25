# Battery runtime estimate

The daemon estimates time remaining while the UPS operates on battery. It exposes the result in `Sample.runtime`, separately from device telemetry. The Noctalia bar shows approximate minutes, and the overview shows estimated time remaining and load in watts. On mains power, Details shows the predicted runtime at the current load.

## Battery profile

The stock Gamer 2000 has two 12 V, 9 Ah batteries in series, according to page 44 of the [SMS manual](https://legrand-sms.s3.amazonaws.com/manuais/manual-nobreak-sms-gamer-dovi051832_adId71M.pdf). That is a 24 V, 9 Ah bank with 216 Wh of nominal energy. Series connection doubles voltage, while capacity stays 9 Ah. The [product specification](https://www.sms.com.br/produtos/detalhe/nobreaks-ups-1/line-interactive-senoidal/nobreak-sms-gamer-2000-va/) lists 2000 VA and an output power factor of 0.7, giving 1400 W rated output.

The default profile uses that rating and capacity. Inverter efficiency of 85%, idle draw of 15 W, and battery health of 100% are editable assumptions. The C20 capacity rating and Peukert exponent of 1.25 are generic lead-acid defaults, consistent with the approach described in [Victron's battery-monitor manual](https://www.victronenergy.com/upload/documents/BMV_-700/9172-Manual_BMV_and_SmartShunt-pdf-en.pdf). They are not measured SMS battery characteristics.

In Noctalia, **Settings → Runtime estimation** exposes rated output, battery capacity, and assumed health. **Model assumptions** exposes efficiency, idle draw, exponent, and capacity rating period. An external battery bank or aging batteries require a different profile. Other apps can update the same `Config.runtime` object through Varlink.

## Calculation

The daemon uses reported load percentage as a fraction of configured rated watts. It estimates battery power from that load, inverter losses, and idle consumption:

```text
load_w = rated_output_w × load_percent / 100
battery_power_w = load_w / (efficiency_percent / 100) + idle_power_w
current_a = battery_power_w / nominal_battery_v
effective_ah = battery_capacity_ah × battery_health_percent / 100
reference_a = effective_ah / capacity_rating_hours

full_seconds = 3600 × capacity_rating_hours × (reference_a / current_a) ^ peukert_exponent
```

At low currents, the model caps `full_seconds` at `3600 × effective_ah / current_a`. It therefore does not predict extra capacity below the configured rating current. With exponent 1, the calculation reduces to stored energy divided by estimated battery power.

The initial remaining fraction is the UPS charge percentage. During battery operation, the daemon integrates modeled discharge using monotonic elapsed time and the previous and current loads. A flat reported charge percentage does not freeze the countdown. A lower reported charge can reduce the estimate, but an upward fluctuation does not increase the tracked fraction during the same discharge. Remaining seconds equal the tracked fraction multiplied by full-charge runtime at the latest load, so a heavier load shortens the prediction.

Returning to mains, disconnecting, restarting the daemon, or changing the battery profile resets the integration. The next discharge starts from the reported charge. Missing or zero nominal voltage makes the estimate unavailable.

## Accuracy limits

The UPS does not report measured watts, battery current, usable capacity, or time to empty through the known queries. Estimated watts assume its load percentage scales with rated real power. Battery age, temperature, inverter losses, cutoff voltage, and the firmware's charge calculation can change actual runtime. The estimate is not calibrated against a full discharge of this unit. The optional [PC protection policy](protection.md) uses it to warn and request an orderly shutdown with a configurable reserve.

The runtime model follows PowerView's on-battery or self-test flags. A physical mains disconnect and reconnect verified the on-battery flag on the tested model and firmware. Recording that verification in `Config.protection` sets `mode_verified: true` for normal battery and mains operation. Self-test-only mode remains unverified and does not trigger the automatic PC policy. `flags_verified` stays false because the complete flag mapping has not been verified.

Focused Rust tests check the energy units, heavier loads, aging, empty charge, returning to mains, missing voltage, and a countdown that decreases while reported charge stays constant. Native UI verification uses a simulated battery-mode frame, with configuration changes applied through the real Varlink daemon.
