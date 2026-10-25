# SMS Gamer 2000 protocol findings

## Observed hardware

- SMS Gamer 2000, model `GAM2000BiBiW`, firmware `1.0`.
- Prolific PL2303GC bridge, USB ID `067b:23a3`, using Linux's `pl2303` driver.
- 2400 baud, 8 data bits, no parity, 1 stop bit, no flow control.
- Binary serial protocol; no USB HID Power Device interface.

Identity, ratings, and repeated status replies passed checksum validation. Initial voltage, load, charge, and temperature values were consistent with the display in a qualitative check. A physical mains disconnect and reconnect verified battery flag bit 7 on the tested firmware. Other flag meanings remain unverified.

Intermittent no-reply stalls remain unresolved. USB traces showed completed outgoing queries with no incoming bytes. No active USB autosuspend or system suspend was observed during the captured stalls. Matching PowerView's deasserted DTR setting did not eliminate them, so the driver retains Linux's default control-line levels. A short CPU idle experiment was inconclusive.

## Monitoring requests

Requests have seven bytes:

```text
command FF FF FF FF checksum 0D
```

`checksum = -sum(first five bytes) mod 256`. Equivalently, the sum of the first six bytes is zero modulo 256. The final `0D` is CR and is excluded from the checksum.

Observed queries and replies:

```text
Identity request
49 FF FF FF FF BB 0D

Identity reply
3A 47 41 4D 32 30 30 30 42 69 42 69 57 31 2E 30 F3 0D
   G  A  M  2  0  0  0  B  i  B  i  W  1  .  0

Status request
51 FF FF FF FF B3 0D

Status reply
3D 00 00 08 6B 08 60 00 E9 02 57 03 E8 01 E5 39 9C 0D

Ratings request
46 FF FF FF FF BE 0D

Ratings reply
3B 45 42 69 53 32 32 30 30 30 30 20 32 34 36 30 72 0D
   E  B  i  S  2  2  0  0  0  0     2  4  6  0
```

Each observed reply has 18 bytes. Byte 16 is a checksum over bytes 0 through 15. Byte 17 is CR. The sum of bytes 0 through 16 is zero modulo 256. Binary payload fields can contain `0D`, so splitting replies at CR is incorrect.

## Identity reply

Byte 0 is `3A`, an ASCII colon. Bytes 1 through 12 contain the model. Bytes 13 through 15 contain the firmware version.

## Status reply

Offsets are zero-based. Each numeric word is unsigned, big-endian, divided by 10 according to NUT's SMS implementation:

- Bytes 1 through 2: last input voltage. Observed `0000`, interpreted as 0.0 V. Its precise meaning on this model is unverified.
- Bytes 3 through 4: input voltage. `086B` gives 215.5 V.
- Bytes 5 through 6: output voltage. `0860` gives 214.4 V.
- Bytes 7 through 8: load. `00E9` gives 23.3%.
- Bytes 9 through 10: output frequency. `0257` gives 59.9 Hz.
- Bytes 11 through 12: battery percentage. `03E8` gives 100.0%.
- Bytes 13 through 14: temperature. `01E5` gives 48.5 degrees Celsius.
- Byte 15: status flags. Observed `39`, or binary `00111001`.

Byte 0 was `3D`, an ASCII equals sign. PowerView 5.01.07 maps `=` to line-interactive topology, which agrees with the Gamer manual. It maps `>` to the label `On Line Interative` and `<` to `UPS On Line`. Those other headers have not been observed on this unit.

Repeated polling produced changing voltage, load, and temperature values. The units follow the vendor parser and agree with the initial display check.

## Ratings reply

Byte 0 is `3B`, an ASCII semicolon. The observed payload is `EBiS220000 2460`:

- Bytes 1 through 7: voltage range code `EBiS220`.
- Bytes 8 through 10: current range code `000`.
- Bytes 11 through 13: nominal battery voltage ` 24`, or 24 V.
- Bytes 14 through 15: nominal output frequency `60`, or 60 Hz.

The range codes remain strings. They have not been interpreted beyond the field boundaries in SMS's definition.

## PowerView 5.01.07 inspection

PowerView 5.01.07 protocol resources and JVM bytecode were inspected without installing or running the application. The inspected archive SHA-256 is `71e5fdf7fb29f4d2cd939c7dc647038718c1eca0d8fec567a81ebcbb260e321a`. Vendor files and disassembly are not distributed with this project.

`powerview.jar!/protocolos/monofasico.xml` still defines only three wire queries for monitoring: `I`, `Q`, and `F`. Its duplicate `Default` aliases construct fallback beans locally. The remaining monophase commands change the beeper, run battery tests, schedule shutdown/restore, or cancel those operations. None was sent. Other UPS families have additional queries in separate protocol definitions; that does not establish support on the Gamer.

The monophase mobile response exposes input/output voltage, load in percent, battery percentage, output frequency, temperature, and topology. Its minimum and maximum readings come from comparisons in the desktop polling code. They are software history, not additional UPS measurements. The vendor parser also confirms division by ten for battery percentage on firmware `1.0`; its exception for firmware names containing `SE` does not apply here.

### Status-bit names

The XML and binary-string parser assign these names to byte 15, with bit 0 least significant:

