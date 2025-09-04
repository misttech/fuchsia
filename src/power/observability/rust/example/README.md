# Building and running

Startup:
```
fx set workbench_eng.x64 --with-test //src/power/observability:tests
fx build
ffx emu start --headless
fx serve --background
```

Rebuilding after code changes:
```
fx build && fx ota
```

Collect a sample trace:
```
if ffx component list 2>/dev/null | grep -q state_recorder_rust_example; then
  ffx component destroy /core/ffx-laboratory:state_recorder_rust_example
fi
ffx trace start --categories kernel:meta,power_example --duration 10 &
sleep 0.2
ffx component run \
  /core/ffx-laboratory:state_recorder_rust_example \
  fuchsia-pkg://fuchsia.com/state_recorder_rust_example#meta/state_recorder_rust_example.cm
wait %1
```
