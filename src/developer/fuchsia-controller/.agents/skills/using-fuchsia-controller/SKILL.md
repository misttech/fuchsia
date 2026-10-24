---
name: using-fuchsia-controller
description: >
  Build and install the fuchsia-controller and FIDL SDK Python wheels into a
  virtual environment (using uv or fuchsia-vendored-python venv) to
  interactively query Fuchsia targets, author standalone reproduction scripts,
  and invoke raw FIDL protocols without modifying BUILD.gn.
---

# Using Fuchsia Controller for Interactive & Reproduction Scripting

This skill guides agents and developers on setting up **Fuchsia Controller**
(`fuchsia_controller_py`) inside a standalone Python virtual environment using
the pre-packaged FIDL SDK wheel targets in `//src/developer/fuchsia-controller`,
and authoring async Python scripts to interact with Fuchsia devices over FIDL.

## When to Use

- Writing standalone reproduction scripts to trigger FIDL calls, race
  conditions, or state transitions on a live Fuchsia device or emulator without
  adding temporary `python_binary` targets to `BUILD.gn`.
- Implementing host-side FIDL servers/watchers (e.g., `WatcherServer` callbacks)
  to inspect events or responses emitted by target components.
- Running quick interactive or scripted FIDL probes against components
  discovered via `ffx component capability`.

---

## 1. Building & Installing the FIDL SDK Wheel (`uv` / `venv`)

`//src/developer/fuchsia-controller/BUILD.gn` provides wheel targets that bundle
`fuchsia-controller` and the Fuchsia FIDL SDK (`partner_idk_stable`,
`partner_idk_unstable`, `prebuilt`, and `host_tool` - ~270 `fidl_fuchsia_*`
libraries) into PEP 427 `.whl` files:

- `//src/developer/fuchsia-controller:fuchsia_controller_combined_wheel` (alias
  `:fuchsia_controller_with_fidl_wheel`): Single combined wheel containing
  `fuchsia_controller_py`, `fuchsia_controller_internal`, `fidl`, and all SDK
  `fidl_fuchsia_*` libraries.
- `//src/developer/fuchsia-controller:fidl_sdk_wheel` (alias `:fidl_wheel`) and
  `//src/developer/fuchsia-controller:fuchsia_controller_wheel`: Separate wheels
  for `fidl-sdk` and `fuchsia-controller`.

### Critical Python ABI Requirement

Because `fuchsia_controller_internal` is a native C++ extension compiled against
Fuchsia's prebuilt Python (`fuchsia-vendored-python`, e.g., `cp314`), you
**must** create the virtual environment using `$(fx
get-workspace-dir)/prebuilt/third_party/python3/linux-x64/bin/python3` (or
`fuchsia-vendored-python`). Using system `/usr/bin/python3` will fail with a
wheel platform/ABI tag mismatch.

### Step 1: Ensure the Combined Wheel is Built

Check if the wheel already exists in the build directory before invoking `fx
build`:

```bash
BUILD_DIR="$(fx get-build-dir)"
WHEEL_DIR="$BUILD_DIR/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel"
ls "$WHEEL_DIR"/fuchsia_controller-*.whl 2>/dev/null || \
  fx build --host //src/developer/fuchsia-controller:fuchsia_controller_combined_wheel
```

### Step 2A: Setup & Execution with `uv` (Preferred when `uv` is available)

```bash
FUCHSIA_PYTHON="$(fx get-workspace-dir)/prebuilt/third_party/python3/linux-x64/bin/python3"
WHEEL_DIR="$(fx get-build-dir)/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel"

# Option 1: Ephemeral single-command script execution via `uv run`
uv run --no-project --python "$FUCHSIA_PYTHON" \
  --no-index --find-links "$WHEEL_DIR" \
  --with fuchsia-controller /tmp/repro.py

# Option 2: Persistent virtualenv in /tmp
uv venv /tmp/fc_repro_venv --python "$FUCHSIA_PYTHON"
uv pip install --python /tmp/fc_repro_venv/bin/python \
  --no-index --find-links "$WHEEL_DIR" fuchsia-controller
/tmp/fc_repro_venv/bin/python /tmp/repro.py
```

