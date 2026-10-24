# `ffx` Development Guide for AI Agents

`ffx` (Fuchsia Command Line Tools) is the primary host-side developer tool for interacting with Fuchsia target devices, product bundles, emulators, and build artifacts. When writing, refactoring, or reviewing code under `//src/developer/ffx`, follow these architecture rules and team best practices.

---

## 1. Subtool Architecture & Code Organization

### External Subtools (`tools/`) over Built-in Plugins (`plugins/`)
* **New subtools belong in `//src/developer/ffx/tools/`** (or in the owning team's subsystem directory with `file:/src/developer/ffx/OWNERS` included in `OWNERS`), built as standalone binaries using the `ffx_tool` GN template (`//src/developer/ffx/build/ffx_tool.gni`).
* **Avoid adding new subtools to `//src/developer/ffx/plugins/`**: The `plugins/` directory is for legacy built-in commands compiled directly into the main `ffx` binary. Only add to `plugins/` if explicitly required.
* **Shared libraries belong in `//src/developer/ffx/lib/`**: Reusable domain logic, target connection handling, protocol wrappers, and configuration schemas should live in `lib/` crates rather than inside individual subtools.

### Split Subtools into `lib.rs` and `main.rs`
Structure every subtool as a library crate (`rustc_library("lib")` with `with_unit_tests = true`) paired with a thin `ffx_tool` binary wrapper:
* **`src/main.rs`**: Minimal entry point invoking FHO:
  ```rust
  use ffx_tool_example::ExampleTool;
  use fho::FfxTool;

  #[fuchsia_async::run_singlethreaded]
  async fn main() {
      ExampleTool::execute_tool().await
  }
  ```
* **`src/lib.rs`**: Defines the `argh` command struct (`#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]`), the `#[derive(FfxTool)]` struct, the `FfxMain` implementation, and unit tests.
* **Rust Edition**: Use `edition = "2024"` in all new `BUILD.gn` targets.

---

## 2. Daemonless Architecture & FDomain Target Connections

`ffx` operates on a **daemonless, direct-connection architecture**.

### Do Not Use Legacy Daemon or Overnet APIs
* **No `ffx-daemon` (`DaemonProxy`)**: Never introduce dependencies on `DaemonProxy`, `fidl_fuchsia_developer_ffx::DaemonProxy`, or `daemon.*` configuration keys.
* **No Host FIDL (`fuchsia.developer.ffx`) for Target/Discovery State**: Do not use legacy `fidl_fuchsia_developer_ffx::TargetInfo` or `TargetProxy`. Use native Rust domain types from:
  * `//src/developer/ffx/lib/discovery` (`TargetHandle`, `TargetEvent`)
  * `//src/developer/ffx/lib/target` (`TargetInfo`, `TargetInfoQuery`)
  * `//src/developer/ffx/lib/mdns_discovery` (`MdnsTargetInfo`)
* **Use FDomain (`*_rust_fdomain`) Instead of Overnet (`*_rust`)**:
  * Target FIDL communication uses **FDomain** (`fdomain_client`) over direct SSH, VSOCK, or USB transports.
  * In `BUILD.gn`, depend on the `_rust_fdomain` target of a FIDL library (e.g., `//sdk/fidl/fuchsia.device:fuchsia.device_rust_fdomain`) and import the `fdomain_fuchsia_*` crate in Rust.

---

## 3. FHO (`//src/developer/ffx/lib/fho`) & Dependency Injection

Subtools use FHO (`FfxTool` and `FfxMain`) to declaratively inject environment context, target proxies, and configuration.

```rust
use argh::{ArgsInfo, FromArgs};
use fdomain_fuchsia_device::NameProviderProxy;
use ffx_writer::{ToolIO as _, VerifiedMachineWriter};
use fho::{FfxContext, FfxMain, FfxTool, Result};
use target_holders::moniker;

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "example", description = "example ffx subtool")]
pub struct ExampleCommand {}

#[derive(FfxTool)]
pub struct ExampleTool {
    #[command]
    cmd: ExampleCommand,
    #[with(moniker("/core/system-update"))]
    proxy: NameProviderProxy,
}
```

### Target Connection Declarations & Holders (`//src/developer/ffx/lib/target/holders`)
* **Tools that do NOT talk to a target device**: Always annotate the `FfxTool` struct with `#[target(None)]`. This is required so `ffx --strict` does not demand a `--target` argument when running the command.
* **Immediate Target Injection**: Use `#[with(moniker("..."))]` or `#[with(toolbox())]` on a FIDL proxy field (or inject `RemoteControlProxyHolder`, `NodenameHolder`, `SshAddrHolder`, `HostAddrHolder`) when every invocation of the tool needs the target.
* **Conditional / Lazy Target Injection (`fho::Deferred<T>`)**: When a tool has subcommands or code paths that may not require a target connection, wrap the proxy or holder in `fho::Deferred<T>` (or `#[with(fho::deferred(moniker("...")))]`) and `.await?` it only on the branch that needs it.
* **Resilient Multi-Shot Reconnection (`Connector<T>`)**: For workflows that reboot or flash the target device and must reconnect across disconnects, use `Connector<RemoteControlProxyHolder>` or `DirectConnector` (`try_connect()`).

---

## 4. Structured Output & Writers (`//src/developer/ffx/lib/writer`)

Never use `println!` or `eprintln!` for tool output. Always write through the `Writer` passed to `FfxMain::main`.

### Prefer Supporting Both Machine and Human-Readable Output in New Subtools
When creating a new subtool, always prefer supporting **both** structured machine output (`ffx --machine json` / `ffx --machine json-pretty`) and human-readable terminal output:
* **Machine output** (`VerifiedMachineWriter<T>` with `schemars::JsonSchema`) lets scripts, test harnesses, IDE integrations, and AI agents consume results reliably with a compile-time schema contract.
* **Human-readable output** ensures developers running `ffx <subtool>` interactively get clear, readable text rather than empty output or raw JSON dumps.

Use `VerifiedMachineWriter<T>` by default and emit both formats from your `FfxMain::main` implementation:

```rust
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExampleOutput {
    Success { device_name: String },
    Error { message: String },
}

impl std::fmt::Display for ExampleOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success { device_name } => write!(f, "Device: {device_name}"),
            Self::Error { message } => write!(f, "Error: {message}"),
        }
    }
}

impl FfxMain for ExampleTool {
    type Writer = VerifiedMachineWriter<ExampleOutput>;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let name = self.proxy.get_device_name().await.user_message("Failed to query device")?;
        let output = ExampleOutput::Success { device_name: name };
        // Emits JSON when `--machine` is passed, or `Display` text in human mode:
        writer.item(&output)?;
        Ok(())
    }
}
```

#### Choosing the Right `VerifiedMachineWriter` Method
* **`writer.item(&output)`**: Best when your output type implements `std::fmt::Display`. Automatically emits structured JSON in `--machine` mode and the `Display` representation in human mode.
* **`writer.machine_or(&output, human_text)` / `writer.machine_or_else(&output, || ...)`**: Emits structured JSON in `--machine` mode and the provided string/closure output in human mode without requiring `Display` on `T`.
* **Branching on `writer.is_machine()`**: When human output requires multi-line formatting, tables, or streaming progress, handle both branches explicitly:
  * Call `writer.machine(&output)?` when `writer.is_machine()` is `true`.
  * Call `writer.line(...)` or `writeln!(writer, ...)` when `writer.is_machine()` is `false`.

#### Exceptions to Dual-Output
While supporting both machine and human-readable output is the default expectation for new subtools, the following categories are valid exceptions:
* **Interactive-Only / Human-Only Tools**: Subtools that launch interactive shells, TUIs, or debuggers (e.g., `ffx component explore`, `ffx debug connect`) should use `SimpleWriter` (`SimpleWriter` automatically rejects `--machine json`).
* **Stream / Filter Tools**: Subtools that continuously filter or transform byte/text streams over stdio (e.g., `ffx debug symbolize`) should use `SimpleWriter` unless each streamed record has a well-defined JSON schema.
* **Commands with No Required Human Output**: Action-only commands that succeed silently in human mode (e.g., `ffx target reboot`) should still support `--machine` for automation by calling `writer.machine(&output)?` (such as `MachineWriter<()>` or a status enum with `VerifiedMachineWriter`) without emitting human stdout on success.
* **Artifact / Extraction Tools**: Subtools whose primary responsibility is writing extracted files to disk rather than reporting state to stdout (e.g., `ffx scrutiny extract blobfs`) may use `SimpleWriter` or emit a minimal status summary.

#### Output Pitfalls to Avoid
* **Never call *only* `writer.machine(&output)` for data-reporting tools**: `writer.machine()` is a no-op when `--machine` is not set. Unless the command is intentionally silent on success (like `ffx target reboot`), calling only `writer.machine()` leaves interactive CLI users with unexpected blank output.
* **Never call *only* `writer.line(...)` or `writeln!(writer, ...)` on `VerifiedMachineWriter`**: Standard `Write` and `line()` calls on `VerifiedMachineWriter` are ignored in `--machine` mode, producing empty stdout for machine consumers.
* **Do not abuse `MachineWriter<String>` or `MachineWriter<serde_json::Value>`**: If a command genuinely has no structured output use case, use `SimpleWriter`. Using `MachineWriter<String>` creates an untyped contract.
* **Golden Checks (`cli-goldens` & `mw-goldens`)**:
  * CLI flags (`ArgsInfo`) are verified against `//src/developer/ffx/tests/cli-goldens`.
  * Machine output schemas (`JsonSchema`) are verified against `//src/developer/ffx/tests/mw-goldens`.
  * If you modify CLI arguments or machine output types, run the golden tests and update the golden files as instructed by the build failure.

---

## 5. Error Handling (`//src/developer/ffx/lib/command/error`)

### Moratorium on `anyhow`
* **Do NOT use `anyhow` (`anyhow::Error`, `anyhow::Result`, `anyhow!`, `bail!`, `Context`) in new or refactored `ffx` code**, whether in subtools or library crates.
* **Why**: `anyhow` erases error types, prevents callers from programmatically matching on failure modes, and obscures the critical distinction between actionable user errors and internal tool bugs. When touching existing code that uses `anyhow`, migrate it to typed errors (`thiserror`) or `fho::Result` (`ffx_command_error::Result`) as appropriate.
* **What to use instead**:
  * **In library crates (`//src/developer/ffx/lib/*`)**: Define strongly-typed domain error enums using `#[derive(thiserror::Error, Debug)]`.
  * **In subtools (`//src/developer/ffx/tools/*`)**: Use `fho::Result<T>` and `fho::Error` (`ffx_command_error::Error`), converting library errors at the subtool boundary via `From` or `fho::FfxContext`.

### User Errors vs. Internal Bugs
`ffx` distinguishes between **Actionable User Errors** and **Unexpected Internal Bugs** via `fho::Error` (`ffx_command_error::Error`):
1. **User Errors (`Error::User`)**: Printed cleanly to `stderr` without stack traces. Use for bad CLI arguments, missing files, target unreachability, or anything the user can act on.
2. **Internal Bugs (`Error::Unexpected`)**: Prints a `BUG: An internal command error occurred.` banner with full error chain diagnostics and instructs the user to file a bug at `go/ffx-bug`.

### Best Practices for Error Propagation
* **Use `fho::FfxContext` instead of legacy `ffx_error!` / `ffx_bail!` or `anyhow` macros**:
  * Attach user-facing context with `.user_message("...")` or `.with_user_message(|| format!(...))`:
    ```rust
    let contents = std::fs::read_to_string(&path)
        .with_user_message(|| format!("Unable to read manifest at '{}'", path.display()))?;
    ```
  * Mark internal invariant failures with `.bug()` or `.bug_context("...")`:
    ```rust
    let parsed = parse_internal_state().bug_context("Internal state corrupted")?;
    ```
  * For early returns or standalone errors, use `fho::return_user_error!(...)`, `fho::user_error!(...)`, `fho::return_bug!(...)`, or `fho::bug!(...)`.
* **Actionable Error Messages**: State *what* failed first, followed by *how* the user can resolve it (e.g., `"Target connection failed. Run 'ffx target list' or 'ffx doctor' to verify device state."`).

---

## 6. Configuration (`//src/developer/ffx/config`)

`ffx` resolves configuration across a 5-level priority hierarchy (`ConfigLevel` in `//src/developer/ffx/config`):
1. **`Runtime`** (`--config` / `-c key=val` CLI flags — highest priority)
2. **`User`** (`~/.fuchsia/config.json`)
3. **`Build`** (active build directory configuration, read-only)
4. **`Global`** (system-wide policy configuration)
5. **`Default`** (compiled-in defaults via `include_default!()` — lowest priority)

* **Always Thread `EnvironmentContext` Explicitly**:
  * Never rely on ambient global configuration state when an `EnvironmentContext` can be passed or injected.
  * `EnvironmentContext` implements `TryFromEnv` and can be injected directly as a field on your `#[derive(FfxTool)]` struct:
    ```rust
    #[derive(FfxTool)]
    pub struct ExampleTool {
        #[command]
        cmd: ExampleCommand,
        context: EnvironmentContext,
    }
    ```
* **Querying Configuration via `EnvironmentContext`**:
  * Use `self.context.get::<T, _>(KEY_CONST)` or `self.context.get_optional::<T, _>(KEY_CONST)` for direct typed lookups, or `self.context.query(KEY_CONST)` (`ConfigQueryBuilder`) when specifying a `ConfigLevel` or `SelectMode`.
  * For structured config-backed types, implement `ffx_config::TryFromEnvContext`.
* **Define Configuration Keys in `//src/developer/ffx/config/src/keys.rs` & Use Constants for Defaults**:
  * **No Inline Config Key Strings**: Never pass raw string literals (e.g., `context.get("discovery.fastboot.timeout")`) directly at call sites. Place shared or global configuration keys in `//src/developer/ffx/config/src/keys.rs` (`ffx_config::keys::*`), or define a named `const` at the module/crate level if a key is strictly internal to a single subtool.
  * **No Magic Default Values**: Never pass hardcoded numeric or string literals directly to `.unwrap_or(...)` when falling back to a default configuration value (e.g., avoid `.unwrap_or(500)`). Define a descriptive named `const` (e.g., `const DEFAULT_FASTBOOT_DISCOVERY_TIMEOUT_MS: u64 = 500;`) and/or register the default in `//src/developer/ffx/data/config.json`.
* **Support `ffx --strict`**: Avoid assuming ambient host state or implicit user/build config files exist. Any required settings in strict mode must be resolvable via explicit CLI flags or `-c` runtime config overrides (`EnvironmentContext::is_strict()`).

---

## 7. Testing Guidelines

### Use `#[fuchsia::test]` for All Unit Tests
* **Always annotate unit tests with `#[fuchsia::test]`**: Use `#[fuchsia::test]` for both synchronous and `async` unit tests across `ffx` libraries and subtools.
* **Do not use legacy or runtime-specific test macros**: Avoid `#[fuchsia_async::run_singlethreaded(test)]` (or `#[fasync::run_singlethreaded(test)]`), `#[tokio::test]`, and bare `#[test]`. `#[fuchsia::test]` automatically configures the single-threaded `fuchsia_async` executor for `async fn` tests and initializes test logging consistently.

### Keep `#[cfg(test)]` Confined to the `test` / `tests` Module
* **Avoid scattering `#[cfg(test)]` in production code**: Do not place `#[cfg(test)]` attributes on individual functions, methods, struct fields, imports, or `impl` blocks inside non-test modules.
* **Place all test helpers, utilities, and test-only methods inside the `test` module**:
  * In Rust, a child `#[cfg(test)] mod test` (or `mod tests`) module has full visibility into the private fields and items of its parent module. Define test-only constructors, mock setup helpers, and `impl` blocks for parent types directly inside the `test` module rather than annotating items in the main module.
  * When test utilities are shared across multiple modules within a crate, consolidate them into a single `#[cfg(test)] mod test_utils` (or a dedicated `testonly = true` crate if shared across crates) instead of sprinkling `#[cfg(test)]` throughout production code.
  * Keeping non-test modules free of `#[cfg(test)]` prevents struct layouts or control flow from diverging between test and production builds and avoids conditional unused-import or dead-code warnings.

### Unit Testing Subtools
* **Test Both Machine and Human-Readable Output with Local FDomain Proxies**:
  Use `fdomain_local::local_client_empty()`, `target_holders::fake_proxy` (or `fake_async_proxy`), and `TestBuffers` to verify both `Some(Format::Json)` (including schema validation) and `None` (human-readable output) without an emulator or network connection:
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use ffx_writer::{Format, TestBuffers};

      fn setup_fake_proxy() -> NameProviderProxy {
          let client = fdomain_local::local_client_empty();
          target_holders::fake_proxy::<NameProviderProxy>(client, move |req| match req {
              fdomain_fuchsia_device::NameProviderRequest::GetDeviceName { responder } => {
                  responder.send(Ok("fuchsia-test-node")).unwrap();
              }
          })
      }

      #[fuchsia::test]
      async fn test_example_json_output() {
          let tool = ExampleTool { cmd: ExampleCommand {}, proxy: setup_fake_proxy() };
          let buffers = TestBuffers::default();
          let writer = VerifiedMachineWriter::<ExampleOutput>::new_test(Some(Format::Json), &buffers);
          tool.main(writer).await.expect("tool should succeed");
          let output = buffers.into_stdout_str();
          VerifiedMachineWriter::<ExampleOutput>::verify_schema(&serde_json::from_str(&output).unwrap())
              .expect("output must match schema");
      }

      #[fuchsia::test]
      async fn test_example_human_output() {
          let tool = ExampleTool { cmd: ExampleCommand {}, proxy: setup_fake_proxy() };
          let buffers = TestBuffers::default();
          let writer = VerifiedMachineWriter::<ExampleOutput>::new_test(None, &buffers);
          tool.main(writer).await.expect("tool should succeed");
          assert_eq!(buffers.into_stdout_str(), "Device: fuchsia-test-node\n");
      }
  }
  ```
* **Isolated Config in Tests**: When testing code that reads or writes `ffx_config`, initialize an isolated test environment with `let test_env = ffx_config::test_init().expect("test env");` and pass `&test_env.context`.
* **Registering Host Tests in `BUILD.gn`**:
  Always include a `tests` group in the subtool/library `BUILD.gn` and ensure it is wired into the parent `tests` group (e.g., `//src/developer/ffx/tools/BUILD.gn`):
  ```gn
  group("tests") {
    testonly = true
    deps = [ ":lib_test($host_toolchain)" ]
  }
  ```
