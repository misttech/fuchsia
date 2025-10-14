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

After building and setting up your emulator, run examples using
[`run_example.sh`](//examples/power/state_recorder/run_example.sh):

```
./examples/power/state_recorder/run_example.sh cpp
```
or

```
./examples/power/state_recorder/run_example.sh rust
```
