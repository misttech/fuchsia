// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::arm64::periph_paddr_to_vaddr;
use crate::hw_rng::{HwRngOps, hw_rng_register};
use crate::kernel::thread::sleep_relative;
use crate::kernel::types::PAddr;
use crate::platform_rs::timer::DurationMono;
use bitrs::layout;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull, copy_nonoverlapping, with_exposed_provenance};
use explicit_memory::{ZeroOnDrop, mandatory_memset};
use kprint::kprint;
use regio::{MmioBank, MmioPtr, Offset, Ro};
use zbi::{DcfgQcomRng, QcomRngFlags};

zr::static_assert_size_and_align!(DcfgQcomRng, 16, 8);

ksync::declare_singleton_mutex!(QcomRngLock);

layout!({
    struct StatusRegister(u32);
    {
        let __ @ 31..1;
        let data_ready @ 0;
    }
});

const STATUS_REGISTER: Offset<StatusRegister, Ro> = Offset::new(0x4);

layout!({
    struct DataRegister(u32);
    {
        let data @ 31..0;
    }
});

const DATA_REGISTER: Offset<DataRegister, Ro> = Offset::new(0x0);

// Size of the register bank spanned by `DATA_REGISTER` and `STATUS_REGISTER`.
const MMIO_BANK_SIZE: usize = 0x8;

const RETRY_DELAY: DurationMono = DurationMono::from_micros(440);
const MAX_RETRIES: i32 = 5;

static mut MMIO_BASE: usize = 0;

/// # Safety
///
/// `mmio_base` must be a valid, 4-byte aligned virtual address of an `MMIO_BANK_SIZE`-byte
/// register bank that remains mapped and readable without trapping for the duration of the call.
unsafe fn get_entropy(mmio_base: usize, buf: &mut [MaybeUninit<u8>]) -> &mut [u8] {
    ksync::lock!(let _guard = QcomRngLock::lock());
    let mut buffer = &mut *buf;
    let mmio = MmioBank::new(
        // SAFETY: The caller guarantees `mmio_base` is a valid, 4-byte aligned virtual address
        // of an `MMIO_BANK_SIZE`-byte register bank for the duration of the call.
        unsafe { MmioPtr::<u32, Ro>::new(with_exposed_provenance(mmio_base)) },
        MMIO_BANK_SIZE,
    );
    // RNG data that exists in the current stack frame must be scrubbed before returning, preventing
    // any sort of "leaks". `explicit_memory` ensures none of these cleanup operations are elided by
    // the compiler.
    let mut data = ZeroOnDrop::new(0u32);
    let mut retries = 0;
    while !buffer.is_empty() {
        // SAFETY: `STATUS_REGISTER` lies within the bank guaranteed valid by the caller.
        let status_register = unsafe { mmio.at(STATUS_REGISTER) }.read();
        // When the HW RNG unit runs out of data, it must produce more. This bit is set when the
        // unit has produced data, and it can be read from the `DataRegister`.
        if !status_register.data_ready() {
            if retries == MAX_RETRIES {
                // We failed to generate enough random data, scrub everything and bail out.
                // SAFETY: `buf` is a valid mutable slice of `buf.len()` bytes.
                unsafe {
                    mandatory_memset(buf.as_mut_ptr().cast::<u8>(), 0, buf.len());
                }
                return &mut [];
            }
            retries += 1;
            let _ = sleep_relative(RETRY_DELAY);
            continue;
        }

        // SAFETY: As above, with `DATA_REGISTER` lying within the bank.
        *data = unsafe { mmio.at(DATA_REGISTER) }.read().data();
        let copy_size = buffer.len().min(size_of_val(&*data));
        // SAFETY: `data` and `buffer` are valid for `copy_size` bytes and do not overlap.
        unsafe {
            copy_nonoverlapping(ptr::from_ref(&*data).cast(), buffer.as_mut_ptr(), copy_size);
        }
        buffer = &mut buffer[copy_size..];
    }

    // SAFETY: Every byte of `buf` was initialized in the loop above.
    unsafe { buf.assume_init_mut() }
}

/// # Safety
///
/// If `len > 0`, `buf` must be `Some` and point to writable memory of at least `len` bytes
/// that is not accessed concurrently for the duration of the call. Must only be called after
/// `qcom_rng_init` has initialized the driver.
unsafe extern "C" fn hw_rng_get_entropy(buf: Option<NonNull<c_void>>, len: usize) -> usize {
    let buf = buf.map_or(core::ptr::null_mut(), NonNull::as_ptr).cast::<MaybeUninit<u8>>();
    // SAFETY: When `len > 0`, the caller guarantees `buf` is non-null and points to `len` writable
    // bytes that are not accessed concurrently for the duration of the call. When `len == 0`,
    // `zr::slice_from_raw_parts_mut` returns `&mut []` for any pointer, including null.
    let slice: &mut [MaybeUninit<u8>] = unsafe { zr::slice_from_raw_parts_mut(buf, len) };
    // SAFETY: Initialized once by `qcom_rng_init` during early boot before `hw_rng_get_entropy`
    // can be called.
    let mmio_base = unsafe { MMIO_BASE };
    // Return the number of bytes provided.
    // SAFETY: `mmio_base` was initialized by `qcom_rng_init` from a peripheral mapping that
    // remains valid for the lifetime of the kernel.
    unsafe { get_entropy(mmio_base, slice) }.len()
}

