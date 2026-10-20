// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use debug::dprintf;
use handoff::MappedMmioRange;

use crate::kernel::types::{PAddr, VAddr};

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
#[cfg(console_enabled)]
use core::ffi::{CStr, c_int};
#[cfg(console_enabled)]
use core::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
#[cfg(console_enabled)]
use kprint::kprint;
// Only the `pm` command reads and writes registers.
#[cfg(not(console_enabled))]
use mmio_ptr as _;
#[cfg(console_enabled)]
use zx_status::Status;

unsafe extern "C" {
    fn cpp_phys_handoff_periph_ranges(count: &mut usize) -> *const MappedMmioRange;
    fn cpp_mapped_mmio_range_paddr(range: &MappedMmioRange) -> u64;
    fn cpp_mapped_mmio_range_vaddr(range: &MappedMmioRange) -> usize;
    fn cpp_mapped_mmio_range_size_bytes(range: &MappedMmioRange) -> usize;
}

fn paddr(range: &MappedMmioRange) -> u64 {
    // SAFETY: `range` is a live `MappedMmioRange`, and the accessor only reads it.
    unsafe { cpp_mapped_mmio_range_paddr(range) }
}

fn vaddr(range: &MappedMmioRange) -> usize {
    // SAFETY: `range` is a live `MappedMmioRange`, and the accessor only reads it.
    unsafe { cpp_mapped_mmio_range_vaddr(range) }
}

fn size_bytes(range: &MappedMmioRange) -> usize {
    // SAFETY: `range` is a live `MappedMmioRange`, and the accessor only reads it.
    unsafe { cpp_mapped_mmio_range_size_bytes(range) }
}

#[cfg(console_enabled)]
fn paddr_end(range: &MappedMmioRange) -> u64 {
    paddr(range).wrapping_add(size_bytes(range) as u64)
}

#[cfg(console_enabled)]
fn vaddr_end(range: &MappedMmioRange) -> usize {
    vaddr(range).wrapping_add(size_bytes(range))
}

// Backed by permanent hand-off memory.
static PERIPH_RANGES: AtomicPtr<MappedMmioRange> = AtomicPtr::new(core::ptr::null_mut());
static PERIPH_RANGES_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Returns the ranges `record_periph_ranges` recorded, or none before it runs.
fn periph_ranges() -> &'static [MappedMmioRange] {
    let ranges = PERIPH_RANGES.load(Ordering::Relaxed);
    let count = PERIPH_RANGES_COUNT.load(Ordering::Relaxed);
    if ranges.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: `record_periph_ranges` stores each static once, on the boot CPU at
    // `LK_INIT_LEVEL_EARLIEST`, before any secondary CPU starts, and the check above returns
    // `&[]` unless both stores are visible. The pair describes the hand-off's peripheral ranges,
    // in permanent hand-off memory that stays mapped and is never written for the life of the
    // kernel, so the slice can be `'static`.
    unsafe { core::slice::from_raw_parts(ranges, count) }
}

fn record_periph_ranges(_level: init::LkInitLevel) {
    const EXPECTED_MAX: usize = 4;

    let mut count = 0usize;
    // SAFETY: `gPhysHandoff` stays valid until `EndHandoff` clears it, which is after the
    // `LK_INIT_LEVEL_EARLIEST` hooks run, and `count` is a live local that the callee fills in.
    let ranges = unsafe { cpp_phys_handoff_periph_ranges(&mut count) };
    PERIPH_RANGES.store(ranges.cast_mut(), Ordering::Relaxed);
    PERIPH_RANGES_COUNT.store(count, Ordering::Relaxed);
    if periph_ranges().len() > EXPECTED_MAX {
        dprintf!(
            INFO,
            "Warning: unexpected large number of PERIPHERAL ranges: {}",
            periph_ranges().len()
        );
    }
}

init::lk_init_hook!(record_periph_ranges, record_periph_ranges, init::LK_INIT_LEVEL_EARLIEST);

/// Address translation through the peripheral ranges, from a range's `src` address to its `dst`.
trait PeriphUtil {
    fn src(r: &MappedMmioRange) -> u64;
    fn dst(r: &MappedMmioRange) -> u64;

    /// Translate (without range checking) the (virt|phys) peripheral provided to its (phys|virt)
    /// counterpart using the provided range.
    fn translate(range: &MappedMmioRange, addr: u64) -> u64 {
        addr.wrapping_sub(Self::src(range)).wrapping_add(Self::dst(range))
    }

