The Battery Manager component surfaces battery info to other components.

It obtains lower-level information from `fuchsia.hardware.powersource.Service`
(legacy), `fuchsia.hardware.power.battery.Service` (Fuel Gauge data plane), and
`fuchsia.hardware.power.charger.Service` (Charger status, operating mode, and
power source). It derives combined power metrics (charge source, charge status,
charging currents) and exposes `fuchsia.power.battery.BatteryManager`.

For devices that don't have a real battery, Battery Manager can _simulate
battery data_, by exposing fuchsia.power.battery.test.BatterySimulator.

On Sorrel devices, the battery driver does not provide battery info through
fuchsia.hardware.powersource.Service, and instead directly exposes
fuchsia.power.battery.BatteryInfoProvider to the primary consumer.
Hence, on Sorrel, Battery Manager is bypassed and unused.

## Charge Limit Arbitration and Governors

Battery Manager runs child charge governors (listed in `governor_urls` in
`/config/config.json`) inside a `#governors` collection. Battery Manager is the
sole client of the battery and charger drivers; governors do not connect to the
drivers directly.

### Voting
Each governor is given its own `fuchsia.hardware.power.charger.Controller`
connector, so its votes are attributed to its child name (e.g., `msc`). Closing
the connection clears that governor's votes.

`ChargeLimitArbiter` combines active votes across governors and sends the
effective settings to the charger driver. The vote that charges least wins:
- **Lower charges less**: lowest active vote (`input_current_limit_ua`,
  `charge_current_limit_ua`, `float_voltage_uv`, `top_off_duration`).
- **Higher charges less**: highest active vote (`input_voltage_limit_uv`,
  `charge_term_current_ua`).
- **Operating mode**: highest-precedence vote
  (`Discharging > Passthrough > Charging`). `Otg` is rejected.

### Supervision and Reconnection
- **Governors**: If a governor exits or fails to start, Battery Manager restarts
  it with exponential backoff (2s to 60s, resetting after 60s of running).
- **Drivers**: If a battery or charger `Watch` ends, Battery Manager reconnects
  with exponential backoff (1s to 60s). Charger votes stay applied as long as
  the driver's `Controller` channel remains open.

### Inspect
- `charge_limit_arbiter/elections/<field>`: `result`, `overridden`, and
  per-governor `votes`.
- `governors/<child_name>`: `url`, `running`, and `restarts`.
- `governor_telemetry`: per-governor `battery_sample_interval` and
  `charger_sample_interval` votes, `wakeup`, and `sample_count`.

## Battery and Charger Status Cache

Each governor also receives its own `fuchsia.hardware.power.battery.Battery` and
`fuchsia.hardware.power.charger.Charger` connectors. Battery Manager serves them
from the driver status it caches, so governors add no bus reads or driver
clients. See `src/cache.rs` for the semantics.

Governors can also request periodic samples with `ConfigureWatch.max_interval`.
A sample notifies `BatteryInfo` watchers only if it changes a field they act on.
Products with suspend enabled must serve `fuchsia.time.alarms.WakeAlarms` from
Timekeeper (`timekeeper.serve_fuchsia_time_alarms`) so that samples can wake the
system.
