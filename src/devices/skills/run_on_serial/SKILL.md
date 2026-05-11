---
name: run-on-serial
description: >
  Runs serial commands on a Fuchsia device and filters output.  Use when `ffx`
  fails or when direct serial access is needed.
---

# Run on Serial

Guidance for running serial commands on a Fuchsia device using the
`scripts/run_serial_cmd.sh` script.

## Prerequisites

The script relies on environment variables to find the serial socket and log
file.

Required:
- `FUCHSIA_SERIAL_UNIX_SOCKET`
- `FUCHSIA_SERIAL_LOG_FILE`

If these are not set, you **must** ask the user to provide them.

## Executing a Command

Run the script with a single argument (the command string).

### Syntax
```sh
src/devices/skills/run_on_serial/scripts/run_serial_cmd.sh "{command}"
```

### Example
```sh
src/devices/skills/run_on_serial/scripts/run_serial_cmd.sh "driver list"
```

## Parsing Output

Serial output is a shared stream. Use judgement to filter noise.

- **Ignore**: Lines with timestamps and pids (e.g., `[24462.203] 1234:5678>`).
  These are background device logs.
- **Keep**: Lines matching the expected output of your command.

```copy-paste-checklist
- [ ] Verify environment variables are set
- [ ] Execute command via script
- [ ] Parse output, filtering out background logs
```
