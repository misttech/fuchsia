---
name: sampler
description: >
  End-to-end validation of Fuchsia Diagnostics Sampler and Archivist
  SampleServer configurations on real hardware (local or remote via fx test-
  remote) by observing Cobalt local event traces ([COBALT_EVENT_TRACE]) in
  device syslog across multiple polling intervals.
---

# Fuchsia Diagnostics Sampler Hardware Validation Skill (`sampler`)

This skill provides a repeatable workflow to validate the full
**Driver/Component Inspect → Archivist (`SampleServer`) → Sampler → Cobalt**
telemetry pipeline on real hardware (either remotely via `fx test-remote` or on
a locally attached device). It is especially useful when verifying:

- Hardware-only Inspect trees (such as NAND/FTL `[name=ftl]`, `[name=nand]`, or
  SDMMC `[name=sdmmc]` drivers under `bootstrap/*-drivers:*`).
- Steady-state `SampleStrategy::OnDiff` delta calculation across multiple polls
  for `Occurrence` and `IntHistogram` metrics vs. `SampleStrategy::Always`
  (`Integer` / `String`) metrics.
- Selector matching, wildcard monikers, and tree-name filters in
  `src/diagnostics/config/sampler/*.json5`.

> **Important:**
> All source and configuration edits described in this workflow are **temporary
> local validation scaffolding (`DO NOT SUBMIT`)**. Always revert them once
> validation is complete.

## Does Sampler Write to Disk? (Flash Wear Considerations)

- **Sampler (`core/sampler`) performs zero disk writes.** It has no `storage`
  (`data`, `cache`, `tmp`) or `directory` capabilities in
  `src/diagnostics/sampler/meta/sampler.cml` or
  `src/diagnostics/sampler/meta/sampler.core_shard.cml`. Its previous-sample
  cache (`Project::cache` in `src/diagnostics/sampler/src/project.rs`) is a pure
  in-memory `HashMap<(MetricId, Vec<EventCode>), Property>`.
- **Archivist (`bootstrap/archivist`) performs zero disk writes** when serving
  `fuchsia.diagnostics.Sample`; its `OnDiff` cache lives entirely in memory.
- **Cobalt (`core/cobalt`) batches local aggregation in memory** via
  `DelayedLocalAggregateStorage`
  (`third_party/cobalt/src/local_aggregation/local_aggregate_storage/delayed_local_aggregate_storage.cc`),
  which only flushes `GlobalAggregates` to `/data` once every **5 minutes**
  (`kDefaultWritebackFrequency = std::chrono::minutes(5)`) or on orderly
  shutdown. Consequently, lowering Sampler's poll period to 5 seconds for a test
  does **not** cause 5-second disk writes unless you also explicitly lower
  Cobalt's `kDefaultWritebackFrequency`.

---

## Workflow

### 1. Enable `also_log_locally` on target Cobalt metrics

Find the Cobalt project and metric IDs referenced by your Sampler config in
`src/diagnostics/config/sampler/<config>.json5` (which maps to
`third_party/cobalt_config/projects.yaml` and
`third_party/cobalt_config/fuchsia/<project_name>/metrics.yaml`).

Under each target metric's `meta_data:` block in
`third_party/cobalt_config/fuchsia/<project_name>/metrics.yaml`, add
`also_log_locally: true`:

```yaml
  meta_data:
    max_release_stage: GA
    expiration_date: "2026/05/04"
    also_log_locally: true
```

When compiled into `//src/cobalt/bin/app:global_metrics_registry`, Cobalt's
`EventLogger::TraceLogSuccess` logs every recorded event at `INFO` severity from
`core/cobalt` with the prefix `[COBALT_EVENT_TRACE]`.

> **Note:** Past `expiration_date` values in `metrics.yaml` do not need to be
> modified for local validation; `config_parser` only rejects dates more than
> 1 year in the future, and Cobalt's runtime `EventLogger` does not gate
> `also_log_locally` on expiration date.

### 2. Bypass Cobalt's UTC clock gate (`--test_only_use_fake_clock`)

By default, `Logger::Log` in `core/cobalt` checks `validated_clock_->now()`,
which requires `fuchsia::time::SIGNAL_UTC_CLOCK_LOGGING_QUALITY`. On isolated
hardware test bots (or before network time sync completes), Cobalt diverts
incoming events into `UndatedEventManager::Save` **before** calling
`EventLogger::Log`, suppressing `[COBALT_EVENT_TRACE]` output.

Append `"--test_only_use_fake_clock"` to `program.args` in
`src/cobalt/bin/app/meta/cobalt.cml`:

```json5
    program: {
        args: [
            // Exit if lifecycle service cannot be connected to.
            "--require_lifecycle_service",

            // DO NOT SUBMIT: local validation only.
            "--test_only_use_fake_clock",
        ],
    },
```

### 3. Speed up Sampler polling and lower minimum sample rate guards

1.  **Lower `poll_rate_sec` in the target Sampler config(s)** (for example,
    `src/diagnostics/config/sampler/ftl.json5` or `sdmmc.json5`): Change
    `poll_rate_sec` (typically `1800` or `300`) to `5` (or `10`).
   - *Tip:* Archivist's `SampleServer` ticks at the greatest common divisor
     (GCD) of all configured project poll rates (`300`, `600`, `1200`, `1800`,
     `3600`). Choosing `5` or `10` makes the GCD `5s` or `10s` rather than `1s`.
2.  **Lower Sampler's minimum sample rate guard** in
    `src/diagnostics/sampler/meta/sampler.cml`: Change
    `fuchsia.diagnostics.sampler.MinimumSampleRateSec` from `default: 300` to
    `default: 1`.
