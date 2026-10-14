// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::x86::feature::{X86_FEATURE_RDRAND, X86_FEATURE_RDSEED, x86_feature_test};
use crate::hw_rng::{HwRngOps, hw_rng_register};
use core::arch::x86_64::{_rdrand64_step, _rdseed64_step};
use core::ffi::c_void;
use core::ptr::NonNull;
use zx_status::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntropyInstr {
    Rdseed,
    Rdrand,
}

/// Get entropy from the CPU using RDSEED.
///
/// `buf.len()` must be at most `isize::MAX`
///
/// It will retry the RDSEED instruction until `buf.len()` bytes are written to `buf`.
///
/// Returns the number of bytes written to the buffer on success (potentially 0),
/// and an error status on failure.
fn get_entropy_from_cpu(buf: &mut [u8]) -> Result<usize, Status> {
    // TODO(security, https://fxbug.dev/42105848): Move this to a shared kernel/user lib, so we can
    // write usermode tests against this code

    if buf.len() >= isize::MAX as usize {
        return Err(Status::INVALID_ARGS);
    }

    if x86_feature_test(X86_FEATURE_RDSEED) {
        // SAFETY: `x86_feature_test` confirmed the CPU supports RDSEED.
        return Ok(unsafe { get_entropy_from_rdseed(buf) });
    } else if x86_feature_test(X86_FEATURE_RDRAND) {
        // SAFETY: `x86_feature_test` confirmed the CPU supports RDRAND.
        return Ok(unsafe { get_entropy_from_rdrand(buf) });
    }

    // We don't have an entropy source
    Err(Status::NOT_SUPPORTED)
}

/// # Safety
///
/// The CPU must support the instruction selected by `instr` (`X86_FEATURE_RDSEED` for
/// [`EntropyInstr::Rdseed`], `X86_FEATURE_RDRAND` for [`EntropyInstr::Rdrand`]).
unsafe fn instruction_step(instr: EntropyInstr, val: &mut u64) -> bool {
    match instr {
        // SAFETY: The caller guarantees the CPU supports RDRAND, so calling the
        // `#[target_feature(enable = "rdrand")]` intrinsic is sound.
        EntropyInstr::Rdrand => unsafe { _rdrand64_step(val) != 0 },
        // SAFETY: The caller guarantees the CPU supports RDSEED, so calling the
        // `#[target_feature(enable = "rdseed")]` intrinsic is sound.
        EntropyInstr::Rdseed => unsafe { _rdseed64_step(val) != 0 },
    }
}

/// # Safety
///
/// The CPU must support the instruction selected by `instr`; see [`instruction_step`].
unsafe fn get_entropy_from_instruction(buf: &mut [u8], instr: EntropyInstr) -> usize {
    let len = buf.len();
    let mut written: usize = 0;
    while written < len {
        let mut val: u64 = 0;
        // SAFETY: The caller guarantees the CPU supports `instr`.
        if !unsafe { instruction_step(instr, &mut val) } {
            continue;
        }
        let to_copy = (len - written).min(core::mem::size_of_val(&val));
        buf[written..written + to_copy].copy_from_slice(&val.to_ne_bytes()[..to_copy]);
        written += to_copy;
    }
    debug_assert!(written == len);

    written
}

/// # Safety
///
/// The CPU must support the RDSEED instruction (`X86_FEATURE_RDSEED`).
unsafe fn get_entropy_from_rdseed(buf: &mut [u8]) -> usize {
    // SAFETY: The caller guarantees the CPU supports RDSEED.
    unsafe { get_entropy_from_instruction(buf, EntropyInstr::Rdseed) }
}

/// # Safety
///
/// The CPU must support the RDRAND instruction (`X86_FEATURE_RDRAND`).
unsafe fn get_entropy_from_rdrand(buf: &mut [u8]) -> usize {
    // TODO(security, https://fxbug.dev/42105846): This method is not compliant with Intel's
    // "Digital Random Number Generator (DRNG) Software Implementation Guide".  We are using rdrand
    // in a way that is explicitly against their recommendations.  This needs to be corrected, but
    // this fallback is a compromise to allow our development platforms that don't support RDSEED
    // to get some degree of hardware-based randomization.
    //
    // SAFETY: The caller guarantees the CPU supports RDRAND.
    unsafe { get_entropy_from_instruction(buf, EntropyInstr::Rdrand) }
}

/// # Safety
///
/// If `len > 0`, `buf` must be non-null and point to writable memory of at least `len` bytes
/// that is not accessed concurrently for the duration of the call.
unsafe extern "C" fn intel_hw_rng_get_entropy(buf: Option<NonNull<c_void>>, len: usize) -> usize {
    if len == 0 {
        return 0;
    }

    let buf = buf.expect("hw_rng buf must be non-null when len > 0");

    // SAFETY: `len > 0` and the caller guarantees `buf` is non-null and points to writable memory
    // of at least `len` bytes with no concurrent access for the duration of the call.
    let buf = unsafe { zr::slice_from_raw_parts_mut(buf.as_ptr().cast::<u8>(), len) };

    get_entropy_from_cpu(buf).unwrap_or(0)
}

static OPS: HwRngOps = HwRngOps { hw_rng_get_entropy: intel_hw_rng_get_entropy };

fn intel_rng_init(_level: init::LkInitLevel) {
    // SAFETY: Registration occurs during early boot before multi-threading, and `OPS` is a
    // `'static` with a valid callback.
    unsafe { hw_rng_register(&OPS) };
}

init::lk_init_hook!(
    intel_rng_init,
    intel_rng_init,
    init::LkInitLevel(init::LK_INIT_LEVEL_PLATFORM_EARLY.0 + 1)
);
