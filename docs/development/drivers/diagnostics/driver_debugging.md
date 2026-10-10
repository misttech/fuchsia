<!--
Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# Driver Debugging Framework

Fuchsia provides a productionized driver debugging framework that allows developers to author, expose, and interactively execute driver-specific diagnostic and debug commands.

## Architecture

The debugging framework consists of four primary components:

1. **`fuchsia.driver.debug.Debug` FIDL Protocol**: Defines the standardized interface for listing and executing debug commands across drivers.
2. **Helper Libraries (`sdk/lib/driver/debug/rust` & `sdk/lib/driver/debug/cpp`)**: Simplifies the authoring of debug subcommands in Rust (via `argh::SubCommands` and `driver_debug`) and C++ (via `fdf_debug::DebugServer`) with argument parsing, automatic `--help` generation, command discovery, and request dispatching.
3. **`debug` CLI Utility (`src/devices/bin/driver_debug`)**: A host/target CLI tool packaged for interactive execution inside `component explore`.
4. **Driver Integration (`driver/debug.shard.cml`)**: DFv2 drivers include `driver/debug.shard.cml` in their component manifest and export `fuchsia.driver.debug.Debug` via `ServiceFs` or `OutgoingDirectory` in their outgoing directory (`/out/svc/fuchsia.driver.debug.Debug`).

---

## FIDL Protocol (`fuchsia.driver.debug`)

The `fuchsia.driver.debug` protocol exposes two methods:

```fidl
library fuchsia.driver.debug;

using zx;

const MAX_COMMAND_NAME_LENGTH uint32 = 256;
const MAX_ARG_LENGTH uint32 = 1024;
const MAX_ARG_COUNT uint32 = 128;
const MAX_DESCRIPTION_LENGTH uint32 = 1024;
const MAX_COMMAND_COUNT uint32 = 64;

type CommandInfo = table {
    1: name string:MAX_COMMAND_NAME_LENGTH;
    2: description string:MAX_DESCRIPTION_LENGTH;
};

@discoverable
open protocol Debug {
    flexible Execute(resource struct {
        args vector<string:MAX_ARG_LENGTH>:MAX_ARG_COUNT;
        stdout zx.Handle:SOCKET;
        stderr zx.Handle:SOCKET;
    }) -> (struct {
        exit_code int32;
    }) error zx.Status;

    flexible ListCommands() -> (struct {
        commands vector<CommandInfo>:MAX_COMMAND_COUNT;
    }) error zx.Status;
};
```

---

## Implementing Debug Commands in a Rust Driver

### 1. Include the CML Shard in the Driver Manifest

`driver/debug.shard.cml` is not included implicitly for all drivers. Any driver that implements the `fuchsia.driver.debug.Debug` protocol must explicitly include `"driver/debug.shard.cml"` in its `.cml` component manifest.

This shard:
- Declares and exposes the `fuchsia.driver.debug.Debug` protocol capability from `self`.
- Configures the `fuchsia.dash.launcher-tool-urls` facet (`"fuchsia-pkg://fuchsia.com/driver_debug"`) so the `debug` CLI tool is automatically available in `ffx component explore`.

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

### 2. Define Subcommands with `argh`

Define your subcommand structs and top-level subcommand enum with `#[derive(FromArgs)]` and `#[argh(subcommand)]`:

```rust
use argh::FromArgs;

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "ping")]
/// Ping the driver to verify connectivity.
pub struct PingArgs {
    #[argh(option, short = 'c', default = "1", description = "number of pings")]
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
pub enum MyDriverCommands {
    Ping(PingArgs),
    Reset(ResetArgs),
}
```

### 3. Handle Debug Requests

Use `driver_debug::next_command` in a `while let` loop to read parsed subcommands from a `DebugRequestStream`:

```rust
use fidl_fuchsia_driver_debug::DebugRequestStream;

async fn handle_debug(mut stream: DebugRequestStream) -> Result<(), fidl::Error> {
    while let Some((cmd, responder)) = driver_debug::next_command(&mut stream).await? {
        let result: Result<String, anyhow::Error> = match cmd {
            MyDriverCommands::Ping(args) => {
                let mut out = String::new();
                for i in 1..=args.count {
                    out.push_str(&format!("ping {i}\n"));
                }
                Ok(out)
            }
            MyDriverCommands::Reset(args) => {
                if args.hard {
                    // perform hardware reset
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
```

`driver_debug::next_command` automatically:
- Responds to `ListCommands` requests using `argh::SubCommands` metadata (command names and doc comments).
- Handles `--help` flags and syntax/argument parsing errors on `Execute` requests, writing output to `stdout`/`stderr` and returning the appropriate exit code (`0` for help, `2` for syntax errors).

