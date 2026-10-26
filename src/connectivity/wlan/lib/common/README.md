# WLAN Common

This library provides shared WLAN definitions such as frame parsers, element encoders, and test utilities for the WLAN stack.

## Major Responsibilities

### Field and Element Parsing
The [`ie parser`](./rust/src/ie/mod.rs) contains definitions and parsing logic for binary Information Elements (IEs) and vendor attributes in management frames.

### MAC Frame Parsing
The [`mac frame parser`](./rust/src/mac/mod.rs) inspects binary frames to determine IEEE 802.11 frame control types, subtypes, and header fields.

### Frame Writing
Frame writing utilities serialize IEEE 802.11 management and data frames into binary formats for transmission.

### Test Utilities
Test utilities generate [`fake frames`](./rust/src/test_utils/fake_frames.rs) and [`fake stations`](./rust/src/test_utils/fake_stas.rs) for unit tests across the stack and provide the `assert_variant` macro.

## Rust and C++ Implementations

The core WLAN stack completed its transition to Rust. The canonical library for the stack is `lib/common/rust` (`wlan-common`). The `lib/common/cpp` directory contains shared C++ helpers, element parsers, and utilities retained for C++ drivers (`wlansoftmac` and vendor drivers).

## Build System Note

`BUILD.bazel` defines the `lib/common` build targets. When you modify dependencies or sources, edit `BUILD.bazel` first. Then run `fx bazel2gn` to regenerate `BUILD.gn`. Do not edit `BUILD.gn` directly.
