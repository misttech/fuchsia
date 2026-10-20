# batteryutil

`batteryutil` is a command-line diagnostic and control utility for battery fuel gauges and power
path chargers on Fuchsia.

It connects to `fuchsia.hardware.power.battery.Service` (Fuel Gauge data plane),
`fuchsia.hardware.power.charger.Service` (charger telemetry),
`fuchsia.hardware.power.charger.DebugService` (charger operating mode control),
and `fuchsia.power.battery` services.

## Specifying Paths & Multiple Devices

By default, `batteryutil` automatically discovers available battery and charger service instances
under `/svc/`. If multiple instances are present:
- `get`, `battery get`, and `charger get` query and report telemetry for all discovered instances.
- `watch`, `battery watch`, and `charger watch` multiplex real-time streaming updates across all
  discovered instances.
- Charger control subcommands (`charger mode`, `charger charge-current`, `charger float-voltage`,
  `charger term-current`, `charger clear`) select the first discovered instance and warn if multiple are present.

To target a specific battery or charger instance directly, pass the `-p` / `--path` option before
the command:

```bash
# Query a specific battery service instance
$ batteryutil -p /svc/fuchsia.hardware.power.battery.Service/default get

# Watch a specific fuel gauge instance
$ batteryutil -p /svc/fuchsia.hardware.power.battery.Service/default watch

# Set operating mode on a specific charger instance
$ batteryutil -p /svc/fuchsia.hardware.power.charger.DebugService/default charger mode charging
```

## Commands

### 1. Telemetry Inspection (`get`, `battery get`, `charger get`)
Query real-time battery and charger status across all discovered instances (`batteryutil get`),
battery-only telemetry (`batteryutil battery get`), or charger-only telemetry
(`batteryutil charger get`).

```console
$ batteryutil get
=== Battery Telemetry (/svc/fuchsia.hardware.power.battery.Service/default) ===
Model: MAX77779
Chemistry: Li-Ion
Design Capacity: 4.947 Ah
Design Voltage: 3.850 V
Supported Triggers: level_percent, cycle_count
Supported Wake Triggers: None
Present: true
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)

=== Charger Telemetry (/svc/fuchsia.hardware.power.charger.Service/default) ===
Charger Model: MAX77779
Max Charge Current: 4.000 A
Max Charge Voltage: 4.550 V
Max Input Current: 3.200 A
Supported Operating Modes: Charging, Passthrough, Discharging, OTG
Supported Triggers: online, operating_mode, charge_phase, source_type
Supported Wake Triggers: None
Online: true
Source Type: USB
Operating Mode: Charging
Charge Phase: Fast
Input Current: 499.992 mA
Input Current Limit: 1.500 A
Charge Current Limit: 2.000 A
Float Voltage: 4.400 V

$ batteryutil battery get
Model: MAX77779
Chemistry: Li-Ion
Design Capacity: 4.947 Ah
Design Voltage: 3.850 V
Supported Triggers: level_percent, cycle_count
Supported Wake Triggers: None
Present: true
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)

$ batteryutil charger get
Charger Model: MAX77779
Max Charge Current: 4.000 A
Max Charge Voltage: 4.550 V
Max Input Current: 3.200 A
Supported Operating Modes: Charging, Passthrough, Discharging, OTG
Supported Triggers: online, operating_mode, charge_phase, source_type
Supported Wake Triggers: None
Online: true
Source Type: USB
Operating Mode: Charging
Charge Phase: Fast
Input Current: 499.992 mA
Input Current Limit: 1.500 A
Charge Current Limit: 2.000 A
Float Voltage: 4.400 V
```

### 2. Real-Time Event Streaming (`watch`, `battery watch`, `charger watch`)
Stream state transitions and telemetry changes via hanging-get without polling (`batteryutil watch`
for both battery and charger instances, `batteryutil battery watch` for battery instances only, or
`batteryutil charger watch` for charger instances only).

