// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ACPI initialization and global parser for PC platform.

use core::mem::MaybeUninit;
use handoff::PhysHandoff;

#[cfg(console_enabled)]
use crate::console_rust::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};

// System-wide ACPI parser.
static mut GLOBAL_ACPI_PARSER: MaybeUninit<acpi_lite::AcpiParser<'static>> = MaybeUninit::uninit();
static mut ACPI_INITIALIZED: bool = false;

/// Initializes the system-wide ACPI parser.
#[allow(static_mut_refs)]
fn platform_init_acpi(_level: init::LkInitLevel) {
    let rsdp_pa: u64 = Option::from(PhysHandoff::get().acpi_rsdp).unwrap_or(0);
    // SAFETY: Early boot, single-threaded execution at LK_INIT_LEVEL_VM.
    let parser = match unsafe {
        crate::platform_pc::acpi_lite_zircon::acpi_parser_init(crate::kernel::types::PAddr(
            rsdp_pa as usize,
        ))
    } {
        Ok(p) => p,
        Err(status) => {
            panic!("Could not initialize ACPI. Error code: {:?}.", status);
        }
    };

    // SAFETY: Early boot, single-threaded execution.
    unsafe {
        GLOBAL_ACPI_PARSER.write(parser);
        ACPI_INITIALIZED = true;
    }
}

init::lk_init_hook!(acpi, platform_init_acpi, init::LK_INIT_LEVEL_VM);

/// Returns a reference to the global `AcpiParser` instance.
///
/// # Panics
///
/// Panics if ACPI initialization has not occurred yet.
pub fn global_acpi_lite_parser() -> &'static acpi_lite::AcpiParser<'static> {
    // SAFETY: Single-threaded read of initialization status.
    assert!(unsafe { ACPI_INITIALIZED }, "PlatformInitAcpi() not called.");
    // SAFETY: Once initialized at LK_INIT_LEVEL_VM, GLOBAL_ACPI_PARSER is read-only and
    // valid for the lifetime of the kernel.
    #[allow(static_mut_refs)]
    unsafe {
        GLOBAL_ACPI_PARSER.assume_init_ref()
    }
}

#[cfg(console_enabled)]
unsafe extern "C" fn cmd_acpidump(_argc: i32, _argv: *const CmdArgs, _flags: u32) -> i32 {
    // SAFETY: Single-threaded read of initialization flag.
    if !unsafe { ACPI_INITIALIZED } {
        kprint::kprintln!("ACPI not initialized.");
        return 1;
    }
    // SAFETY: GLOBAL_ACPI_PARSER is initialized.
    #[allow(static_mut_refs)]
    let parser = unsafe { GLOBAL_ACPI_PARSER.assume_init_ref() };
    parser.dump_tables();
    0
}

#[cfg(console_enabled)]
static_command!(
    CMD_ACPIDUMP,
    c"acpidump".as_ptr(),
    c"dump ACPI tables to console".as_ptr(),
    cmd_acpidump,
    CMD_AVAIL_ALWAYS
);