3.  **Lower Archivist's minimum sample period guard** in
    `src/diagnostics/archivist/configs.gni`: Change
    `fuchsia_diagnostics_sample_min_period_seconds = 60` to `= 1` in
    `default_config` (otherwise `SampleServer::commit` returns
    `ConfigurationError::SamplePeriodTooSmall` and Sampler exits fatally).

### 4. Keep the target hardware session alive across multiple polls

If validating remotely with `fx test-remote`, select a lightweight on-device
test that runs on the target hardware board (such as
`//src/devices/board/drivers/astro:driver-host-enumeration-test-astro` on Astro
or `//src/devices/board/drivers/nelson:driver-host-enumeration-test-nelson` on
Nelson) and add a short soak loop (for example, `6 × 5s = 30s`) so the test
session stays alive across several steady-state Sampler poll cycles:

```rust
// DO NOT SUBMIT: keep the device test alive across multiple 5s Sampler polls.
for _ in 0..6 {
    std::thread::sleep(std::time::Duration::from_secs(5));
}
```

- **Optional (only if you need synthetic `/data` flash writes on every poll):**
  If you are specifically validating storage/FTL write counters (such as
  `ftl_block_operation_events` or `ftl_nand_operation_events`) and want
  guaranteed disk writes during every 5-second soak step beyond boot and package
  resolution, you can temporarily change
  `DelayedLocalAggregateStorage::kDefaultWritebackFrequency` in
  `third_party/cobalt/src/local_aggregation/local_aggregate_storage/delayed_local_aggregate_storage.cc`
  from `std::chrono::minutes(5)` to `std::chrono::seconds(5)`. Leave this
  unchanged for all other metrics to avoid unnecessary flash writes.

### 5. Pre-flight local build check

Before triggering a full remote hardware run, configure `out/test_remote` for
the target builder and compile the modified targets to catch any schema or
manifest errors quickly:

```posix-terminal
fx test-remote --builder core.astro-vg-release --only-set --auto-confirm
fx --dir out/test_remote build \
  //src/cobalt/bin/app:global_metrics_registry \
  //src/diagnostics/config/sampler:default-config \
  //src/diagnostics/sampler \
  //src/diagnostics/archivist:package \
  //src/cobalt/bin/app \
  //src/devices/board/drivers/astro:driver-host-enumeration-test-astro
```

> **Important:**
> Always choose a `core.*` builder (such as `core.astro-vg-release`,
> `core.nelson-debug`, or `core.sherlock-vg-release`), **never** a `bringup.*`
> builder. `bringup.*` uses `FeatureSetLevel::Bootstrap`, which does not include
> `core/sampler` or `core/cobalt`.

### 6. Execute on remote hardware via `fx test-remote`

Run `fx test-remote` with `--auto-confirm` (required for non-interactive
shells):

```posix-terminal
fx test-remote --builder core.astro-vg-release --test driver-host-enumeration-test-astro --auto-confirm
```

Record the Buildbucket build ID printed at the end of stdout
(`https://ci.chromium.org/b/<BUILD_ID>`), and monitor until completion:

```posix-terminal
./prebuilt/tools/buildbucket/bb get <BUILD_ID> -json -A | jq -r .status
```

### 7. Download and analyze device `syslog.txt`

1.  **Locate and download `syslog.txt`** from the test shard step:

   ```posix-terminal
   ./prebuilt/tools/buildbucket/bb get <BUILD_ID> -json -A \
     | jq -r '.steps[] | select(.logs != null) | .name as $s | .logs[] | select(.name | test("syslog")) | "\($s)\t\(.name)"'
   ./prebuilt/tools/buildbucket/bb log <BUILD_ID> "<step_name>" "syslog.txt" > /tmp/device_syslog.txt
   ```

2.  **Verify pipeline health** (must return no matches):

   ```posix-terminal
   rg -n "SamplePeriodTooSmall|Configured sample period was too small|OccurrenceDecreased|Sampler failed to configure|Error converting Inspect to Cobalt|Sample server error" /tmp/device_syslog.txt
   ```

3.  **Inspect `[COBALT_EVENT_TRACE]` emissions across polls**:

   ```posix-terminal
   rg -n -A3 "COBALT_EVENT_TRACE\] \(fuchsia\." /tmp/device_syslog.txt
   ```

   What to look for:
   - **Poll 1 (initial snapshot at startup):** Logs the initial since-boot total
     for `Occurrence` and `IntHistogram` (`OnDiff`) metrics and the current
     value for `Integer` (`Always`) metrics.
   - **Poll 2..N (steady-state polls every 5s):**
     - `Integer` (`Always`) metrics emit on every poll (serving as a 5s
       heartbeat proving Sampler→Cobalt cadence).
     - `Occurrence` and `IntHistogram` (`OnDiff`) metrics emit positive deltas
       (`new - old`) whenever the underlying Inspect counter/histogram changes
       between polls, and are omitted on polls where the value did not change.

### 8. Clean up temporary changes

Revert all temporary validation edits across repositories and remove temporary
log files:

```posix-terminal
git checkout -- \
  src/cobalt/bin/app/meta/cobalt.cml \
  src/diagnostics/config/sampler/ \
  src/diagnostics/sampler/meta/sampler.cml \
  src/diagnostics/archivist/configs.gni \
  src/devices/board/drivers/
git -C third_party/cobalt_config checkout -- .
git -C third_party/cobalt checkout -- .
rm -f /tmp/device_syslog.txt
```
