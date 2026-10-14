# Testing utility library (`ffx_e2e_emu`)

The [`ffx_e2e_emu`](/src/developer/ffx/lib/e2e_emu) library
(`//src/developer/ffx/lib/e2e_emu`) implements helpers for writing end-to-end
(e2e) tests for `ffx` subtools and libraries.

It includes:

* An isolated emulator (`IsolatedEmulator`) started with a Fuchsia system image
  via `ffx emu`
* A hermetic execution environment for `ffx`
  ([via `ffx_isolate`](/docs/development/tools/ffx/development/integration_testing/README.md))
* Automatic system log streaming and package repository server setup
* Helper methods for executing `ffx` and `ssh` commands against the isolated
  emulator
* GN templates (`ffx_e2e_test`, `ffx_e2e_product_bundle`, and
  `ffx_e2e_package_repository`) in
  `//src/developer/ffx/lib/e2e_emu/ffx_e2e_test.gni`

## Usage

Subtool and library e2e tests are defined with the `ffx_e2e_test` GN template
and are typically added to a `host_tests` group in the subtool or library's
`BUILD.gn`.

In `BUILD.gn`:

```gn
import("//build/host.gni")
import("//src/developer/ffx/lib/e2e_emu/ffx_e2e_test.gni")

if (is_host) {
  ffx_e2e_test("ffx_<something>_e2e_tests") {
    edition = "2024"
    source_root = "tests/e2e.rs"
    sources = [ source_root ]

    deps = [
      "//src/lib/fuchsia",
      "//third_party/rust_crates:anyhow",
    ]
  }

  group("host_tests") {
    testonly = true
    deps = [ ":ffx_<something>_e2e_tests($host_toolchain)" ]
  }
}
```

In `tests/e2e.rs`:

```rust
use anyhow::Result;
use ffx_e2e_emu::IsolatedEmulator;

#[fuchsia::test(logging = true)]
async fn my_test() -> Result<()> {
  let emu = IsolatedEmulator::start("my-test").await?;
  let output = emu.ffx_output(&["version"]).await?;
  assert!(!output.is_empty());
  Ok(())
}
```

> Note: Starting an `IsolatedEmulator` spins up a full Fuchsia instance and can
> take 20+ seconds. When testing multiple behaviors against the same
> configuration, prefer sharing a single `IsolatedEmulator` instance within a
> test function where practical.

### Customizing the system image and package repository

By default, `ffx_e2e_test` uses a minimal default product bundle and an empty
package repository.

* **Custom Product Bundle**: If your test requires specific platform
  configuration or base packages, define a custom bundle with
  `ffx_e2e_product_bundle` and pass `product_bundle = ":<label>"` to
  `ffx_e2e_test`:

  ```gn
  ffx_e2e_product_bundle("product_bundle") {
    name = "my_custom_test_product"
    product_configuration = {
      platform = {
        build_type = "eng"
      }
      base_packages = [
        {
          package_target = "//path/to/my:package"
        },
      ]
    }
  }
  ```

* **Custom Package Repository**: If your test needs to resolve packages from a
  repository server at runtime rather than including them in base, define a
  repository with `ffx_e2e_package_repository` and pass
  `package_repository = ":<label>($target_toolchain)"` to `ffx_e2e_test`:

  ```gn
  if (is_fuchsia) {
    ffx_e2e_package_repository("package_repo") {
      packages = [
        {
          label = "//path/to/my:package"
        },
      ]
    }
  }
  ```

## Low-Level Emulators

For tests that specifically need a low-level emulator brought up with an empty
disk (for example, testing bootloader and fastboot flashing via Gigaboot before
a Fuchsia image is installed), `//src/developer/ffx/tests/target:ffx_testing`
provides `ffx_testing::Emu`.

`Emu::start` spawns QEMU directly via `run-zircon` with an empty UEFI disk and
exposes the emulator serial stream. For an example, see
[`ffx_target_test`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/developer/ffx/tests/target/src/tests.rs).

**/!\ This emulator is brought up with an empty disk /!\\**

This is by design. To e2e test with a fuchsia image use the `ffx_e2e_emu`
library under `//src/developer/ffx/lib/e2e_emu`.

```rust
use ffx_testing::Emu;

let emu = Emu::start(&ctx);

// NOTE: If using serial, make sure to drain or close the serial stream when no longer in use.
// Failure to do so will result in QEMU halting.
{
  let serial = emu.serial().await;

  // ...
}

// Dropping or letting the emu go out of scope cleans up the emulator.
```
