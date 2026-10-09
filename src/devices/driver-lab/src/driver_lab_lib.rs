// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Embedded Fuchsia driver library (`driver_lab_rust`) and shared runtime
//! backends for `driver-lab`.
//!
//! This library provides:
//! - [`platform_provider::MappedMmio`]: pre-mapped MMIO adapter via VMO
//!   duplication (`zx::Rights::SAME_RIGHTS`) so an active driver can share
//!   its MMIO banks without perturbing its own mappings.
//! - [`fuchsia_backends`]: Fuchsia FIDL protocol backends (GPIO, I2C, SPI,
//!   Clock, Reset, Serial).
//! - [`server`]: `fuchsia.driver.lab` FIDL service and session server.
//! - [`trace`]: mirrors audit records into the `driver_lab` and
//!   `driver_lab:irq` trace categories.

pub mod embedded;
pub mod fuchsia_backends;
pub mod platform_provider;
pub mod server;
pub mod trace;

pub use embedded::{DriverLabBuilder, EmbeddedLabServer};
pub use fuchsia_backends::LiveBackend;
pub use lab_proxy_core as core;
pub use platform_provider::MappedMmio;
