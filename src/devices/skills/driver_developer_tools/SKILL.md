---
name: driver-developer-tools
description: >
  Implement interactive developer and diagnostic CLI tools for a Fuchsia DFv2
  driver using either in-driver debug subcommands via the
  fuchsia.driver.debug/Debug protocol (with driver_debug in Rust or
  fdf_debug::DebugServer in C++ and driver/debug.shard.cml) or a dedicated CLI
  binary registered in the fuchsia.dash.launcher-tool-urls facet for execution
  inside ffx component explore.
---

# Implement Driver Developer Tools

## Dependencies

### In-Driver Debug Protocol ([`fuchsia.driver.debug/Debug`](/sdk/fidl/fuchsia.driver.debug/debug.fidl))

**GN**

```gn
# Rust drivers
deps = [
  # Provides fdf_component::Driver and DriverContext
  "//sdk/lib/driver/component/rust",
  # Provides driver_debug::next_command, driver_debug::serve, and re-exports
  "//sdk/lib/driver/debug/rust",
  # Provides fuchsia_async::Scope
  "//src/lib/fuchsia-async",
  # Provides ServiceFs for serving the outgoing directory
  "//src/lib/fuchsia-component",
  # Provides StreamExt for driving ServiceFs
  "//third_party/rust_crates:futures",
]

# C++ drivers
deps = [
  # Provides fdf::DriverBase2 and outgoing()
  "//sdk/lib/driver/component/cpp",
  # Provides fdf_debug::DebugServer and fdf_debug::ExecutionResult
  "//sdk/lib/driver/debug/cpp",
]
```

**Bazel**

```bazel
# Rust drivers
deps = [
    "//sdk/lib/driver/component/rust",
    "//sdk/lib/driver/debug/rust",
    "//src/lib/fuchsia-async",
    "//src/lib/fuchsia-component",
    "//third_party/rust_crates:futures",
]

# C++ drivers
deps = [
    "@fuchsia_sdk//pkg/driver_component_cpp",
    "@fuchsia_sdk//pkg/driver_debug_cpp",
]
```

### Dedicated CLI Binary (`fuchsia.dash.launcher-tool-urls`)

**GN**

```gn
# Rust CLI binary
deps = [
  # Add bindings for the domain-specific FIDL library
  "//sdk/fidl/fuchsia.hardware.example:fuchsia.hardware.example_rust",
  # Provides zx::Status
  "//sdk/rust/zx",
  # Provides #[fuchsia::main]
  "//src/lib/fuchsia",
  # Provides connect_to_protocol_at_path and Service client helpers
  "//src/lib/fuchsia-component",
  # Provides error context handling
  "//third_party/rust_crates:anyhow",
  # Provides CLI argument parsing
  "//third_party/rust_crates:argh",
]

# C++ CLI binary
deps = [
  # Add bindings for the domain-specific FIDL library
  "//sdk/fidl/fuchsia.hardware.example:fuchsia.hardware.example_cpp",
  # Provides component::OpenServiceRoot and component::ConnectAtMember
  "//sdk/lib/component/incoming/cpp",
  # Provides fdio directory operations
  "//sdk/lib/fdio",
]
```

**Bazel**

```bazel
# Rust CLI binary
deps = [
    "@fuchsia_sdk//fidl/fuchsia.hardware.example:fuchsia.hardware.example_rust",
    "//sdk/rust/zx",
    "//src/lib/fuchsia",
    "//src/lib/fuchsia-component",
    "//third_party/rust_crates:anyhow",
    "//third_party/rust_crates:argh",
]

# C++ CLI binary
deps = [
    "@fuchsia_sdk//fidl/fuchsia.hardware.example:fuchsia.hardware.example_cpp",
    "@fuchsia_sdk//pkg/component_incoming_cpp",
    "@fuchsia_sdk//pkg/fdio",
]
```

## Select a Developer Tool Architecture

#### **If** implementing driver-specific diagnostic commands, internal state inspection, or lightweight subcommands for a single driver (Recommended):

Implement the
[`fuchsia.driver.debug/Debug`](/sdk/fidl/fuchsia.driver.debug/debug.fidl)
protocol directly inside the driver using
[`driver_debug`](/sdk/lib/driver/debug/rust/src/lib.rs) (Rust) or
[`fdf_debug::DebugServer`](/sdk/lib/driver/debug/cpp/include/lib/driver/debug/cpp/debug_server.h)
(C++), and include [`driver/debug.shard.cml`](/sdk/lib/driver/debug.shard.cml)
in the driver's component manifest.