### Step 2B: Setup & Execution with `fuchsia-vendored-python -m venv`

When `uv` is not installed on the host, use `fuchsia-vendored-python -m venv`:

```bash
WHEEL_DIR="$(fx get-build-dir)/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel"
VENV_DIR="/tmp/fc_repro_venv"

fuchsia-vendored-python -m venv "$VENV_DIR"
"$VENV_DIR/bin/pip" install --no-index --find-links "$WHEEL_DIR" fuchsia-controller
"$VENV_DIR/bin/python" /tmp/repro.py
```

---

## 2. Discovering Component Monikers & Protocols

Before connecting to a protocol, identify the component moniker exposing it.

- **Fast Lookup (`ffx component list` + `ffx component show <filter>`)**: On
  large product builds (such as `iris`), `ffx component capability` walks all
  components in `RealmQuery` and can hang on unresolved components. Prefer
  filtering `ffx component list` and running `ffx component show <subpath>`:

  ```bash
  ffx component list | grep memory
  ffx component show core/memory_pressure_signaler
  ```

- **Capability Search (with timeout)**: If you do not know the component name,
  wrap `ffx component capability` in `timeout 15`:

  ```bash
  timeout 15 ffx component capability fuchsia.memorypressure.Provider
  ```

---

## 3. Canonical Script Template & `Context` Configuration

### Important `Context` Pitfalls

1.  **Do not pass `target=None` without `target.default` in `config`**: Calling
    `Context(target=None)` initializes an empty local testing FDomain and causes
    `connect_remote_control_proxy()` / `connect_device_proxy()` to fail with
    `RuntimeError: no device has been specified for this Context`.
2.  **Preserve `~/.local/share/Fuchsia/ffx/config/.ffx_user_config.json`**:
    `Context()` defaults to an isolated `IsolateDir()`, which ignores user-level
    `ffx` settings such as USB/Cadmium connectivity (`connectivity.enable_usb`,
    `connectivity.usb_socket_path`) or manual targets (`targets.manual`). Always
    load `.ffx_user_config.json` (stripping any conflicting `target.default`
    key) into `Context(target=target, config=config)`.

### Template

See [`references/repro_template.py`](references/repro_template.py) for a
complete standalone reproduction script template that:

- Loads `~/.local/share/Fuchsia/ffx/config/.ffx_user_config.json` while
  stripping conflicting `target.default` keys.
- Resolves the default target from `ffx target list --format json` when
  `--target` is omitted.
- Connects to Remote Control Service (`ctx.connect_remote_control_proxy()`) and
  individual component capabilities (`ctx.connect_device_proxy(moniker,
  Marker)`).

---

## 4. Implementing Host-Side FIDL Callback / Watcher Servers

When a target FIDL protocol accepts a `client_end:<WatcherProtocol>` (for
example, `fuchsia.memorypressure.Provider.RegisterWatcher`), create a channel
pair with `ctx.channel_create()`, subclass the generated `<Protocol>Server`
class, and spawn `server.serve()` on the event loop while retaining a strong
reference to the task.

See
[`references/watcher_server_template.py`](references/watcher_server_template.py)
for a complete `WatcherServer` implementation example.

---

## 5. Key Rules

1.  **Retain `asyncio.Task` References**: Never call `asyncio.create_task(...)`
    without storing the returned task in a variable; Python's garbage collector
    will destroy and cancel unreferenced tasks mid-execution.
2.  **Never Call `time.sleep()`**: Blocking the thread stalls the Python event
    loop and prevents FDomain handle readiness notifications (`add_reader`) from
    firing. Always use `await asyncio.sleep(...)`.
3.  **Pass Handles via `.take()`**: When passing a `Channel`, `Socket`, or
    `Event` into a FIDL request parameter, call `.take()` to transfer ownership
    of the underlying handle integer.
