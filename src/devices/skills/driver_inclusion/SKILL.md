---
name: driver_inclusion
description: Include drivers in Bazel-based boot images for specific hardware targets.
---

# Driver Inclusion for Specific Hardware Targets

For certain hardware targets, the boot image is constructed using the Bazel
build system, typically defined in directories like
`<repo>/boards/<target>/`.

## Driver Inclusion

To include a driver in the image for these targets, adding it via GN build
arguments (or its test via `fx add-test`) is not sufficient. You must define
the driver and its component using Bazel rules (e.g., `fuchsia_cc_driver`,
`fuchsia_driver_component` in a `BUILD.bazel` file) and ensure that package
target is listed in the board's image definition file (e.g.,
`//vendor/google/boards/<target>/BUILD.bazel`).

## Tests

If you are just adding unit tests that run on the host or in the test realm
without needing to be part of the boot image drivers, the `fx add-test` and GN
approach may still apply.