* **No custom FIDL or separate binary**: The driver uses the standardized
  `fuchsia.driver.debug/Debug` protocol (`fuchsia.driver.debug/Debug.Execute`
  and `fuchsia.driver.debug/Debug.ListCommands`) and the shared
  [`debug`](/src/devices/bin/driver_debug/src/main.rs) CLI tool
  (`fuchsia-pkg://fuchsia.com/driver_debug`).
* **Direct access to internal driver state**: Command handlers run inside the
  driver process and can inspect or mutate driver-private state, hardware
  registers, and internal queues without exposing those operations over a public
  FIDL API.
* **Built-in discovery and help**: The helper libraries automatically implement
  `fuchsia.driver.debug/Debug.ListCommands`, `--help` generation, argument
  parsing, and standard exit codes (`0` for success/help, `1` for execution
  error, `2` for syntax error) streamed over `stdout` and `stderr` sockets.

#### **Otherwise** (If building a standalone CLI for a domain-specific FIDL protocol or service shared across multiple drivers or components):

Build a dedicated CLI binary package (such as
[`resetctl`](/src/devices/reset/bin/resetctl/main.cc) or
[`usb-cli`](/src/devices/usb/bin/usb-cli/src/main.rs)) and register its package
URL in the `fuchsia.dash.launcher-tool-urls` facet of the component manifest.

* **Reusable across multiple providers and consumers**: A single CLI tool (for
  example, `resetctl` for `fuchsia.hardware.reset/Service`) works with any
  driver that exposes the FIDL capability in `/out/svc` or any client component
  that consumes it in `/svc`.
* **Rich client-side workflows**: Suits tools that aggregate data from multiple
  protocols (such as combining FIDL calls with
  `fuchsia.diagnostics/ArchiveAccessor` Inspect queries), format complex
  JSON/tables, or already have an established domain-specific FIDL protocol.
* **No debug-only FIDL server code in the driver**: The driver only serves its
  standard hardware or policy FIDL protocol without adding
  `fuchsia.driver.debug/Debug` handlers.

---

## Option A: Implement In-Driver Debug Commands via `fuchsia.driver.debug/Debug`

### 1. Include the Debug Shard in the Driver Manifest

Include [`driver/debug.shard.cml`](/sdk/lib/driver/debug.shard.cml) in the
driver's `.cml` file. This shard declares and exposes
`fuchsia.driver.debug.Debug` from `self` and sets
`"fuchsia.dash.launcher-tool-urls": [ "fuchsia-pkg://fuchsia.com/driver_debug"
]` so `ffx component explore` automatically loads the
[`debug`](/src/devices/bin/driver_debug/src/main.rs) binary.

```json5
{
    include: [
        "driver/debug.shard.cml",
        "driver_component/driver.shard.cml",
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/my_driver.so",
        bind: "meta/bind/my_driver.bindbc",
    },
}
```

### 2. Define and Serve Debug Subcommands in the Driver

#### **If** the driver is written in Rust:

Define subcommands with `argh::FromArgs` and `#[argh(subcommand)]`, handle
incoming requests with
[`driver_debug::next_command`](/sdk/lib/driver/debug/rust/src/lib.rs) or
[`driver_debug::serve`](/sdk/lib/driver/debug/rust/src/lib.rs), and expose
`DebugRequestStream` in `/out/svc` using `ServiceFs`:

```rust
use driver_debug::argh::FromArgs;
use driver_debug::fidl_fuchsia_driver_debug::DebugRequestStream;
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "ping")]
/// Ping the driver to check responsiveness.
pub struct PingArgs {
    #[argh(option, short = 'c', default = "1", description = "ping count")]
    pub count: u32,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "reset")]
/// Reset the hardware device state.
pub struct ResetArgs {
    #[argh(switch, description = "perform a hard reset")]
    pub hard: bool,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum DriverCommands {
    Ping(PingArgs),
    Reset(ResetArgs),
}

pub struct MyDriver {
    _node: Node,
    _scope: fuchsia_async::Scope,
}

async fn handle_debug(mut stream: DebugRequestStream) -> Result<(), driver_debug::fidl::Error> {
    while let Some((cmd, responder)) = driver_debug::next_command(&mut stream).await? {
        let result: Result<String, driver_debug::anyhow::Error> = match cmd {
            DriverCommands::Ping(args) => {
                let mut out = String::new();
                for i in 1..=args.count {
                    out.push_str(&format!("ping {i}\n"));
                }
                Ok(out)
            }
            DriverCommands::Reset(args) => {
                if args.hard {
                    Ok("hard reset completed\n".to_string())
                } else {
                    Ok("soft reset completed\n".to_string())
                }
            }
        };
        responder.send(result)?;
    }
    Ok(())
}

impl Driver for MyDriver {
    const NAME: &str = "my_driver";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;
        let scope = fuchsia_async::Scope::new_with_name("my_driver_scope");
        let scope_handle = scope.to_handle();
        let mut service_fs = ServiceFs::new();

        service_fs.dir("svc").add_fidl_service(move |stream: DebugRequestStream| {
            scope_handle.spawn(async move {
                let _: Result<(), driver_debug::fidl::Error> = handle_debug(stream).await;
            });
        });

        context.serve_outgoing(&mut service_fs)?;
        scope.spawn(service_fs.collect::<()>());

        Ok(Self { _node: node, _scope: scope })
    }

    async fn stop(&self) {}
}

driver_register!(MyDriver);
```