Alternatively, you can use `driver_debug::serve(stream, handler)` with an async closure when you don't need custom stream loop control.

### 4. Serve the Protocol in the Driver

In your driver's `start` method:

```rust
use fidl_fuchsia_driver_debug::DebugRequestStream;
use fuchsia_component::server::ServiceFs;

let mut service_fs = ServiceFs::new();

service_fs.dir("svc").add_fidl_service(move |stream: DebugRequestStream| {
    fuchsia_async::Scope::current().spawn(async move {
        let _ = handle_debug(stream).await;
    });
});

context.serve_outgoing(&mut service_fs)?;
```

---

## Implementing Debug Commands in a C++ Driver (`fdf_debug::DebugServer`)

C++ DFv2 drivers can use the `//sdk/lib/driver/debug/cpp` library to register debug commands and serve `fuchsia.driver.debug.Debug`.

### 1. Add Build Dependency

Add `//sdk/lib/driver/debug/cpp` to your driver's target dependencies in GN (`BUILD.gn`) or Bazel (`BUILD.bazel`):

GN (`BUILD.gn`):

```gn
fuchsia_cc_driver("my_driver") {
  deps = [
    "//sdk/lib/driver/component/cpp",
    "//sdk/lib/driver/debug/cpp",
  ]
}
```

Bazel (`BUILD.bazel`):

```bazel
fuchsia_cc_driver(
    name = "my_driver",
    deps = [
        "//sdk/lib/driver/debug/cpp",
        "@fuchsia_sdk//pkg/driver_component_cpp",
    ],
)
```

### 2. Include Header and Declare `DebugServer`

In your driver class, include `<lib/driver/debug/cpp/debug_server.h>` and add `fdf_debug::DebugServer` as a member:

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/debug/cpp/debug_server.h>

class MyDriver : public fdf::DriverBase2 {
 public:
  MyDriver() : fdf::DriverBase2("my_driver") {}

  zx::result<> Start(fdf::DriverContext context) override;

 private:
  fdf_debug::DebugServer debug_server_;
};
```

### 3. Register Commands and Serve in `Start()`

Register synchronous, asynchronous, or string-returning commands and call `Serve(dispatcher(), outgoing())`:

```cpp
zx::result<> MyDriver::Start(fdf::DriverContext context) {
  // Register synchronous string command:
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

  // Register full ExecutionResult command:
  debug_server_.RegisterCommand(
      "reset", "Reset the driver state.", "[--hard]",
      [](cpp20::span<const std::string> args) -> fdf_debug::ExecutionResult {
        bool hard = false;
        for (const auto& arg : args) {
          if (arg == "--hard") {
            hard = true;
          }
        }
        fdf_debug::ExecutionResult res;
        res.exit_code() = 0;
        res.stdout_() = hard ? "hard reset\n" : "soft reset\n";
        res.stderr_() = "";
        return res;
      });

  // Register asynchronous command:
  debug_server_.RegisterCommand(
      "async_action", "Run async diagnostic test.",
      [this](cpp20::span<const std::string> args,
             fit::callback<void(fdf_debug::ExecutionResult)> completer) {
        // Run asynchronous workload and invoke completer when done
      });

  // Serve Debug protocol on the driver's outgoing directory:
  zx::result result = debug_server_.Serve(dispatcher(), outgoing());
  if (result.is_error()) {
    return result.take_error();
  }

  return zx::ok();
}

FUCHSIA_DRIVER_EXPORT2(MyDriver);
```

---

## Using the `debug` CLI in `component explore`

When debugging a running system, use `ffx component explore` to drop into the driver component namespace:

```sh
$ ffx component explore /bootstrap/boot-drivers:my-driver
```

### List Available Commands

```sh
$ debug list-commands
COMMAND              DESCRIPTION
-------              -----------
ping                 Ping the driver to verify connectivity.
reset                Reset the hardware device state.
```

### Execute Commands

```sh
$ debug ping -c 3
ping 1
ping 2
ping 3
```

```sh
$ debug reset --hard
hard reset completed
```

### Help and Diagnostics

```sh
$ debug --help
Usage: debug <command> [<args>]

Driver debug commands.

Options:
  --help            display usage information

Commands:
  ping              Ping the driver to verify connectivity.
  reset             Reset the hardware device state.
```

### Return Codes

- `0`: Success (or `--help` output printed to standard output).
- `1`: Driver internal execution error / handler failure (details in standard error).
- `2`: Syntax or argument parsing error (details in standard error).
