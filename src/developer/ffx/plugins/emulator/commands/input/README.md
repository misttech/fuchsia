# `ffx emu input`

`ffx emu input` sends keyboard and mouse events to a running emulator (QEMU engine only). Use it
to check input driver changes on an emulator.

## Usage

```bash
# Raw: QMP InputEvent objects, sent unchanged in one input-send-event command
ffx emu input raw '[{"type":"rel","data":{"axis":"x","value":7}}]'

# Global options: pick an instance
ffx emu input --name fuchsia-emulator raw '[{"type":"rel","data":{"axis":"x","value":7}}]'
```

See `ffx emu input --help` and the help of each subcommand for all options.

## Design

Each subcommand turns its arguments into a list of steps, such as a key press, a key release or a
mouse motion. Every step becomes one QMP `input-send-event` command. The events enter the
emulator's virtual input devices (`virtio-keyboard-pci`, `virtio-mouse-pci`), so they go through
the guest's input drivers. The on-device `input` tool, in contrast, injects events above the
drivers.

All arguments are validated before anything is sent, so bad input never sends a partial sequence.

## Build

```bash
fx build --host //src/developer/ffx/plugins/emulator
```

## Test

Unit tests:

```bash
fx test --host ffx_emulator_input_lib_test ffx_emulator_input_args_lib_test
```

End-to-end with `print-input-report`:

```bash
ffx emu start --engine qemu --headless
ffx target ssh -- print-input-report list   # lists the virtio input devices

# Read reports in the background, from all devices, as JSON:
ffx target ssh -- print-input-report read --num-reads 2 --output json > /tmp/pir.json &
sleep 3   # give the reader time to connect

# A report with KEY_A pressed, then an empty one:
ffx emu input raw '[{"type":"key","data":{"down":true,"key":{"type":"qcode","data":"a"}}}]'
ffx emu input raw '[{"type":"key","data":{"down":false,"key":{"type":"qcode","data":"a"}}}]'
wait; cat /tmp/pir.json
```
