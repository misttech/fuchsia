# odpm_util

`odpm_util` is a command-line diagnostic and configuration utility for On-Device Power Measurement (ODPM) hardware rails on Fuchsia.

It connects to the `fuchsia.hardware.google.odpm.Service` protocol exposed by ODPM drivers to query power, current, energy, rail metadata, and configure Inspect time-series polling intervals.

## Usage

Typically run inside the ODPM driver component's sandbox using `ffx component explore`:

```bash
$ ffx component explore <odpm_moniker>
$ odpm_util read
```

### Commands

#### 1. Read Power Rails (`read`)
Reads power, current, or energy from all discovered ODPM rails, or a specific target rail:

```bash
# Read average power (Watts) across all rails
$ odpm_util read

# Read only a specific rail (e.g. cpu_big)
$ odpm_util read cpu_big

# Read instantaneous power (Watts) instead of average power
$ odpm_util read -i

# Read current (Amperes) instead of power
$ odpm_util read -c

# Read energy (Joules) instead of power
$ odpm_util read -e
```

#### 2. Query or Modify Polling Configuration (`config`)
Queries or dynamically updates hardware polling parameters:

```bash
# Display current polling configuration
$ odpm_util config get

# Set hardware polling interval to 100ms and record Inspect series entry every 1s
$ odpm_util config set --poll-interval-ms 100 --polls-per-series-entry 10

# Reset polling configuration to driver defaults
$ odpm_util config reset
```

#### 3. Query or Multiplex Power Rails (`rails`)
Lists available or currently enabled rails, and dynamically swaps an internal PMIC channel to monitor a different rail:

```bash
# List all available rails on the platform, their active channel status, and swapped-out energy/timestamps
$ odpm_util rails available

# List currently enabled/monitored rails, their channel IDs, and measurement start timestamps
$ odpm_util rails enabled

# Dynamically swap an internal PMIC channel (e.g. Ch 0..11 or Ch 41..52) to a new rail
$ odpm_util rails set 11 buck_13m
$ odpm_util rails set 52 buck_13s
```