```console
$ batteryutil watch
Watching battery and charger events on all instances (press Ctrl+C to exit)...

=== Battery Telemetry Update (/svc/fuchsia.hardware.power.battery.Service/default) ===
Present: true
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)

=== Charger Telemetry Update (/svc/fuchsia.hardware.power.charger.Service/default) ===
Online: true
Source Type: USB
Operating Mode: Charging
Charge Phase: Fast
Input Current: 499.992 mA
Input Current Limit: 1.500 A
Charge Current Limit: 2.000 A
Float Voltage: 4.400 V

$ batteryutil battery watch
Watching battery events on all instances (press Ctrl+C to exit)...

=== Battery Telemetry Update (/svc/fuchsia.hardware.power.battery.Service/default) ===
Present: true
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)

$ batteryutil charger watch
Watching charger events on all instances (press Ctrl+C to exit)...

=== Charger Telemetry Update (/svc/fuchsia.hardware.power.charger.Service/default) ===
Online: true
Source Type: USB
Operating Mode: Charging
Charge Phase: Fast
Input Current: 499.992 mA
Input Current Limit: 1.500 A
Charge Current Limit: 2.000 A
Float Voltage: 4.400 V
```

### 3. Charger Operating Mode & Limit Controls (`charger`)
Control charger settings via `fuchsia.hardware.power.charger.DebugService` (with `charger mode`
falling back to Sorrel `SPMI 0x2954` + legacy `fuchsia.power.battery.ChargerService`).

Supported operating modes (`batteryutil charger mode <mode>`):
- `charging` / `usb`: `OperatingMode::Charging` (USB powers system + charges battery)
- `passthrough`: `OperatingMode::Passthrough` (USB powers system, charging inhibited)
- `discharging` / `battery`: `OperatingMode::Discharging` (active discharging from battery)
- `otg`: `OperatingMode::Otg` (reverse boost)
- `auto`: clears the operating mode override on `DebugService` (`Debug.ClearControl`)

Supported limit and reset controls:
- `batteryutil charger charge-current <limit | auto>`: sets or clears the sticky fast charge
  current limit (`charge_current_limit_ua`, e.g. `2.5A`, `2500mA`, `2500000uA`, `2500000`, or
  `auto`; bare numbers without a unit suffix are in microamps, `uA`)
- `batteryutil charger float-voltage <limit | auto>`: sets or clears the sticky battery float
  voltage limit (`float_voltage_uv`, e.g. `4.35V`, `4350mV`, `4350000uV`, `4350000`, or `auto`;
  bare numbers without a unit suffix are in microvolts, `uV`)
- `batteryutil charger term-current <current | auto>`: sets or clears the sticky charge
  termination (top-off) current (`charge_term_current_ua`, e.g. `200mA`, `200000uA`, `200000`, or
  `auto`; bare numbers without a unit suffix are in microamps, `uA`)
- `batteryutil charger clear`: clears all sticky `DebugService` overrides

Settings applied through `DebugService` are sticky overrides: they stay in effect after
`batteryutil` exits and take precedence over the production policy client connected to `Controller`
until cleared per-field (`auto`) or all at once (`batteryutil charger clear`).

```console
$ batteryutil charger mode charging
Successfully set charger operating mode to Charging via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger mode passthrough
Successfully set charger operating mode to Passthrough via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger mode discharging
Successfully set charger operating mode to Discharging via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger mode auto
Successfully cleared charger operating mode override via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger charge-current 2.5A
Successfully set charger fast charge current limit to 2.500 A via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger charge-current auto
Successfully cleared charger fast charge current limit override via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger float-voltage 4.35V
Successfully set charger float voltage to 4.350 V via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger float-voltage auto
Successfully cleared charger float voltage override via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger term-current 200mA
Successfully set charger termination current to 200.000 mA via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger term-current auto
Successfully cleared charger termination current override via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil charger clear
Successfully cleared all charger overrides via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)
```