* **End-to-End Tests (`ffx_e2e_emu`)**: When an integration test against a real Fuchsia system is necessary, use `//src/developer/ffx/lib/e2e_emu` (`IsolatedEmulator`) and `//src/developer/ffx/lib/isolate` so the test runs in a sandboxed isolate directory without polluting the developer's host environment.
* **Python Test Scripts**: When a test requires a helper or mock executable written in Python (e.g., a mock driver or fake `ssh` binary), do **not** construct the Python script as an inline string literal (`r#"..."#` or `format!(...)`) inside the Rust test. Instead:
  1. Place the standalone script in `test_data/<script>.py` (e.g., `test_data/mock_driver.py`), and start the file with:
     ```python
     #!/usr/bin/env python3
     # allow-non-vendored-python
     ```
     In Infra test environments, an in-tree vendored Python interpreter is not necessarily available; using `/usr/bin/env python3` alongside `# allow-non-vendored-python` ensures the script runs reliably both in Infra and on a developer's machine while passing presubmit shebang checks. Pass any dynamic per-test parameters via CLI flags or environment variables rather than interpolating values into Python source code.
  2. Declare the script in the target's `inputs` list in `BUILD.gn` so GN tracks it for incremental rebuilds:
     ```gn
     inputs = [ "test_data/mock_driver.py" ]
     ```
  3. Embed the script in the Rust test using `include_str!("../test_data/mock_driver.py")`, write it to the test's `TempDir`, and set its permissions to `0o755` before invoking it.
