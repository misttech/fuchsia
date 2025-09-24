# State Recorder examples

This directory provides examples for the [Rust and C++ state recorder
libraries](//src/power/observability/README.md).

## Building and initialization

First, build and set up an emulator and server as follows:
```
fx set workbench_eng.x64 --with-test //src/power/observability:tests
fx build
ffx emu start --headless
fx serve --background
```

## Rebuilding

If you need to rebuild, be sure to OTA before you re-run:
```
fx build && fx ota
```

## Running

To run one of the examples, define this function:

```
function run_example() {
  LANG=$1  # "rust" or "cpp"
  EXAMPLE="state_recorder_${LANG}_example"
  if ffx component list 2>/dev/null | grep -q ${EXAMPLE}$; then
    ffx component destroy /core/ffx-laboratory:${EXAMPLE}
  fi
  ffx trace start --categories kernel:meta,power_example --duration 10 &
  sleep 0.2
  ffx component run \
    /core/ffx-laboratory:${EXAMPLE} \
    "fuchsia-pkg://fuchsia.com/${EXAMPLE}#meta/${EXAMPLE}.cm"
  wait %1
}
```

Then run the example for the language of interest with either `run_example cpp`
or `run_example rust`.
