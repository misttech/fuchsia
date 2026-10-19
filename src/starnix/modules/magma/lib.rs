// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Increase recursion limit because LTO causes overflow.
#![recursion_limit = "256"]

mod ffi;
mod file;
mod gpu_class_directory;
mod image_file;
mod init;
#[allow(clippy::module_inception)]
mod magma;

use ffi::get_magma_params;
pub use file::MagmaFile;
pub use gpu_class_directory::GpuDeviceConfig;
pub use init::{cmdline_params, module_init, validate_args};