static OPS: HwRngOps = HwRngOps { hw_rng_get_entropy };

/// Initializes the driver.
///
/// # Safety
///
/// Must only be called once, during early boot single-threaded initialization before
/// secondary CPUs and multi-threading are started. `config.mmio_phys` must be the physical
/// address of the Qualcomm hardware RNG register bank in the peripheral memory map.
pub unsafe fn qcom_rng_init(config: &DcfgQcomRng) {
    // Check that the HWRNG unit has been handed off ready for use, otherwise complain and move on.
    if !config.flags.contains(QcomRngFlags::ENABLED) {
        kprint!("QCOM HWRNG: Handed off disabled.\n");
        return;
    }
    let mmio_base = periph_paddr_to_vaddr(PAddr(config.mmio_phys as usize))
        .expect("qcom rng registers not in peripheral map");
    // SAFETY: Called once during single-threaded early boot initialization.
    unsafe { MMIO_BASE = mmio_base.0 };
    // SAFETY: Registration occurs during early boot before multi-threading, and `OPS` is a
    // `'static` with a valid callback.
    unsafe { hw_rng_register(&OPS) };
    kprint!(
        "QCOM HWRNG: Registered {{mmio={:u},flags={:u}}}.\n",
        config.mmio_phys,
        config.mmio_phys
    );
}

/// FFI wrapper for C++ callers.
///
/// # Safety
///
/// Must only be called once, during early boot single-threaded initialization before
/// secondary CPUs and multi-threading are started. `config` must reference a valid,
/// initialized `DcfgQcomRng` with a valid hardware register address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_qcom_rng_init(config: &DcfgQcomRng) {
    // SAFETY: The caller guarantees early boot single-threaded invocation and valid hardware
    // config.
    unsafe { qcom_rng_init(config) };
}

/// Qualcomm HW RNG kernel unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "qcom_rng")]
mod tests {
    use super::get_entropy;
    use core::mem::MaybeUninit;
    use unittest::{assert_eq, assert_true};

    /// Tests drawing random bytes from an initialized, ready hardware RNG register bank.
    #[test]
    fn test_get_entropy_ready() {
        let regs: [u32; 2] = [0xdead_beef, 1];
        let mmio_base = regs.as_ptr().expose_provenance();

        // 0-byte draw returns an empty slice.
        let mut buf0: [MaybeUninit<u8>; 0] = [];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let out0 = unsafe { get_entropy(mmio_base, &mut buf0) };
        assert_eq!(out0.len(), 0);

        // Exact 4-byte word draw.
        let mut buf4 = [MaybeUninit::uninit(); 4];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let out4 = unsafe { get_entropy(mmio_base, &mut buf4) };
        assert_eq!(out4.len(), 4);
        assert_true!(out4 == 0xdead_beefu32.to_ne_bytes());

        // Partial word draw (2 bytes).
        let mut buf2 = [MaybeUninit::uninit(); 2];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let out2 = unsafe { get_entropy(mmio_base, &mut buf2) };
        assert_eq!(out2.len(), 2);
        assert_true!(out2 == &0xdead_beefu32.to_ne_bytes()[..2]);

        // Multi-word draw (6 bytes).
        let mut buf6 = [MaybeUninit::uninit(); 6];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let out6 = unsafe { get_entropy(mmio_base, &mut buf6) };
        assert_eq!(out6.len(), 6);
        let expected = [
            0xdead_beefu32.to_ne_bytes()[0],
            0xdead_beefu32.to_ne_bytes()[1],
            0xdead_beefu32.to_ne_bytes()[2],
            0xdead_beefu32.to_ne_bytes()[3],
            0xdead_beefu32.to_ne_bytes()[0],
            0xdead_beefu32.to_ne_bytes()[1],
        ];
        assert_true!(out6 == expected);
    }

    /// Tests that exhausting retries when data is not ready scrubs the buffer to zeroes.
    #[test]
    fn test_get_entropy_not_ready_scrubs_buffer() {
        let regs: [u32; 2] = [0xdead_beef, 0];
        let mmio_base = regs.as_ptr().expose_provenance();

        let mut buf = [MaybeUninit::new(0xa5u8); 8];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let out = unsafe { get_entropy(mmio_base, &mut buf) };
        assert_eq!(out.len(), 0);

        // Verify that the buffer was scrubbed with zeroes by `mandatory_memset`.
        for b in &buf {
            // SAFETY: The buffer was zeroed by `mandatory_memset`, so each byte is initialized
            // to 0.
            let val = unsafe { b.assume_init() };
            assert_eq!(val, 0);
        }
    }
}
