---
name: driver_file_structure
description: >-
  Describe the standard directory and file structure for a Fuchsia driver.
---

# Driver File Structure

## Directory Layout

A typical driver directory in Fuchsia (e.g., `src/devices/.../my-driver/`)
follows this layout:

*   **`meta/`**: Contains component manifests and metadata.
*   **Source Files**: The implementation files (C++ or Rust).
*   **Build Files**: Define how the driver is compiled and packaged.
*   **Tests**: Unit and integration tests for the driver.

### 1. The `meta/` Directory

The `meta/` directory contains files that define the driver's identity,
capabilities, and binding rules.

*   **`[driver_name].bind`**: The source file containing the bind rules.
*   **`[driver_name].cml`**: The component manifest defining capabilities and
    location in the topology.
*   **`[driver_name]-info.json`**: Metadata about the driver (name, description,
    etc.).
*   **`bind-tests.json`**: Defines tests for the bind rules (optional).

### 2. Source Files

Implementation files are located at the root of the driver directory or in
subdirectories if complex.

#### C++ Drivers

C++ drivers typically consist of the following file structure:
*   **`[driver_name].cc`**: Contains the driver implementation, including the
    lifecycle hooks and the `FUCHSIA_DRIVER_EXPORT2` macro.
*   **`[driver_name].h`**: Defines the driver class, inheriting from
    `fdf::DriverBase2`.

#### Rust Drivers

Rust drivers have a strict file structure enforced by the Rust compilation
model:
*   **`src/lib.rs`**: The root library file. Fuchsia Rust drivers are compiled
    as dynamic libraries loaded by the driver host, so they **must** use
    `lib.rs`.
*   **Other source files**: Additional modules can be included in the `src/`
    directory (e.g., `src/device.rs`, `src/protocol.rs`).
*   **`src/main.rs`**: Used only for tools or executables that live within the
    driver directory, not for the driver itself.

### 3. Build Files

Build files define the targets for compiling the driver and creating the
component package.

*   **`BUILD.gn`**: Used for the GN build system (in-tree).
*   **`BUILD.bazel`**: Used for the Bazel build system (SDK and portable
    builds).

### 4. Tests

Tests are usually located in a `tests/` subdirectory or directly in the driver
directory.

*   Unit tests often share the directory or are in a dedicated test target in
    the build file.

## Common Pitfalls

*   **Placing `.bind` outside `meta/`**: While not strictly forbidden by the
    compiler if paths are specified, placing it in `meta/` is the standard
    convention and expected by many templates.
*   **Missing `.cml` in `meta/`**: The component manifest must be in `meta/` to
    be correctly processed by component templates.

## Further Reading

*   [Debugging Driver Binding](/src/devices/skills/debug_driver_binding/SKILL.md) -
    Workflow for determining why a driver failed to bind.