    /// Find the index (if any) of the peripheral range which contains the (virt|phys) address
    /// `addr`.
    fn lookup_ndx(addr: u64) -> Option<usize> {
        for (i, range) in periph_ranges().iter().enumerate() {
            if size_bytes(range) == 0 {
                break;
            } else if addr >= Self::src(range) {
                let offset = addr - Self::src(range);
                if offset < size_bytes(range) as u64 {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Map the (virt|phys) peripheral provided to its (phys|virt) counterpart (if any).
    fn map(addr: u64) -> Option<u64> {
        let ndx = Self::lookup_ndx(addr)?;
        Some(Self::translate(&periph_ranges()[ndx], addr))
    }
}

struct Phys2Virt;

impl PeriphUtil for Phys2Virt {
    fn src(r: &MappedMmioRange) -> u64 {
        paddr(r)
    }

    fn dst(r: &MappedMmioRange) -> u64 {
        vaddr(r) as u64
    }
}

struct Virt2Phys;

impl PeriphUtil for Virt2Phys {
    fn src(r: &MappedMmioRange) -> u64 {
        vaddr(r) as u64
    }

    fn dst(r: &MappedMmioRange) -> u64 {
        paddr(r)
    }
}

/// A register width with a single-access `mmio_ptr` read and write.
#[cfg(console_enabled)]
trait MmioReg: Into<u64> {
    /// Truncates `val` to this width.
    fn truncate(val: u64) -> Self;

    /// # Safety
    ///
    /// `addr` must be the address of a mapped MMIO register of this width.
    unsafe fn mmio_read(addr: *const Self) -> Self;

    /// # Safety
    ///
    /// `addr` must be the address of a mapped MMIO register of this width.
    unsafe fn mmio_write(val: Self, addr: *mut Self);
}

macro_rules! impl_mmio_reg {
    ($t:ty, $read:ident, $write:ident) => {
        #[cfg(console_enabled)]
        impl MmioReg for $t {
            fn truncate(val: u64) -> Self {
                val as $t
            }

            unsafe fn mmio_read(addr: *const Self) -> Self {
                // SAFETY: The caller guarantees `addr` is a mapped MMIO register of this width.
                unsafe { mmio_ptr::$read(addr) }
            }

            unsafe fn mmio_write(val: Self, addr: *mut Self) {
                // SAFETY: The caller guarantees `addr` is a mapped MMIO register of this width.
                unsafe { mmio_ptr::$write(val, addr) }
            }
        }
    };
}

impl_mmio_reg!(u8, read8, write8);
impl_mmio_reg!(u16, read16, write16);
impl_mmio_reg!(u32, read32, write32);
impl_mmio_reg!(u64, read64, write64);

/// # Safety
///
/// `addr` must be the virtual address of a `T`-aligned register in a peripheral mapping.
#[cfg(console_enabled)]
unsafe fn rd_reg<T: MmioReg>(addr: usize) -> u64 {
    // SAFETY: The caller guarantees `addr` is a mapped MMIO register of width `T`.
    unsafe { T::mmio_read(with_exposed_provenance(addr)) }.into()
}

/// # Safety
///
/// `addr` must be the virtual address of a `T`-aligned register in a peripheral mapping.
#[cfg(console_enabled)]
unsafe fn wr_reg<T: MmioReg>(addr: usize, val: u64) {
    // SAFETY: The caller guarantees `addr` is a mapped MMIO register of width `T`.
    unsafe { T::mmio_write(T::truncate(val), with_exposed_provenance_mut(addr)) }
}

/// Note; the choice of these values must also align with the definitions in the options array
/// below.
#[cfg(console_enabled)]
#[derive(Clone, Copy)]
enum AccessWidth {
    Byte = 0,
    Halfword = 1,
    Word = 2,
    Doubleword = 3,
}

/// How `pm` prints, reads and writes one access width.
#[cfg(console_enabled)]
struct DumpModOption {
    tag: &'static str,
    print: fn(u64),
    rd: unsafe fn(usize) -> u64,
    wr: unsafe fn(usize, u64),
    byte_width: u32,
}

#[cfg(console_enabled)]
static DUMP_MOD_OPTIONS: [DumpModOption; 4] = [
    DumpModOption {
        tag: "byte",
        print: |val| kprint!(" {:02x}", val),
        rd: rd_reg::<u8>,
        wr: wr_reg::<u8>,
        byte_width: 1,
    },
    DumpModOption {
        tag: "halfword",
        print: |val| kprint!(" {:04x}", val),
        rd: rd_reg::<u16>,
        wr: wr_reg::<u16>,
        byte_width: 2,
    },
    DumpModOption {
        tag: "word",
        print: |val| kprint!(" {:08x}", val),
        rd: rd_reg::<u32>,
        wr: wr_reg::<u32>,
        byte_width: 4,
    },
    DumpModOption {
        tag: "doubleword",
        print: |val| kprint!(" {:016x}", val),
        rd: rd_reg::<u64>,
        wr: wr_reg::<u64>,
        byte_width: 8,
    },
];

#[cfg(console_enabled)]
fn dump_periph(mut phys: usize, count: u64, width: AccessWidth) -> Result<(), Status> {
    let opt = &DUMP_MOD_OPTIONS[width as usize];

    // Sanity check count
    if count == 0 {
        kprint!("Illegal count {:u}\n", count);
        return Err(Status::INVALID_ARGS);
    }

    let byte_amt: u64 = count.wrapping_mul(u64::from(opt.byte_width));
    let phys_end_addr: usize = phys.wrapping_add(byte_amt as usize).wrapping_sub(1);

    // Sanity check alignment.
    if phys & (opt.byte_width.wrapping_sub(1) as usize) != 0 {
        kprint!("{:016x} is not aligned to a {:u} byte boundary!\n", phys, opt.byte_width);
        return Err(Status::INVALID_ARGS);
    }

    // Validate that the entire requested range fits within a single mapping.
    let start_ndx = Phys2Virt::lookup_ndx(phys as u64);
    let end_ndx = Phys2Virt::lookup_ndx(phys_end_addr as u64);
    let ndx = match (start_ndx, end_ndx) {
        (Some(start_ndx), Some(end_ndx)) if start_ndx == end_ndx => start_ndx,
        _ => {
            kprint!(
                "Physical range [{:016x}, {:016x}] is not contained in a single mapping!\n",
                phys,
                phys_end_addr
            );
            return Err(Status::INVALID_ARGS);
        }
    };

    // OK, all of our sanity checks are complete.  Time to start dumping data.
    const BYTES_PER_LINE: u32 = 16;
    let count_per_line = u64::from(BYTES_PER_LINE / opt.byte_width);
    let mut virt: usize = Phys2Virt::translate(&periph_ranges()[ndx], phys as u64) as usize;
    let virt_end_addr: usize = virt.wrapping_add(byte_amt as usize);

    kprint!(
        "Dumping {:u} {:s}{:s} starting at phys 0x{:016x}\n",
        count,
        opt.tag,
        if count == 1 { "" } else { "s" },
        phys
    );
    while virt < virt_end_addr {
        kprint!("{:016x} :", phys);
        let mut i: u64 = 0;
        while (i < count_per_line) && (virt < virt_end_addr) {
            // SAFETY: `virt` is below `virt_end_addr`, which is one past the translation of
            // `phys_end_addr` even when `byte_amt` wraps, so it lies in the translation of the
            // physical span checked above, which lies in a single range. A mapping keeps the
            // offset within a page, so `virt` has the `byte_width` alignment checked above.
            (opt.print)(unsafe { (opt.rd)(virt) });
            i += 1;
            virt = virt.wrapping_add(opt.byte_width as usize);
        }
        phys = phys.wrapping_add(BYTES_PER_LINE as usize);
        kprint!("\n");
    }

    Ok(())
}

#[cfg(console_enabled)]
fn mod_periph(phys: usize, val: u64, width: AccessWidth) -> Result<(), Status> {
    let opt = &DUMP_MOD_OPTIONS[width as usize];

    // Sanity check alignment.
    if phys & (opt.byte_width.wrapping_sub(1) as usize) != 0 {
        kprint!("{:016x} is not aligned to a {:u} byte boundary!\n", phys, opt.byte_width);
        return Err(Status::INVALID_ARGS);
    }

    // Translate address
    let vaddr = Phys2Virt::map(phys as u64);
    if vaddr.is_none() {
        kprint!("Physical addr {:016x} in not in the peripheral mappings!\n", phys);
    }

    // Perform the write, then report what we did.
    let vaddr = vaddr.expect("phys is not in the peripheral mappings") as usize;
    // SAFETY: `vaddr` is the translation of `phys` through the range that holds it. A mapping
    // keeps the offset within a page, so `vaddr` is `byte_width`-aligned like `phys`, and the
    // aligned access stays inside that mapped page.
    unsafe { (opt.wr)(vaddr, val) };
    kprint!("Wrote");
    (opt.print)(val);
    kprint!(" to phys addr {:016x}\n", phys);

    Ok(())
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs` whose `arg_str` fields point to
/// NUL-terminated strings.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_peripheral_map(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let cmd = args[0].arg_str;
    let usage = |not_enough_args: bool| -> c_int {
        if not_enough_args {
            kprint!("not enough arguments\n");
        }

        kprint!("usage:\n");
        kprint!("{:cs} dump\n", cmd);
        kprint!("{:cs} phys2virt <addr>\n", cmd);
        kprint!("{:cs} virt2phys <addr>\n", cmd);
        kprint!(
            concat!(
                "{:cs} dd|dw|dh|db <phys_addr> [<count>] :: Dump <count> (double|word|half|byte) ",
                "from <phys_addr> (count default = 1)\n"
            ),
            cmd
        );
        kprint!(
            concat!(
                "{:cs} md|mw|mh|mb <phys_addr> <value> :: Write the contents of <value> to the ",
                "(double|word|half|byte) at <phys_addr>\n"
            ),
            cmd
        );

        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        return usage(true);
    }

    // SAFETY: The caller guarantees that `arg_str` points to a NUL-terminated string, and `argc`
    // is at least 2 here.
    let arg1 = unsafe { CStr::from_ptr(args[1].arg_str) }.to_bytes();
    if arg1 == b"dump" {
        for range in periph_ranges() {
            debug_assert!(size_bytes(range) > 0);
            kprint!(
                "Phys [{:016x}, {:016x}) ==> Virt [{:16p}, {:016x}) (len 0x{:08x})\n",
                paddr(range),
                paddr_end(range),
                vaddr(range),
                vaddr_end(range),
                size_bytes(range)
            );
        }
        kprint!("Dumped {:u} defined peripheral map ranges\n", periph_ranges().len());
    } else if arg1 == b"phys2virt" || arg1 == b"virt2phys" {
        if argc < 3 {
            return usage(true);
        }

        let phys_src = arg1 == b"phys2virt";
        let map_fn: fn(u64) -> Option<u64> = if phys_src { Phys2Virt::map } else { Virt2Phys::map };
        let res = map_fn(args[2].arg_uint);
        if let Some(res) = res {
            kprint!("{:016x} ==> {:016x}\n", args[2].arg_uint, res);
        } else {
            kprint!(
                "Failed to find the {:s} address 0x{:016x} in the peripheral mappings.\n",
                if phys_src { "physical" } else { "virtual" },
                args[2].arg_uint
            );
        }
    } else if (arg1.first() == Some(&b'd')) || (arg1.first() == Some(&b'm')) {
        // If this is a valid display or modify command, its length will be exactly 2.
        if arg1.len() != 2 {
            return usage(false);
        }

        // Parse the next letter to figure out the width of the operation.
        let width = match arg1[1] {
            b'd' => AccessWidth::Doubleword,
            b'w' => AccessWidth::Word,
            b'h' => AccessWidth::Halfword,
            b'b' => AccessWidth::Byte,
            _ => return usage(false),
        };

        // A dump or modify needs an address.
        if argc < 3 {
            return usage(true);
        }

        let phys_addr = args[2].arg_uint as usize;
        if arg1[0] == b'd' {
            // Dump commands have a default count of 1
            return Status::result_into_raw(dump_periph(
                phys_addr,
                if argc < 4 { 1 } else { args[3].arg_uint },
                width,
            ));
        } else {
            // Modify commands are required to have a value.
            return if argc < 4 {
                usage(true)
            } else {
                Status::result_into_raw(mod_periph(phys_addr, args[3].arg_uint, width))
            };
        }
    } else {
        return usage(false);
    }

    zx_status::sys::ZX_OK
}

/// Translates a peripheral physical address to a virtual address in the big kernel map.
///
/// Returns 0 if `paddr` is not in the peripheral mappings.
#[unsafe(export_name = "periph_paddr_to_vaddr")]
pub extern "C" fn rust_periph_paddr_to_vaddr(paddr: usize) -> usize {
    let ret = Phys2Virt::map(paddr as u64);
    ret.unwrap_or(0) as usize
}

/// Translates a peripheral virtual address to a physical address in the big kernel map.
///
/// Returns 0 if `vaddr` is not in the peripheral mappings.
#[unsafe(export_name = "periph_vaddr_to_paddr")]
pub extern "C" fn rust_periph_vaddr_to_paddr(vaddr: usize) -> usize {
    let ret = Virt2Phys::map(vaddr as u64);
    ret.unwrap_or(0) as usize
}

/// Translates an arm64 peripheral physical address to a virtual address in the kernel periphmap.
///
/// Returns `None` if `paddr` is not in the peripheral mappings.
#[inline]
pub fn periph_paddr_to_vaddr(paddr: PAddr) -> Option<VAddr> {
    Phys2Virt::map(paddr.into()).map(VAddr::from)
}

#[cfg(console_enabled)]
static_command!(
    CMD_PM,
    c"pm".as_ptr(),
    c"peripheral mapping commands".as_ptr(),
    cmd_peripheral_map,
    CMD_AVAIL_ALWAYS
);
