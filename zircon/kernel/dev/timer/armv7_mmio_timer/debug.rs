// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::Armv7MmioTimer;
use super::registers::{PrintableReg, RegisterMmio, cnt_base, cnt_ctl_base, cnt_el0_base};
#[cfg(console_enabled)]
use super::{IrqHandler, IrqHandlerResult, Type};
#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
#[cfg(console_enabled)]
use crate::kernel::types::{BOOT_CPU_ID, cpu_num_to_mask};
use core::ffi::c_int;
use kprint::{kformat, kprint, kprintln};
use regio::{Offset, RwSafe};
#[cfg(console_enabled)]
use zx_status::Status;

const NS_PER_SEC: i64 = super::NS_PER_SEC as i64;
#[cfg(console_enabled)]
const NS_PER_MSEC: i64 = 1_000_000;
const NS_PER_USEC: i64 = 1_000;

fn dump_reg<T: PrintableReg>(tag: &str, r: T) {
    if T::PRINTER_ENABLED {
        const RAW_FIELD_NAME: &str = "raw";
        let mut max_name_width = RAW_FIELD_NAME.len();
        let mut field_count: u32 = 0;

        r.for_each_field(|name, _, _, _| {
            field_count += 1;
            max_name_width = max_name_width.max(name.len());
        });

        if field_count > 0 {
            let field_printer = |name: &str, val: u64, hi: u32, lo: u32| {
                const SPACES: &str = "                                ";
                let hi_pad = &SPACES[..max_name_width
                    .saturating_sub(if hi >= 10 { 2 } else { 1 })
                    .min(SPACES.len())];
                let lo_pad = &SPACES[..max_name_width
                    .saturating_sub(if lo >= 10 { 2 } else { 1 })
                    .min(SPACES.len())];
                let pad = &SPACES[..max_name_width.saturating_sub(name.len()).min(SPACES.len())];
                if T::HEX_WIDTH == 16 {
                    kprintln!(
                        "[{hi_pad:s}{hi:u}:{lo_pad:s}{lo:u}] : {pad:s}{name:s} : 0x{val:016x}"
                    );
                } else {
                    kprintln!(
                        "[{hi_pad:s}{hi:u}:{lo_pad:s}{lo:u}] : {pad:s}{name:s} : 0x{val:08x}"
                    );
                }
            };

            field_printer(RAW_FIELD_NAME, r.reg_value(), T::DIGITS - 1, 0);
            r.for_each_field(field_printer);
            kprint!("\n");
            return;
        }
    }

    let val = r.reg_value();
    if T::HEX_WIDTH == 16 {
        kprintln!("{tag:s} : 0x{val:>16x} ({val:u})");
    } else {
        kprintln!("{tag:s} : 0x{val:>8x} ({val:u})");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SkipZeros {
    No,
    Yes,
}

fn dump_reg_array<R: PrintableReg>(
    name: &str,
    base: &RegisterMmio,
    reg_count: u32,
    get_reg: fn(u32) -> Offset<R, RwSafe>,
    skip_zeros: SkipZeros,
) {
    for i in 0..reg_count {
        let mut tag_buf = [0u8; 32];
        let tag_bytes = kformat!(&mut tag_buf, "{name:s}[{i}]");
        let tag = core::str::from_utf8(tag_bytes).unwrap_or("");

        let r = R::read_from(base, get_reg(i));
        if (skip_zeros == SkipZeros::No) || (r.reg_value() != 0) {
            dump_reg(tag, R::read_from(base, get_reg(i)));
        }
    }
}

impl Armv7MmioTimer {
    /// Dumps register state for all timers matching `timer_mask`, returning `-1` if no ARMv7 MMIO
    /// timer hardware was detected during initialization and `0` otherwise.
    pub fn dump(timer_mask: u8) -> c_int {
        let mmio_ctl = Self::mmio_ctl();
        if mmio_ctl.base() == 0 {
            kprintln!("No ARMv7 MMIO Timer hardware was detected during initialization");
            return -1;
        }

        let cnttidr = mmio_ctl.read32(cnt_ctl_base::CNTTIDR);
        if timer_mask == 0xff {
            kprintln!("ARMv7 MMIO Timer CNTCTL");

            // FRQ and NSAR are only accessible from a secure context.  We _could_ print
            // them here, but they are all just going to read as zero.
            // dump_reg("CNTFRQ", mmio_ctl.read32(cnt_ctl_base::CNTFRQ));
            // dump_reg("CNTNSAR", mmio_ctl.read32(cnt_ctl_base::CNTNSAR));
            dump_reg("CNTTIDR", mmio_ctl.read32(cnt_ctl_base::CNTTIDR));
            dump_reg_array(
                "CNTACR",
                &mmio_ctl,
                cnt_ctl_base::CNTACR_REG_COUNT,
                cnt_ctl_base::cnt_acr,
                SkipZeros::No,
            );
            dump_reg_array(
                "CNTVOFF",
                &mmio_ctl,
                cnt_ctl_base::CNTVOFF_REG_COUNT,
                cnt_ctl_base::cnt_voff,
                SkipZeros::No,
            );
            dump_reg_array(
                "CounterID",
                &mmio_ctl,
                cnt_ctl_base::COUNTER_ID_REG_COUNT,
                cnt_ctl_base::counter_id,
                SkipZeros::Yes,
            );
            kprintln!("");
        }

        for i in 0..Self::MAX_TIMERS as u32 {
            if (timer_mask & (1u8 << i)) == 0 {
                continue;
            }

            let has_impl = ((cnttidr.bits() >> (i * 4)) & 0x1) != 0;
            let has_virt = ((cnttidr.bits() >> (i * 4)) & 0x2) != 0;
            let has_el0 = ((cnttidr.bits() >> (i * 4)) & 0x4) != 0;

            if !has_impl {
                continue;
            }

            let Some(timer) = Self::get(i as usize) else {
                kprintln!("Failed to discover timer frame {i}");
                continue;
            };

            kprintln!("ARMv7 MMIO Timer Frame {i} CNT");
            if timer.pct_timer().supported() {
                kprintln!("Phys IRQ : {}", timer.pct_timer().irq());
            }
            if timer.vct_timer().supported() {
                kprintln!("Virt IRQ : {}", timer.vct_timer().irq());
            }

            let el1acr = mmio_ctl.read32(cnt_ctl_base::cnt_acr(i));
            let el0acr = timer.mmio.read32(cnt_base::CNTEL0ACR);

            if el1acr.rfrq() {
                dump_reg(" CNTFRQ", timer.mmio.read32(cnt_base::CNTFRQ));
            }
            if el1acr.rpct() {
                dump_reg(" CNTPCT", timer.mmio.read64(cnt_base::CNTPCT));
            }
            if has_virt && el1acr.rvct() {
                dump_reg(" CNTVCT", timer.mmio.read64(cnt_base::CNTVCT));
            }
            if has_virt && el1acr.rvoff() {
                dump_reg("CNTVOFF", timer.mmio.read64(cnt_base::CNTVOFF));
            }
            dump_reg("CNTEL0ACR", el0acr);
            if el1acr.rwpt() {
                dump_reg("CNTP_CVAL", timer.mmio.read64(cnt_base::CNTP_CVAL));
                dump_reg("CNTP_TVAL", timer.mmio.read32(cnt_base::CNTP_TVAL));
                dump_reg("CNTP_CTL", timer.mmio.read32(cnt_base::CNTP_CTL));
            }
            if el1acr.rwvt() && has_virt {
                dump_reg("CNTV_CVAL", timer.mmio.read64(cnt_base::CNTV_CVAL));
                dump_reg("CNTV_TVAL", timer.mmio.read32(cnt_base::CNTV_TVAL));
                dump_reg("CNTV_CTL", timer.mmio.read32(cnt_base::CNTV_CTL));
            }
            dump_reg_array(
                "CounterID",
                &timer.mmio,
                cnt_base::COUNTER_ID_REG_COUNT,
                cnt_base::counter_id,
                SkipZeros::Yes,
            );
            kprintln!("");

            if has_el0 {
                if timer.el0_mmio.base() == 0 {
                    kprintln!("Failed to discover timer EL0 frame {i}");
                } else {
                    kprintln!("ARMv7 MMIO Timer Frame {i} EL0 CNT");
                    if el0acr.el0pcten() || el0acr.el0vcten() {
                        dump_reg("CNTFRQ", timer.el0_mmio.read32(cnt_el0_base::CNTFRQ));
                    }
                    if el0acr.el0pcten() {
                        dump_reg("CNTPCT", timer.el0_mmio.read64(cnt_el0_base::CNTPCT));
                    }
                    if has_virt && el0acr.el0vcten() {
                        dump_reg("CNTVCT", timer.el0_mmio.read64(cnt_el0_base::CNTVCT));
                    }
                    if el0acr.el0pten() {
                        dump_reg("CNTP_CVAL", timer.el0_mmio.read64(cnt_el0_base::CNTP_CVAL));
                        dump_reg("CNTP_TVAL", timer.el0_mmio.read32(cnt_el0_base::CNTP_TVAL));
                        dump_reg("CNTP_CTL", timer.el0_mmio.read32(cnt_el0_base::CNTP_CTL));
                    }
                    if has_virt && el0acr.el0vten() {
                        dump_reg("CNTV_CVAL", timer.el0_mmio.read64(cnt_el0_base::CNTV_CVAL));
                        dump_reg("CNTV_TVAL", timer.el0_mmio.read32(cnt_el0_base::CNTV_TVAL));
                        dump_reg("CNTV_CTL", timer.el0_mmio.read32(cnt_el0_base::CNTV_CTL));
                    }
                    dump_reg_array(
                        "CounterID",
                        &timer.el0_mmio,
                        cnt_el0_base::COUNTER_ID_REG_COUNT,
                        cnt_el0_base::counter_id,
                        SkipZeros::Yes,
                    );
                    kprintln!("");
                }
            }
        }

        0
    }

    /// Prints the status and remaining deadline for all timers matching `timer_mask`, returning `0`.
    pub fn show_status(timer_mask: u8) -> c_int {
        for i in 0..Self::MAX_TIMERS as u32 {
            if (timer_mask & (1u8 << i)) != 0 {
                let Some(timer) = Self::get(i as usize) else {
                    if timer_mask.count_ones() == 1 {
                        kprintln!("Timer[{i}] : does not exist");
                    }
                    continue;
                };

                if !timer.pct_timer.supported() && !timer.vct_timer.supported() {
                    kprintln!("Timer[{i}] : no timers supported");
                    continue;
                }

                let pvtimers = [timer.pct_timer(), timer.vct_timer()];
                for t in pvtimers {
                    if t.supported() {
                        if let Ok(rem) = t.time_until_deadline() {
                            kprintln!(
                                "Timer[{i}] : {:s} {} fires in {}.{:06} sec",
                                t.type_name(),
                                t.ticks(),
                                rem / NS_PER_SEC,
                                (rem % NS_PER_SEC) / NS_PER_USEC
                            );
                        } else {
                            kprintln!("Timer[{i}] : {:s} {} <disabled>", t.type_name(), t.ticks());
                        }
                    }
                }
            }
        }

        0
    }
}

#[cfg(console_enabled)]
fn mask_from_arg(arg: &CmdArgs) -> u8 {
    if arg.as_str() == "all" {
        0xFF
    } else if (arg.arg_uint as usize) < Armv7MmioTimer::MAX_TIMERS {
        1u8 << arg.arg_uint
    } else {
        0
    }
}

/// # Safety
///
/// `argc` must be at least 1 and `argv` must point to a valid array of `CmdArgs` of length `argc`.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_av7t(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: Caller guarantees `argv` is valid for `argc` elements (`argc >= 1`).
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    let usage = || -> c_int {
        let cmd_name = args[0].as_str();
        kprintln!("usage:");
        kprintln!("{cmd_name:s} dump <all|0-7>");
        kprintln!("{cmd_name:s} status <all|0-7>");
        kprintln!("{cmd_name:s} set <0-7> (pct|vct) <msec>");
        zx_status::sys::ZX_OK
    };

    if argc < 2 {
        return usage();
    }

    let subcmd = args[1].as_str();
    if subcmd == "dump" {
        let timer_mask = if argc == 3 { mask_from_arg(&args[2]) } else { 0 };
        return if timer_mask != 0 { Armv7MmioTimer::dump(timer_mask) } else { usage() };
    } else if subcmd == "status" {
        let timer_mask = if argc == 3 { mask_from_arg(&args[2]) } else { 0 };
        return if timer_mask != 0 { Armv7MmioTimer::show_status(timer_mask) } else { usage() };
    } else if subcmd == "set" {
        if argc != 5 {
            kprint!("Bad argument count {argc} for set command");
            return usage();
        }

        let timer_ndx = args[2].arg_uint as usize;
        if timer_ndx >= Armv7MmioTimer::MAX_TIMERS {
            kprintln!("Bad timer index {}", args[2].arg_uint);
            return usage();
        }

        let type_arg = args[3].as_str();
        let timer_type = if type_arg == "pct" {
            Type::Pct
        } else if type_arg == "vct" {
            Type::Vct
        } else {
            kprintln!("Unrecognized timer type \"{type_arg:s}\"");
            return usage();
        };

        let Some(timer) = Armv7MmioTimer::get(timer_ndx) else {
            kprintln!("Timer {} not found", args[2].arg_uint);
            return zx_status::sys::ZX_OK;
        };

        let inner_timer =
            if timer_type == Type::Pct { timer.pct_timer() } else { timer.vct_timer() };

        if !inner_timer.supported() {
            kprintln!("Timer {}:{type_arg:s} is not supported.", args[2].arg_uint);
            return Status::NOT_SUPPORTED.into_raw();
        }

        let res = inner_timer.set_handler(
            IrqHandler::new(move || {
                kprintln!("{:s} Timer FIRED (IRQ {})!", inner_timer.type_name(), inner_timer.irq());
                IrqHandlerResult::Unregister
            }),
            cpu_num_to_mask(BOOT_CPU_ID),
        );

        if let Err(status) = res {
            kprintln!("Timer {}:{type_arg:s} is busy!", args[2].arg_uint);
            return status.into_raw();
        }

        kprintln!(
            "Setting timer {}:{type_arg:s} to fire in {} mSec",
            args[2].arg_uint,
            args[4].arg_uint
        );
        let res =
            inner_timer.set_relative_timer((args[4].arg_uint as i64).saturating_mul(NS_PER_MSEC));

        if let Err(status) = res {
            kprintln!(
                "Failed to set timer {}:{type_arg:s} (err = {})",
                args[2].arg_uint,
                status.into_raw()
            );
            return status.into_raw();
        }
        return zx_status::sys::ZX_OK;
    }

    kprintln!("unknown command");
    usage()
}

#[cfg(console_enabled)]
static_command!(
    CMD_AV7T,
    c"av7t".as_ptr(),
    c"ARMv7 Timer Command".as_ptr(),
    cmd_av7t,
    CMD_AVAIL_ALWAYS
);
