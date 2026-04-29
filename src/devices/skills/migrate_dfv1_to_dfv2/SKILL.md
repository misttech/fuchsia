---
name: migrate_dfv1_to_dfv2
description: Guide for migrating Fuchsia drivers from Driver Framework v1 (DFv1) to Driver Framework v2 (DFv2).
---
# Migration Guide: DFv1 to DFv2

You are a coding agent tasked with migrating drivers in the Fuchsia codebase from DFv1 to DFv2.
This work is tracked in Bug 500358249.
Your objective is to update the driver's interfaces, services, and build configuration to comply with DFv2 standards.

## Mandatory Workflow Checklist

Before declaring a migration complete, you MUST complete all steps in this checklist:
- [ ] Code changes implemented (DriverBase, Start, FIDL, etc.)
- [ ] Build dependencies updated in `BUILD.gn`
- [ ] Component manifest updated (`.cml`)
- [ ] Driver added to build graph (e.g., `build_only_labels` or `fx set`)
- [ ] **Run `fx format-code`** to ensure code style compliance.
- [ ] `fx build` completes successfully.
- [ ] Tests pass (if applicable).
- [ ] Commit changes after logical chunks of work.

## 1. Identify DFv1 Drivers

Before starting a migration, confirm that the driver is indeed a DFv1 driver.

For a comprehensive guide on distinguishing DFv1 from DFv2 drivers (including
codebase indicators and runtime checks), see the
[Driver Version Identification Skill](../driver_version_identification/SKILL.md).

## 2. Update Build Dependencies

In the driver's `BUILD.gn` file:

1.  Remove dependencies on `//src/lib/ddk` and `//src/lib/ddktl`.
2.  Add dependency on `//sdk/lib/driver/component/cpp`.

Example:
```gn
# Old
deps = [
  "//src/lib/ddk",
  "//src/lib/ddktl",
]

# New
deps = [
  "//sdk/lib/driver/component/cpp",
]
```

## 3. Update Source Code (C++)

### A. Headers

Replace DDK headers with DFv2 headers:

*   Replace `#include <ddktl/device.h>` with `#include <lib/driver/component/cpp/driver_base.h>`.
*   Replace `#include <lib/driver/component/cpp/driver_export.h>` if not already present for the export macro.

### B. Driver Class

Migrate the driver class to inherit from `fdf::DriverBase`:

*   **DFv1**: Inherits from `ddk::Device<ClassName, ...>`.
*   **DFv2**: Inherits from `fdf::DriverBase`.

The constructor signature changes:

*   **DFv1**: `ClassName(zx_device_t* parent);`
*   **DFv2**: `ClassName(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher);`

Example:
```cpp
// DFv2
class MyDriver : public fdf::DriverBase {
 public:
  MyDriver(fdf::DriverStartArgs start_args,
           fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("my-driver", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override;
};
```

### C. Initialization (Start)

In DFv1, initialization happens in the constructor, `Bind` static method, and `DdkInit`.
In DFv2, all this logic should be moved to the `Start()` method.

*   Return `zx::ok()` on success.
*   Use `incoming()` to access incoming services (replacing `device_get_protocol`).

### D. Macros

Replace `ZIRCON_DRIVER` with `FUCHSIA_DRIVER_EXPORT`.

*   **DFv1**: `ZIRCON_DRIVER(my_driver, driver_ops, "zircon", "0.1");`
*   **DFv2**: `FUCHSIA_DRIVER_EXPORT(MyDriver);` (Usually placed in the `.cc` file).

### E. Logging

Migrate `zxlogf` to `fdf::info`, `fdf::error`, etc.
Refer to the `migrate_logging` skill for detailed instructions on logging migration, including `std::format` syntax.

### F. Banjo to FIDL / Service Discovery

If the driver uses `device_get_protocol` to get a Banjo protocol, use `compat::ConnectBanjo` or migrate to FIDL if available.

Example for Banjo:
```cpp
zx::result<ddk::MiscProtocolClient> client =
     compat::ConnectBanjo<ddk::MiscProtocolClient>(incoming());
```

## 4. Verification

After migration, you must verify that the driver builds and passes tests:

### A. Build Graph Inclusion
Many drivers may not be in the default build graph. To ensure the driver source code is compiled during `fx build`:
1.  Add the driver target (e.g., `//src/devices/rtc/drivers/pl031-rtc:pl031-rtc`) to the build graph.
2.  You can use `fx set ... --with //path/to/driver:target` or manually add it to `build_only_labels` in `out/default/args.gn` to force compilation.

### B. Building
Run `fx build` to ensure the driver compiles successfully.

### C. Testing

Migrating tests from DFv1 to DFv2 can be challenging. If tests exist, update them to use DFv2 testing libraries (e.g., `sdk/lib/driver/testing/cpp`) and run them with `fx test`.

A helpful guide for DFv2 unit testing can be found at docs/development/sdk/driver-testing/driver-unit-testing-quick-start.

## 5. Cleanup and Commits

### A. Cleanup
Before committing, run `fx format-code` to ensure the migrated code adheres to Fuchsia style guidelines.

### B. Commits
Make a local commit after a logical chunk of work. You do not necessarily need a commit for every individual driver if you are migrating multiple similar drivers, but each commit should represent a coherent state.
Follow the Git commit message guidelines in `GEMINI.md` or the project style guide.

## 6. Common Pitfalls and Tips

*   **Non-Default Constructible Members**: Members like `fdf::MmioBuffer` do not have default constructors. If you need to initialize them in `Start()` rather than the constructor, wrap them in `std::optional`.
*   **Missing `fdf_` Symbols at Link Time**: If the linker fails with undefined symbols like `fdf_arena_create`, add `//src/devices/lib/driver:driver_runtime` to `deps` in `BUILD.gn`.
*   **Ambient DDK Dependencies**: If the driver uses a library that has not been migrated to DFv2 and still includes DDK headers (e.g., `<lib/ddk/driver.h>`), you may need to keep `//src/lib/ddk` in `deps` temporarily.
*   **Dispatcher Usage**: When passing a dispatcher to FIDL bindings, `dispatcher()` (which returns `fdf::UnownedSynchronizedDispatcher`) can often be used directly or converted as needed.
*   **Contiguous Buffer Allocation Size**: When using `dma_buffer::CreateContiguous` (or `zx_vmo_create_contiguous`), the requested size MUST be a multiple of the page size. If you pass a size that is not page-aligned, it will fail with `ZX_ERR_INVALID_ARGS` (-10). Ensure you round up the size to `zx_system_get_page_size()` before allocating.

## 7. References

*   `docs/development/drivers/migration/migrate-from-dfv1-to-dfv2/overview.md`
*   `docs/development/drivers/migration/migrate-from-dfv1-to-dfv2/update-ddk-interfaces-to-dfv2.md`
*   `docs/development/drivers/migration/migrate-from-dfv1-to-dfv2/faq.md`
*   `docs/development/drivers/migration/migrate-from-dfv1-to-dfv2/update-other-services-to-dfv2.md`
*   `docs/development/sdk/driver-testing/driver-unit-testing-quick-start`
