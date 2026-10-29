// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Type-safe bindings for Zircon kernel
//! [syscalls](https://fuchsia.dev/fuchsia-src/reference/syscalls).

#![cfg_attr(not(target_os = "fuchsia"), allow(unused_crate_dependencies, unused_macros))]

// Put this first so subsequently declared modules have access.
#[macro_use]
mod macros;

#[cfg(target_os = "fuchsia")]
mod bti;
#[cfg(target_os = "fuchsia")]
mod channel;
#[cfg(target_os = "fuchsia")]
mod clock;
#[cfg(target_os = "fuchsia")]
mod clock_update;
#[cfg(target_os = "fuchsia")]
mod counter;
#[cfg(target_os = "fuchsia")]
mod cprng;
#[cfg(target_os = "fuchsia")]
mod debuglog;
#[cfg(target_os = "fuchsia")]
mod event;
#[cfg(target_os = "fuchsia")]
mod eventpair;
#[cfg(target_os = "fuchsia")]
mod exception;
#[cfg(target_os = "fuchsia")]
mod fifo;
#[cfg(target_os = "fuchsia")]
mod futex;
#[cfg(target_os = "fuchsia")]
mod guest;
#[cfg(target_os = "fuchsia")]
mod handle;
#[cfg(target_os = "fuchsia")]
mod info;
#[cfg(target_os = "fuchsia")]
mod interrupt;
#[cfg(target_os = "fuchsia")]
mod iob;
#[cfg(target_os = "fuchsia")]
mod iommu;
#[cfg(target_os = "fuchsia")]
mod job;
mod koid;
#[cfg(target_os = "fuchsia")]
mod msi;
#[cfg(target_os = "fuchsia")]
mod name;
mod object_type;
#[cfg(target_os = "fuchsia")]
mod pager;
#[cfg(target_os = "fuchsia")]
mod pci_device;
#[cfg(target_os = "fuchsia")]
mod pmt;
#[cfg(target_os = "fuchsia")]
mod port;
#[cfg(target_os = "fuchsia")]
mod process;
#[cfg(target_os = "fuchsia")]
mod profile;
#[cfg(target_os = "fuchsia")]
mod property;
#[cfg(target_os = "fuchsia")]
mod resource;
mod rights;
mod signals;
#[cfg(target_os = "fuchsia")]
mod socket;
#[cfg(target_os = "fuchsia")]
mod stream;
#[cfg(target_os = "fuchsia")]
mod suspend_token;
#[cfg(target_os = "fuchsia")]
mod system;
#[cfg(target_os = "fuchsia")]
mod task;
#[cfg(target_os = "fuchsia")]
mod thread;
#[cfg(target_os = "fuchsia")]
mod time;
#[cfg(target_os = "fuchsia")]
mod vcpu;
#[cfg(target_os = "fuchsia")]
mod version;
#[cfg(target_os = "fuchsia")]
mod vmar;
#[cfg(target_os = "fuchsia")]
mod vmo;
#[cfg(target_os = "fuchsia")]
mod wait;

#[cfg(target_os = "fuchsia")]
pub mod vdso_next {
    pub use super::iob::vdso_next::*;
}

#[cfg(target_os = "fuchsia")]
pub use self::bti::*;
#[cfg(target_os = "fuchsia")]
pub use self::channel::*;
#[cfg(target_os = "fuchsia")]
pub use self::clock::*;
#[cfg(target_os = "fuchsia")]
pub use self::clock_update::{ClockUpdate, ClockUpdateBuilder};
#[cfg(target_os = "fuchsia")]
pub use self::counter::*;
#[cfg(target_os = "fuchsia")]
pub use self::cprng::*;
#[cfg(target_os = "fuchsia")]
pub use self::debuglog::*;
#[cfg(target_os = "fuchsia")]
pub use self::event::*;
#[cfg(target_os = "fuchsia")]
pub use self::eventpair::*;
#[cfg(target_os = "fuchsia")]
pub use self::exception::*;
#[cfg(target_os = "fuchsia")]
pub use self::fifo::*;
#[cfg(target_os = "fuchsia")]
pub use self::futex::*;
#[cfg(target_os = "fuchsia")]
pub use self::guest::*;
#[cfg(target_os = "fuchsia")]
pub use self::handle::*;
#[cfg(target_os = "fuchsia")]
pub use self::info::*;
#[cfg(target_os = "fuchsia")]
pub use self::interrupt::*;
#[cfg(target_os = "fuchsia")]
pub use self::iob::*;
#[cfg(target_os = "fuchsia")]
pub use self::iommu::*;
#[cfg(target_os = "fuchsia")]
pub use self::job::*;
pub use self::koid::*;
#[cfg(target_os = "fuchsia")]
pub use self::msi::*;
#[cfg(target_os = "fuchsia")]
pub use self::name::*;
pub use self::object_type::*;
#[cfg(target_os = "fuchsia")]
pub use self::pager::*;
#[cfg(target_os = "fuchsia")]
pub use self::pci_device::*;
#[cfg(target_os = "fuchsia")]
pub use self::pmt::*;
#[cfg(target_os = "fuchsia")]
pub use self::port::*;
#[cfg(target_os = "fuchsia")]
pub use self::process::*;
#[cfg(target_os = "fuchsia")]
pub use self::profile::*;
#[cfg(target_os = "fuchsia")]
pub use self::property::*;
#[cfg(target_os = "fuchsia")]
pub use self::resource::*;
pub use self::rights::*;
pub use self::signals::*;
#[cfg(target_os = "fuchsia")]
pub use self::socket::*;
#[cfg(target_os = "fuchsia")]
pub use self::stream::*;
#[cfg(target_os = "fuchsia")]
pub use self::suspend_token::*;
#[cfg(target_os = "fuchsia")]
pub use self::system::*;
#[cfg(target_os = "fuchsia")]
pub use self::task::*;
#[cfg(target_os = "fuchsia")]
pub use self::thread::*;
#[cfg(target_os = "fuchsia")]
pub use self::time::*;
#[cfg(target_os = "fuchsia")]
pub use self::vcpu::*;
#[cfg(target_os = "fuchsia")]
pub use self::version::*;
#[cfg(target_os = "fuchsia")]
pub use self::vmar::*;
#[cfg(target_os = "fuchsia")]
pub use self::vmo::*;
#[cfg(target_os = "fuchsia")]
pub use self::wait::*;
pub use zx_status::*;
pub use zx_status_ext::*;

/// Prelude containing common utility traits.
/// Designed for use like `use zx::prelude::*;`
pub mod prelude {
    #[cfg(target_os = "fuchsia")]
    pub use crate::{AsHandleRef, Peered};
}

pub mod sys {
    pub use zx_sys::*;
}
