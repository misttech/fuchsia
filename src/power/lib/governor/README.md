# Charge Governor Harness (`lib/governor`)

The `Governor` trait and `run_governor` event loop for software charge governors, such as `msc`,
`sw_jeita`, and `battery_defender`. A governor only decides what to vote; the harness talks to
battery-manager.

```
battery-manager                         run_governor
  Battery.Watch / Charger.Watch   ───►  wait for an update
  Battery/Charger.GetStatus       ───►  read the status that did not update
                                        Governor::evaluate(&Telemetry) -> Evaluation
  Controller.Set/ClearControl     ◄───  vote (only the changed fields)
  Battery.ConfigureWatch          ◄───  sample interval (only when it changes)
```

## Usage

```rust
use fidl_fuchsia_hardware_power_charger as fcharger;
use governor::{Evaluation, Governor, Telemetry, run_governor};

pub struct MyGovernor;

impl Governor for MyGovernor {
    fn evaluate(&mut self, _telemetry: &Telemetry) -> Evaluation {
        Evaluation {
            vote: fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                charge_current_limit_ua: Some(1_500_000),
                float_voltage_uv: Some(4_200_000),
                ..Default::default()
            },
            sample_interval: Some(zx::BootDuration::from_seconds(5)),
            ..Default::default()
        }
    }
}
```

`Governor::evaluate` returns an `Evaluation`:

| Field | Meaning |
|---|---|
| `vote` | This governor's complete desired `ControlOptions`. `None` fields are unconstrained. |
| `sample_interval` | How often battery-manager should sample the battery, or `None` for no samples. |
| `wake_for_samples` | Whether samples wake the system from suspend. Waking costs power, so set it only while samples must not wait, e.g. while charging. |

`Governor::watch_interest` optionally narrows which battery and charger fields trigger an
evaluation (`WatchOptions.interest`) and which also wake the system (`WatchOptions.wake_on`), e.g.
the charger going online. By default every field battery-manager can watch triggers one.

## Event loop

battery-manager is the only client of the battery and charger drivers, and serves each governor
`fuchsia.hardware.power.battery.Battery`, `fuchsia.hardware.power.charger.Charger`, and
`fuchsia.hardware.power.charger.Controller`. On each update, `run_governor`:

1. Waits for `Battery.Watch` or `Charger.Watch`, and takes the other response too if it is ready.
2. Reads the status that did not update with `GetStatus` (see below).
3. Calls `Governor::evaluate`.
4. Sends the changed vote fields to `Controller`: `SetControl` first, then `ClearControl`, so
   limits tighten before they relax. `ClearControl` is skipped if `SetControl` fails.
5. Sends `sample_interval` and `wake_for_samples` with `Battery.ConfigureWatch` if they changed.
6. Re-arms the `Watch` that completed.

### Consistent snapshots

The battery and charger statuses arrive on separate channels, so a `Watch` response is only a
trigger. Before evaluating, the harness reads the other status from battery-manager's cache, which
battery-manager updates before it completes either `Watch`. So a governor never evaluates a new
status with a stale one, and sees fields outside its interest, such as the battery current, as of
the same update.

The later of the two responses still triggers another evaluation, usually of the same snapshot,
so **`evaluate` may run more than once per update**: don't count each call as a sample.

On a board without a charger driver, `Charger.Watch` never completes. Once `Charger.GetStatus`
fails with no charger status known, the harness stops reading it until `Charger.Watch` reports.

### Sampling

Only the battery is sampled: charger changes (`online`, `charge_phase`, `operating_mode`) come from
hardware interrupts, but battery voltage, current, and temperature only change when sampled.
battery-manager owns the timer. It combines all governors' sample intervals on the boot clock and
completes `Battery.Watch` for each sample, so the harness runs no timers of its own.

### Wake leases

A `Watch` response that woke the system carries a wake lease. The harness holds it through steps
2–5, then passes it to the next `Watch` on that connection. battery-manager hands it back if that
`Watch` completes immediately, and drops it if the `Watch` waits, so a waiting `Watch` never blocks
suspend.

## Failures

| Failure | Behavior |
|---|---|
| `SetControl` or `ClearControl` returns an error (`INVALID_ARGS`, `NOT_SUPPORTED`) | Logs a warning, keeps the session and the accepted votes, and retries on the next evaluation. |
| A channel closes or has a transport error | Drops the session, waits 2 s (`RECONNECT_DELAY`), and reconnects. |
| The governor exits or crashes | battery-manager's `ChargeLimitArbiter` withdraws its votes. |

`SetControl` is atomic, so a governor should vote only fields the charger supports: one unsupported
field gets the whole change rejected at every evaluation.
