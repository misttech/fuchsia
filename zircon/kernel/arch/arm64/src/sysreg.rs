// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use core::ffi::{CStr, c_int};
use kprint::kprint;
use regio::Readable;
use regio::arm64::{SysReg, SystemRegisterSpec, spec};

/// Reads the system register described by `Spec`.
fn read_sysreg<Spec>() -> u64
where
    Spec: SystemRegisterSpec,
    Spec::Access: Readable,
{
    SysReg::<Spec, u64>::new().read()
}

/// A system register the `sysreg` command can read.
struct SysregEntry {
    name: &'static CStr,
    read: fn() -> u64,
}

static SYSREGS_LIST: [SysregEntry; 28] = [
    SysregEntry { name: c"actlr_el1", read: read_sysreg::<spec::ACTLR_EL1> },
    SysregEntry { name: c"ccsidr_el1", read: read_sysreg::<spec::CCSIDR_EL1> },
    SysregEntry { name: c"clidr_el1", read: read_sysreg::<spec::CLIDR_EL1> },
    SysregEntry { name: c"csselr_el1", read: read_sysreg::<spec::CSSELR_EL1> },
    SysregEntry { name: c"ctr_el0", read: read_sysreg::<spec::CTR_EL0> },
    SysregEntry { name: c"midr_el1", read: read_sysreg::<spec::MIDR_EL1> },
    SysregEntry { name: c"mpidr_el1", read: read_sysreg::<spec::MPIDR_EL1> },
    SysregEntry { name: c"sctlr_el1", read: read_sysreg::<spec::SCTLR_EL1> },
    SysregEntry { name: c"spsr_el1", read: read_sysreg::<spec::SPSR_EL1> },
    SysregEntry { name: c"tcr_el1", read: read_sysreg::<spec::TCR_EL1> },
    SysregEntry { name: c"tpidrro_el0", read: read_sysreg::<spec::TPIDRRO_EL0> },
    SysregEntry { name: c"tpidr_el1", read: read_sysreg::<spec::TPIDR_EL1> },
    SysregEntry { name: c"ttbr0_el1", read: read_sysreg::<spec::TTBR0_EL1> },
    SysregEntry { name: c"ttbr1_el1", read: read_sysreg::<spec::TTBR1_EL1> },
    SysregEntry { name: c"vbar_el1", read: read_sysreg::<spec::VBAR_EL1> },
    // Generic Timer regs
    SysregEntry { name: c"cntfrq_el0", read: read_sysreg::<spec::CNTFRQ_EL0> },
    SysregEntry { name: c"cntkctl_el1", read: read_sysreg::<spec::CNTKCTL_EL1> },
    SysregEntry { name: c"cntpct_el0", read: read_sysreg::<spec::CNTPCT_EL0> },
    SysregEntry { name: c"cntps_ctl_el1", read: read_sysreg::<spec::CNTPS_CTL_EL1> },
    SysregEntry { name: c"cntps_cval_el1", read: read_sysreg::<spec::CNTPS_CVAL_EL1> },
    SysregEntry { name: c"cntps_tval_el1", read: read_sysreg::<spec::CNTPS_TVAL_EL1> },
    SysregEntry { name: c"cntp_ctl_el0", read: read_sysreg::<spec::CNTP_CTL_EL0> },
    SysregEntry { name: c"cntp_cval_el0", read: read_sysreg::<spec::CNTP_CVAL_EL0> },
    SysregEntry { name: c"cntp_tval_el0", read: read_sysreg::<spec::CNTP_TVAL_EL0> },
    SysregEntry { name: c"cntvct_el0", read: read_sysreg::<spec::CNTVCT_EL0> },
    SysregEntry { name: c"cntv_ctl_el0", read: read_sysreg::<spec::CNTV_CTL_EL0> },
    SysregEntry { name: c"cntv_cval_el0", read: read_sysreg::<spec::CNTV_CVAL_EL0> },
    SysregEntry { name: c"cntv_tval_el0", read: read_sysreg::<spec::CNTV_TVAL_EL0> },
];

fn print_sysreg(regname: &CStr) {
    let mut printed = false;
    for sysreg in &SYSREGS_LIST {
        if regname.to_bytes().eq_ignore_ascii_case(sysreg.name.to_bytes()) {
            kprint!("{:s} = {:016x}\n", sysreg.name, (sysreg.read)());
            printed = true;
        }
    }

    if !printed {
        kprint!(
            concat!(
                "Could not find register {:s} in list (you may need to add it to ",
                "kernel/arch/arm64/src/sysreg.rs)\n"
            ),
            regname
        );
    }
}

fn print_sysregs_list() {
    kprint!(" system register name: \n");
    for sysreg in &SYSREGS_LIST {
        kprint!("      {:s} \n", sysreg.name);
    }
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs` whose `arg_str` fields point to
/// NUL-terminated strings.
unsafe extern "C" fn cmd_sysreg(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 2 {
        kprint!("not enough arguments\n");
        kprint!("usage:\n");
        kprint!("sysreg list \n");
        kprint!("sysreg <register_name> \n");
        return -1;
    }
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`, and
    // `argc` is at least 2 here.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    // SAFETY: The caller guarantees that each argument's `arg_str` points to a NUL-terminated
    // string.
    let regname = unsafe { CStr::from_ptr(args[1].arg_str) };
    if regname.to_bytes() == b"list" {
        print_sysregs_list();
    } else {
        print_sysreg(regname);
    }

    0
}

static_command!(
    CMD_SYSREG,
    c"sysreg".as_ptr(),
    c"read armv8 system register".as_ptr(),
    cmd_sysreg,
    CMD_AVAIL_NORMAL
);
