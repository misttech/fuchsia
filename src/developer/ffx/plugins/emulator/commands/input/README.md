# `ffx emu input`

`ffx emu input` sends keyboard and mouse events to a running emulator (QEMU engine only). Use it
to check input driver changes on an emulator.

## Usage

```bash
# Keys: QEMU QKeyCode names, joined with '+' for chords
ffx emu input key a
ffx emu input key ctrl+alt+delete
ffx emu input key --down shift
ffx emu input key --up shift

# Text: printable ASCII, US QWERTY layout
ffx emu input text 'Hello, world!'

# Mouse: relative motion, buttons and wheel (needs pointing_device=mouse)
ffx emu input mouse move --dx 10 --dy -5
ffx emu input mouse click left
ffx emu input mouse down left
ffx emu input mouse up left
ffx emu input mouse scroll --dy 3

# Raw: QMP InputEvent objects, sent unchanged in one input-send-event command
ffx emu input raw '[{"type":"rel","data":{"axis":"x","value":7}}]'

# Global options: pick an instance and set the gap between steps
ffx emu input --name fuchsia-emulator --interval 100 key a b c
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

End-to-end with `print-input-report`: the mouse subcommands need a virtual device whose pointing
device is `mouse`, and many products default to `touch`. To get a mouse, copy the product bundle's
virtual device file (`<product bundle>/virtual_devices/<device>.json`), set
`"pointing_device": "mouse"` under `hardware.inputs`, and pass its path to `--device`.

```bash
ffx emu start --engine qemu --headless --device /tmp/mouse_device.json
ffx target ssh -- print-input-report list   # lists the virtio input devices

# Read reports in the background, from all devices, as JSON:
ffx target ssh -- print-input-report read --num-reads 3 --output json > /tmp/pir.json &
sleep 3   # give the reader time to connect

ffx emu input key a                        # a report with KEY_A pressed, then an empty one
ffx emu input mouse move --dx 10 --dy -5   # movement_x=10, movement_y=-5
wait; cat /tmp/pir.json
```
