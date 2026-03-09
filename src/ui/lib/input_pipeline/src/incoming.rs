// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fidl_fuchsia_io as fio;
use fuchsia_component::client::Connect;
use fuchsia_component::directory::{AsRefDirectory, Directory};

#[cfg(not(feature = "dso"))]
use fuchsia_component::client::connect;

#[cfg(feature = "dso")]
#[derive(Clone)]
pub struct Incoming(std::sync::Arc<fdf_component::Incoming>);

impl Incoming {
    pub fn connect_protocol_at<T: Connect>(
        dir: &impl AsRefDirectory,
        path: &str,
    ) -> Result<T, anyhow::Error> {
        T::connect_at_dir_root_with_name(dir, path).context("connect_protocol_at")
    }
}

#[cfg(feature = "dso")]
impl Incoming {
    pub fn new(incoming: std::sync::Arc<fdf_component::Incoming>) -> Self {
        Self(incoming)
    }

    pub fn connect_protocol<T: Connect>(&self) -> Result<T, anyhow::Error> {
        self.0.connect_protocol().context("connect_protocol")
    }
}

#[cfg(feature = "dso")]
impl Directory for Incoming {
    fn open(
        &self,
        path: &str,
        flags: fio::Flags,
        server_end: zx::Channel,
    ) -> Result<(), anyhow::Error> {
        self.0.open(path, flags, server_end)
    }
}

#[cfg(not(feature = "dso"))]
#[derive(Clone)]
pub struct Incoming;

#[cfg(not(feature = "dso"))]
impl Incoming {
    pub fn new() -> Self {
        Self {}
    }

    pub fn connect_protocol<T: Connect>(&self) -> Result<T, anyhow::Error> {
        connect::connect_to_protocol::<T>()
    }
}

#[cfg(not(feature = "dso"))]
impl Directory for Incoming {
    fn open(
        &self,
        path: &str,
        flags: fio::Flags,
        server_end: zx::Channel,
    ) -> Result<(), anyhow::Error> {
        fdio::open(path, flags, server_end).context("Directory::open")
    }
}

impl AsRefDirectory for Incoming {
    fn as_ref_directory(&self) -> &dyn fuchsia_component::directory::Directory {
        self
    }
}
