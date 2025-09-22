// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod input_controller;
pub mod input_device_configuration;
pub mod types;

pub use self::input_device_configuration::build_input_default_settings;

mod input_fidl_handler;
