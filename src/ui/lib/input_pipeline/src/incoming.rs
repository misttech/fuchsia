// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Transport;
use anyhow::Context;
use fidl_fuchsia_io as fio;
use fuchsia_component::client::Connect;
use fuchsia_component::directory::{AsRefDirectory, Directory};

#[cfg(feature = "dso")]
pub use dso::*;

#[cfg(not(feature = "dso"))]
pub use elf::*;

impl Incoming {
    pub fn connect_protocol_at<T: Connect>(
        dir: &impl AsRefDirectory,
        path: &str,
    ) -> Result<T, anyhow::Error> {
        T::connect_at_dir_root_with_name(dir, path).context("connect_protocol_at")
    }
}

mod dso {
    #![cfg(feature = "dso")]

    use super::*;

    #[derive(Clone)]
    pub struct Incoming(std::sync::Arc<fdf_component::Incoming>);

    impl Incoming {
        pub fn new(incoming: std::sync::Arc<fdf_component::Incoming>) -> Self {
            Self(incoming)
        }

        pub fn connect_protocol<T: Connect>(&self) -> Result<T, anyhow::Error> {
            self.0.connect_protocol().context("connect_protocol")
        }

        pub fn connect_protocol_next<P: fidl_next::Discoverable>(
            &self,
        ) -> Result<fidl_next::ClientEnd<P, Transport>, anyhow::Error> {
            self.0.connect_protocol_next().context("connect_protocol_next")
        }

        pub fn connect_protocol_next_at<P: fidl_next::Discoverable>(
            dir: &impl AsRefDirectory,
            path: &str,
        ) -> Result<fidl_next::ClientEnd<P, Transport>, anyhow::Error> {
            fdf_component::Incoming::connect_protocol_next_at(dir, path)
                .context("connect_protocol_next_at")
        }
    }

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
}

mod elf {
    #![cfg(not(feature = "dso"))]

    use super::*;
    use fuchsia_component::client::connect;

    #[derive(Clone)]
    pub struct Incoming;

    impl Incoming {
        pub fn new() -> Self {
            Self {}
        }

        pub fn connect_protocol<T: Connect>(&self) -> Result<T, anyhow::Error> {
            connect::connect_to_protocol::<T>()
        }

        pub fn connect_protocol_next<P: fidl_next::Discoverable>(
            &self,
        ) -> Result<fidl_next::ClientEnd<P, Transport>, anyhow::Error> {
            let (client_end, server_end) = zx::Channel::create();
            fdio::service_connect(&format!("/svc/{}", P::PROTOCOL_NAME), server_end)
                .context("connect_protocol_next")?;
            Ok(fidl_next::ClientEnd::<P, zx::Channel>::from_untyped(client_end))
        }

        pub fn connect_protocol_next_at<P: fidl_next::Discoverable>(
            dir: &impl AsRefDirectory,
            path: &str,
        ) -> Result<fidl_next::ClientEnd<P, Transport>, anyhow::Error> {
            let (client_end, server_end) = zx::Channel::create();
            dir.as_ref_directory().open(path, fio::Flags::PROTOCOL_SERVICE, server_end)?;
            Ok(fidl_next::ClientEnd::<P, zx::Channel>::from_untyped(client_end))
        }
    }

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
}

impl AsRefDirectory for Incoming {
    fn as_ref_directory(&self) -> &dyn fuchsia_component::directory::Directory {
        self
    }
}