#### **Otherwise** (If the driver is written in C++):

Add an
[`fdf_debug::DebugServer`](/sdk/lib/driver/debug/cpp/include/lib/driver/debug/cpp/debug_server.h)
member to the [`fdf::DriverBase2`](/sdk/lib/driver/component/cpp/driver_base2.h)
class, register synchronous or asynchronous command handlers with
`RegisterCommand`, and call `Serve(dispatcher(), outgoing())` during `Start`:

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/debug/cpp/debug_server.h>

class MyDriver : public fdf::DriverBase2 {
 public:
  MyDriver() : fdf::DriverBase2("my_driver") {}

  zx::result<> Start(fdf::DriverContext context) override {
    // Register a synchronous handler returning fit::result<std::string, std::string>.
    debug_server_.RegisterCommand(
        "ping", "Ping the driver.", "[-c <count>]",
        [](cpp20::span<const std::string> args) -> fit::result<std::string, std::string> {
          int count = 1;
          for (size_t i = 0; i < args.size(); ++i) {
            if (args[i] == "-c" && i + 1 < args.size()) {
              count = std::stoi(std::string(args[++i]));
            } else {
              return fit::error("Unknown option: " + std::string(args[i]));
            }
          }
          std::string out;
          for (int i = 1; i <= count; ++i) {
            out += "ping " + std::to_string(i) + "\n";
          }
          return fit::ok(out);
        });

    // Register an asynchronous handler for commands that perform async FIDL or hardware work.
    debug_server_.RegisterCommand(
        "reset", "Reset the hardware device state.", "[--hard]",
        [this](cpp20::span<const std::string> args,
               fit::callback<void(fdf_debug::ExecutionResult)> completer) {
          bool hard = false;
          for (const auto& arg : args) {
            if (arg == "--hard") {
              hard = true;
            }
          }
          fdf_debug::ExecutionResult res;
          res.exit_code() = 0;
          res.stdout_() = hard ? "hard reset completed\n" : "soft reset completed\n";
          completer(std::move(res));
        });

    zx::result result = debug_server_.Serve(dispatcher(), outgoing());
    if (result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

 private:
  fdf_debug::DebugServer debug_server_;
};

FUCHSIA_DRIVER_EXPORT2(MyDriver);
```

### 3. Execute Debug Commands in `ffx component explore`

Ensure the [`driver_debug`](/src/devices/bin/driver_debug/BUILD.gn) package is
available in the build (`//src/devices/bin/driver_debug`), then explore the
target driver component:

```bash
# Interactive session:
ffx component explore /bootstrap/boot-drivers:dev.sys.platform.my-driver
$ debug --list-commands
$ debug ping -c 3
$ debug reset --hard

# Non-interactive one-off command from the host:
ffx component explore /bootstrap/boot-drivers:dev.sys.platform.my-driver -c "debug ping -c 3"
```

---

## Option B: Implement a Dedicated CLI Binary via `fuchsia.dash.launcher-tool-urls`

### 1. Expose Capability and Register Tool Facet in the Manifest

In the driver's (or service component's) `.cml` manifest, expose the
domain-specific FIDL service or protocol and add the tool's package URL to the
`fuchsia.dash.launcher-tool-urls` facet (see
[`msd_arm.shard.cml`](/src/graphics/drivers/msd-arm-mali/meta/msd_arm.shard.cml)
and [`usb-policy.cml`](/src/devices/usb/usb-policy/meta/usb-policy.cml) for
in-tree examples):

```json5
{
    include: [
        "driver_component/driver.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/my_reset_driver.so",
        bind: "meta/bind/my_reset_driver.bindbc",
    },
    capabilities: [
        { service: "fuchsia.hardware.reset.Service" },
    ],
    expose: [
        {
            service: "fuchsia.hardware.reset.Service",
            from: "self",
        },
    ],
    facets: {
        "fuchsia.dash.launcher-tool-urls": [
            "fuchsia-pkg://fuchsia.com/resetctl",
        ],
    },
}
```

### 2. Connect to the Explore Namespace in the CLI Binary

Inside `ffx component explore`, the default namespace layout mounts the
component's outgoing directory at `/out` (`/out/svc/...`), its exposed directory
at `/exposed` (`/exposed/...`), and its incoming namespace at `/ns/svc` (or
`/svc`). A CLI tool should check both `/out/svc` (when exploring the provider
driver) and `/svc` or `/exposed` (when exploring a consumer component) so it
works in either context.

#### **If** writing the CLI binary in Rust:

Define the CLI interface using `argh` and connect to the capability path in
`/out/svc`, `/exposed`, or `/svc` (modeled on
[`usb-cli`](/src/devices/usb/bin/usb-cli/src/main.rs) and
[`driver_debug`](/src/devices/bin/driver_debug/src/lib.rs)):

```rust
use anyhow::{Context, Result};
use argh::FromArgs;
use fidl_fuchsia_hardware_example as fexample;
use fuchsia_component::client::connect_to_protocol_at_path;
use std::path::Path;

const OUT_SVC_PATH: &str = "/out/svc/fuchsia.hardware.example.Device";
const EXPOSED_PATH: &str = "/exposed/fuchsia.hardware.example.Device";
const SVC_PATH: &str = "/svc/fuchsia.hardware.example.Device";

#[derive(FromArgs, Debug, PartialEq)]
/// CLI tool for interacting with fuchsia.hardware.example/Device.
struct CliArgs {
    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum SubCommand {
    Status(StatusArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "status")]
/// Query device status.
struct StatusArgs {}

fn connect_device() -> Result<fexample::DeviceProxy> {
    for path in [OUT_SVC_PATH, EXPOSED_PATH, SVC_PATH] {
        if Path::new(path).exists() {
            return connect_to_protocol_at_path::<fexample::DeviceMarker>(path)
                .with_context(|| format!("Failed to connect at {path}"));
        }
    }
    anyhow::bail!("Device protocol not found in /out/svc, /exposed, or /svc")
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: CliArgs = argh::from_env();
    let proxy = connect_device()?;
    match args.subcommand {
        SubCommand::Status(_) => {
            // Invoke FIDL method on proxy and print output.
            let _ = proxy;
        }
    }
    Ok(())
}
```

#### **Otherwise** (If writing the CLI binary in C++):

Inspect `/svc` and `/out/svc` using `std::filesystem::exists` and connect via
`component::OpenServiceRoot` and `component::ConnectAtMember` (modeled on
[`resetctl`](/src/devices/reset/bin/resetctl/main.cc)):

```cpp
#include <fidl/fuchsia.hardware.reset/cpp/wire.h>
#include <lib/component/incoming/cpp/service.h>

#include <filesystem>
#include <print>
#include <string>

int main(int argc, const char* argv[]) {
  if (argc < 2) {
    std::println(stderr, "Usage: {} <instance_name>", argv[0]);
    return 1;
  }

  std::string instance = argv[1];
  std::string svc_path = "/svc/fuchsia.hardware.reset.Service/" + instance;
  std::string out_svc_path = "/out/svc/fuchsia.hardware.reset.Service/" + instance;

  std::string chosen_root;
  if (std::filesystem::exists(out_svc_path)) {
    chosen_root = "/out/svc";
  } else if (std::filesystem::exists(svc_path)) {
    chosen_root = "/svc";
  } else {
    std::println(stderr, "Service instance '{}' not found in /out/svc or /svc", instance);
    return 1;
  }

  zx::result svc_dir = component::OpenServiceRoot(chosen_root);
  if (svc_dir.is_error()) {
    std::println(stderr, "Failed to open {}: {}", chosen_root, svc_dir.status_string());
    return 1;
  }

  zx::result client_end =
      component::ConnectAtMember<fuchsia_hardware_reset::Service::Reset>(*svc_dir, instance);
  if (client_end.is_error()) {
    std::println(stderr, "Failed to connect: {}", client_end.status_string());
    return 1;
  }

  fidl::WireSyncClient client(std::move(*client_end));
  // Issue synchronous FIDL calls on `client`.
  return 0;
}
```

### 3. Define the Tool Binary and Package Targets

Package the executable as a `fuchsia_package` so `ffx component explore` can
resolve its `fuchsia-pkg://` URL (see
[`resetctl/BUILD.gn`](/src/devices/reset/bin/resetctl/BUILD.gn) and
[`usb-cli/BUILD.gn`](/src/devices/usb/bin/usb-cli/BUILD.gn)):

```gn
import("//build/components.gni")
import("//build/rust/rustc_binary.gni")

rustc_binary("my_tool_bin") {
  name = "my-tool"
  edition = "2024"
  with_unit_tests = true
  deps = [
    "//sdk/fidl/fuchsia.hardware.example:fuchsia.hardware.example_rust",
    "//sdk/rust/zx",
    "//src/lib/fuchsia",
    "//src/lib/fuchsia-component",
    "//third_party/rust_crates:anyhow",
    "//third_party/rust_crates:argh",
  ]
  sources = [ "src/main.rs" ]
}

fuchsia_package("my-tool") {
  deps = [ ":my_tool_bin" ]
}
```

### 4. Invoke the Tool in `ffx component explore`

Ensure the tool package is included in the build graph and package server, then
run `ffx component explore`:

```bash
# If the package URL is listed in fuchsia.dash.launcher-tool-urls:
ffx component explore <driver_moniker> -c "my-tool status"

# Or attach the tool package ad-hoc to any component without modifying its .cml:
ffx component explore <driver_moniker> --tools fuchsia-pkg://fuchsia.com/my-tool
```

## Common Pitfalls

* **Omitting [`driver/debug.shard.cml`](/sdk/lib/driver/debug.shard.cml) when
  using `fuchsia.driver.debug/Debug`**: `driver/debug.shard.cml` is not included
  automatically by
  [`driver_component/driver.shard.cml`](/sdk/lib/driver_component/driver.shard.cml).
  Without it, the `fuchsia.driver.debug.Debug` capability is not exposed and
  `ffx component explore` will not load the `debug` CLI tool via
  `fuchsia.dash.launcher-tool-urls`.
* **Forgetting to poll `ServiceFs` in Rust drivers**: Calling
  `context.serve_outgoing(&mut service_fs)?` only binds the outgoing directory
  handle. The driver must also spawn `service_fs.collect::<()>()` on its
  `fuchsia_async::Scope` and retain the `Scope` in the driver struct, or
  incoming `fuchsia.driver.debug/Debug` connections will hang.
* **Hardcoding only `/svc` in a dedicated CLI tool**: When exploring a driver
  that *provides* a capability, the capability is mounted under `/out/svc` (and
  `/exposed`), not `/svc`. Check `/out/svc` (or `/exposed`) in addition to
  `/svc` so the CLI works when exploring either the provider driver or a
  consumer component.
* **Missing tool package in the active package repository**:
  `fuchsia.dash.launcher-tool-urls` resolves `fuchsia-pkg://` URLs on demand
  when `ffx component explore` starts. If the tool package (such as
  `//src/devices/bin/driver_debug` or a custom `fuchsia_package`) is not in the
  build graph or the package server is not running, `ffx component explore`
  prints a warning and the command will not be on `PATH`.
* **Detaching async tasks for debug streams**: Never use `.detach()` when
  spawning FIDL stream handlers in Rust drivers. Spawn request handlers on a
  `fuchsia_async::Scope` owned by the driver struct so tasks are canceled
  cleanly when the driver stops.

## Further Reading

* [Driver Debugging Framework
  Documentation](/docs/development/drivers/diagnostics/driver_debugging.md)
* [Explore Components with `ffx component
  explore`](/docs/development/tools/ffx/workflows/explore-components.md)
* [`fuchsia.driver.debug/Debug` FIDL
  Definition](/sdk/fidl/fuchsia.driver.debug/debug.fidl)
* [Rust `driver_debug` Helper Library](/sdk/lib/driver/debug/rust/src/lib.rs)
* [C++ `fdf_debug::DebugServer` Helper
  Library](/sdk/lib/driver/debug/cpp/include/lib/driver/debug/cpp/debug_server.h)
* [`debug` CLI Binary Implementation](/src/devices/bin/driver_debug/README.md)
* [`resetctl` Dedicated CLI Example](/src/devices/reset/bin/resetctl/README.md)
* [`usb-cli` Dedicated CLI Example](/src/devices/usb/bin/usb-cli/src/main.rs)
* [Serve FIDL in a Rust
  Driver](/src/devices/skills/driver_fidl/server/implementation/rust/SKILL.md)
* [Serve FIDL in a C++
  Driver](/src/devices/skills/driver_fidl/server/implementation/cpp/SKILL.md)