| Bit | Vendor field | Candidate meaning | Value in captured `0x39` |
| --- | --- | --- | --- |
| 0 | `BeepLigado` | Beeper enabled | 1 |
| 1 | `ShutdownAtivo` | Shutdown active | 0 |
| 2 | `TesteAtivo` | Self-test active | 0 |
| 3 | `UpsOk` | UPS status | 1 |
| 4 | `Boost` | Voltage boost | 1 |
| 5 | `ByPass` | Bypass | 1 |
| 6 | `BateriaBaixa` | Low battery | 0 |
| 7 | `BateriaLigada` | Running on battery | 0 |

These are verified vendor parser assignments. A physical mains disconnect and reconnect separately verified bit 7 on the tested firmware. Other Gamer state meanings remain unverified. Both boost and bypass are set in the `0x39` baseline, so presenting both as established operating states would be premature. The direct Rust monitor preserves the byte and bits without assigning these names to live states.

During a mains disconnect, the byte changed from `0x39` to `0xb9`, input voltage became zero, and output remained near 220 V. After reconnection, it returned to `0x39`. One battery-mode sample already showed restored input voltage before bit 7 cleared, so the [PC protection policy](protection.md) uses the mode flag instead of an input-voltage threshold.

PowerView also modifies these values after parsing. Its traditional correction class, `br.com.sms.powerview.protocolo.a.a.a`, forces battery mode during a self-test. It replaces the low-battery field with a software comparison: on battery, charge must be at or below the configured positive threshold; on mains, the field is false. `br.com.sms.powerview.beans.Util` initializes that threshold to 33, and persisted configuration can override it. The PowerView low-battery warning therefore cannot be equated with raw bit 6.

`MedidorEstadoBean.isPotenciaSaidaElevada()` calculates a high-load warning by integer-parsing the load string and comparing it with 90. It returns false if parsing fails. This is not an independent overload measurement from the UPS.

### LCD and USB coverage

The [Gamer manual](https://legrand-sms.s3.amazonaws.com/manuais/manual-nobreak-sms-gamer-dovi051832_adId71M.pdf) describes battery voltage on the LCD on page 8. Its display diagram and operating tables on pages 29 through 32 also describe load percentage, alarms, and a counter for elapsed time operating on battery. That counter is not an estimate of remaining runtime.

The USB replies inspected here have no fields for measured battery voltage, remaining runtime, elapsed battery time, watts, amperes, fan speed, battery health, RGB settings, or the LCD fault code. Display availability does not establish USB availability. PowerView 5.01.07 provides no additional monophase monitoring query for these values. The next source to inspect for additional telemetry is the Gamer's Wi-Fi interface or a capture of software reading values absent from `Q`.

## Open questions

Battery mode was verified with a physical mains disconnect and reconnect. The other seven status flag meanings still need observation on this unit. PowerView supplies names for all eight bits, but the baseline `39` alone cannot verify their behavior on Gamer firmware.

`cargo run -- analyze CAPTURE.jsonl` compares the status byte and frame header across a capture. It reports bit changes alongside timestamps and numeric readings. The initial five-sample capture has no status changes: all five replies contain status byte `39` and header `3D`. A known state change and its recovery must be observed before assigning a meaning to a bit. Synthetic bit transitions in the analyzer tests verify the comparison code, not the UPS flag meanings.

Battery percentage is a device-reported value. We have not measured runtime or battery health. The traditional status reply does not include a measured battery voltage. In NUT's current driver, `battery.voltage` is calculated from nominal voltage and battery percentage, so it must not be treated as a battery voltage measurement.

PowerView's monophase mobile response labels load as a percentage. Measured watts, current, and RGB settings have not been decoded. Full battery discharge, self-tests, and UPS output control execution remain unverified on physical hardware.

All fields in the identified `I`, `F`, and `Q` monitoring replies are decoded or retained as raw values. Inspection of the supplied PowerView 5.01.07 installer found no further monophase monitoring queries. This does not exhaust the firmware or Wi-Fi interface. Additional numeric telemetry would require evidence of another supported read command or interface.

## Sources

- [SMS Gamer 2000 product documentation](https://www.sms.com.br/produtos/detalhe/nobreaks-ups-1/line-interactive-senoidal/nobreak-sms-gamer-2000-va/) confirms USB PC management and Wi-Fi management.
- PowerView 5.01.07 contains the vendor protocol definition and parser described above.
- [SMS Gamer user manual](https://legrand-sms.s3.amazonaws.com/manuais/manual-nobreak-sms-gamer-dovi051832_adId71M.pdf), revision `ET0029600-02`, documents LCD readings, operating indications, and Wi-Fi management.
- [NUT's SMS protocol notes](https://github.com/networkupstools/nut/blob/113ac87d3d00f9966e83cc4513b7d7856dd40fd3/docs/sms-brazil-protocol.txt) reproduce PowerView's `verificaProtocolo.xml` and `monofasico.xml`. These define the query bytes and response fields used here.
- [NUT's SMS serial driver](https://github.com/networkupstools/nut/blob/113ac87d3d00f9966e83cc4513b7d7856dd40fd3/drivers/sms_ser.c) provides the baud rate, response layouts, numeric scaling, and proposed flag meanings.
- [NUT's sms_ser manual](https://networkupstools.org/docs/man/sms_ser.html) limits its support claim to the SMS 1Phase protocol. The protocol was verified directly on this Gamer; the NUT driver itself has not been run here.
- [Linux's PL2303 USB identifiers](https://github.com/torvalds/linux/blob/master/drivers/usb/serial/pl2303.h) identify `067b:23a3` as PL2303GC.
- [Captured replies used by the tests](../tests/fixtures/gamer-2000.jsonl) contain representative protocol frames without capture metadata.
