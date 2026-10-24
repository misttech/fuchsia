# Fuchsia Controller

`fuchsia-controller` (`fuchsia_controller_py`) is a host-side Python library for
interacting with Fuchsia target devices over FDomain/RCS and invoking FIDL
protocols directly from Python.

## Building Python Wheels for Interactive & Virtualenv Usage

`//src/developer/fuchsia-controller/BUILD.gn` defines PEP 427 Python wheel
targets that package `fuchsia-controller` and the Fuchsia FIDL SDK (`partner_idk`,
`prebuilt`, and `host_tool` FIDL libraries) so they can be installed into any
virtual environment pinned to `fuchsia-vendored-python`:

* `//src/developer/fuchsia-controller:fuchsia_controller_combined_wheel` (alias
  `:fuchsia_controller_with_fidl_wheel`): Builds a single self-contained
  `fuchsia_controller-*.whl` containing `fuchsia_controller_py`,
  `fuchsia_controller_internal`, `fidl`, and all ~270 SDK `fidl_fuchsia_*`
  Python modules.
* `//src/developer/fuchsia-controller:fuchsia_controller_wheel` and
  `//src/developer/fuchsia-controller:fidl_sdk_wheel` (alias `:fidl_wheel`):
  Separate wheels if you prefer to install `fuchsia-controller` and `fidl-sdk`
  independently.

Build the combined wheel once:

```sh
fx build --host //src/developer/fuchsia-controller:fuchsia_controller_combined_wheel
```

The generated wheel is placed at:

```text
$(fx get-build-dir)/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel/fuchsia_controller-0.1.0-*.whl
```

## Setting Up a Virtual Environment (`uv` or `venv`)

Because `fuchsia_controller_internal` is a native C++ extension compiled against
Fuchsia's prebuilt Python interpreter (`fuchsia-vendored-python`), the virtual
environment **must** use Fuchsia's vendored Python binary rather than the system
`/usr/bin/python3`.

### Option 1: Using `uv` (Recommended)

```sh
FUCHSIA_PYTHON="$(fx get-workspace-dir)/prebuilt/third_party/python3/linux-x64/bin/python3"
WHEEL_DIR="$(fx get-build-dir)/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel"

# Create a persistent virtual environment and install the combined wheel:
uv venv /tmp/fc_venv --python "$FUCHSIA_PYTHON"
uv pip install --python /tmp/fc_venv/bin/python \
  --no-index --find-links "$WHEEL_DIR" fuchsia-controller

# Launch an interactive Python REPL (python -m asyncio for top-level await):
/tmp/fc_venv/bin/python -m asyncio

# Or run a standalone script ephemerally in a single command without creating a named venv:
uv run --no-project --python "$FUCHSIA_PYTHON" \
  --no-index --find-links "$WHEEL_DIR" \
  --with fuchsia-controller my_script.py
```

### Option 2: Using `fuchsia-vendored-python -m venv`

```sh
WHEEL_DIR="$(fx get-build-dir)/host_x64/obj/src/developer/fuchsia-controller/fuchsia_controller_combined_wheel"

fuchsia-vendored-python -m venv /tmp/fc_venv
/tmp/fc_venv/bin/pip install --no-index --find-links "$WHEEL_DIR" fuchsia-controller

# Launch an interactive async REPL or run a script:
/tmp/fc_venv/bin/python -m asyncio
/tmp/fc_venv/bin/python my_script.py
```

## Interactive & Standalone Script Example

By default, `Context()` creates a temporary isolated `IsolateDir()`, which does
not load `~/.local/share/Fuchsia/ffx/config/.ffx_user_config.json` and creates a
local testing FDomain if `target=None`. When connecting to a live target (such
as an emulator, SSH target, or USB/Cadmium device), pass an explicit `target`
nodename/address and load `.ffx_user_config.json`:

```python
import asyncio
import json
import pathlib
import subprocess
from typing import Any

from fuchsia_controller_py import Context
import fidl_fuchsia_buildinfo as f_buildinfo
import fidl_fuchsia_developer_remotecontrol as f_remotecontrol


def create_device_context(target: str | None = None) -> Context:
    """Creates a Context preserving user-level ffx connectivity settings."""
    user_cfg_path = (
        pathlib.Path.home()
        / ".local/share/Fuchsia/ffx/config/.ffx_user_config.json"
    )
    config: dict[str, Any] = (
        json.loads(user_cfg_path.read_text()) if user_cfg_path.exists() else {}
    )
    config.pop("target.default", None)
    if isinstance(config.get("target"), dict):
        config["target"].pop("default", None)
        if not config["target"]:
            config.pop("target", None)
    else:
        config.pop("target", None)

    if not target:
        out = subprocess.check_output(
            ["ffx", "target", "list", "--format", "json"], text=True
        )
        targets = json.loads(out)
        defaults = [t for t in targets if t.get("is_default")]
        chosen = defaults[0] if defaults else (targets[0] if targets else None)
        if not chosen:
            raise RuntimeError("No Fuchsia target found via `ffx target list`")
        target = chosen["nodename"]

    return Context(target=target, config=config)


async def main() -> None:
    ctx = create_device_context()

    # Query RemoteControl (RCS)
    rcs = f_remotecontrol.RemoteControlClient(ctx.connect_remote_control_proxy())
    ident = (await rcs.identify_host()).unwrap()
    print(f"Connected to nodename: {ident.response.nodename}")

    # Connect to any component capability by moniker and Marker
    build_info = f_buildinfo.ProviderClient(
        ctx.connect_device_proxy("core/build-info", f_buildinfo.ProviderMarker)
    )
    info = await build_info.get_build_info()
    print(f"Product: {info.build_info.product_config}, Board: {info.build_info.board_config}")


if __name__ == "__main__":
    asyncio.run(main())
```
