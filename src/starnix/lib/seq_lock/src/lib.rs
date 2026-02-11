// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_logging::with_zx_name;
use std::arch::asm;
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use zerocopy::{Immutable, IntoBytes};
use zx::HandleBased as _;

/// Declare an instance of [`SeqLock`] by supplying header([`H`]) and value([`T`]) types,
/// which should be configured with C-style layout & alignment.
/// [`SeqLock`] will place a 32-bit atomic sequence number in-between the
/// header and value, in a VMO.
pub struct SeqLock<H: IntoBytes + Immutable, T: IntoBytes + Immutable> {
    map_addr: usize,
    readonly_vmo: Arc<zx::Vmo>,
    _phantom_data: PhantomData<(H, T)>,
}

impl<H: IntoBytes + Default + Immutable, T: IntoBytes + Default + Immutable> SeqLock<H, T> {
    pub fn new_default() -> Result<Self, zx::Status> {
        Self::new(H::default(), T::default())
    }
}

const fn sequence_offset<H>() -> usize {
    let offset = size_of::<H>();
    assert!(offset % align_of::<AtomicU32>() == 0, "Sequence must be correctly aligned");
    offset
}

const fn value_offset<H, T>() -> usize {
    let offset = sequence_offset::<H>() + size_of::<AtomicU32>();
    assert!(offset % align_of::<T>() == 0, "Value alignment must allow packing without padding");
    offset
}

const fn vmo_size<H, T>() -> usize {
    value_offset::<H, T>() + size_of::<T>()
}

impl<H: IntoBytes + Immutable, T: IntoBytes + Immutable> SeqLock<H, T> {
    /// Returns an instance with initial values and a read-only VMO handle.
    /// May fail if the VMO backing the structure cannot be created, duplicated
    /// read-only, or mapped.
    pub fn new(header: H, value: T) -> Result<Self, zx::Status> {
        // Create a VMO sized to hold the header, value, and sequence number.
        let writable_vmo =
            with_zx_name(zx::Vmo::create(vmo_size::<H, T>() as u64)?, b"starnix:selinux");

        // SAFETY: This is ok because there are no other references to this memory.
        return unsafe { Self::new_from_vmo(header, value, writable_vmo) };
    }

    /// Same as new() except that we can pass in an existing Vmo. This means that the
    /// first part of the Vmo is a SeqLock.
    ///
    /// # Safety
    ///
    /// Callers must guarantee that any other references to this memory will
    /// only make aligned atomic accesses to the sequence offset within the memory
    /// or to fields of H or T.
    pub unsafe fn new_from_vmo(
        header: H,
        value: T,
        writable_vmo: zx::Vmo,
    ) -> Result<Self, zx::Status> {
        // Populate the initial default values.
        writable_vmo.write(header.as_bytes(), 0)?;
        writable_vmo.write(value.as_bytes(), value_offset::<H, T>() as u64)?;

        // Create a readonly handle to the VMO.
        let writable_rights = writable_vmo.basic_info()?.rights;
        let readonly_rights = writable_rights.difference(zx::Rights::WRITE);
        let readonly_vmo = Arc::new(writable_vmo.duplicate_handle(readonly_rights)?);

        // Map the VMO writable by this object, and populate it.
        let flags = zx::VmarFlags::PERM_READ
            | zx::VmarFlags::ALLOW_FAULTS
            | zx::VmarFlags::REQUIRE_NON_RESIZABLE
            | zx::VmarFlags::PERM_WRITE;

        let status = Self {
            map_addr: fuchsia_runtime::vmar_root_self().map(
                0,
                &writable_vmo,
                0,
                vmo_size::<H, T>(),
                flags,
            )?,
            readonly_vmo: readonly_vmo,
            _phantom_data: PhantomData,
        };

        Ok(status)
    }

    /// Returns a read-only handle to the VMO containing the header, atomic
    /// sequence number, and value.
    pub fn get_readonly_vmo(&self) -> Arc<zx::Vmo> {
        self.readonly_vmo.clone()
    }

    /// Updates the value directly. Uses Seqlock pattern.
    pub fn set_value(&self, value: T) {
        // All data in <T> must be stored with some form of atomic write.
        // Given two consecutive writes W1 and W2, it is technically possible for a
        // client to observe the data written by W2 before observing the
        // start-increment for W2. The reader observes the same post-W1/pre-W2
        // sequence number at both start and end of the read, so thinks everything
        // is consistent, but gets some mix of W1 and W2's data.
        // In order to synchronize correctly we must either:
        //
        // 1) Store all the data with any atomic ordering (i.e. relaxed)
        // 2) Store all the data with atomic-release
        // We've chosen to do the second.
        let starting_addr = self.map_addr + value_offset::<H, T>();

        // Convert T to u8s so that we can process in u32 chunks.
        const { assert!(align_of::<T>() == 4) };
        const { assert!(size_of::<T>() % 4 == 0) };
        let value_as_u8_bytes = value.as_bytes();
        let value_ptr_in_u32 = value.as_bytes().as_ptr().cast::<u32>();

        // Lock prior to writing.
        let sequence_addr = (self.map_addr + sequence_offset::<H>()) as *mut u32;
        // Don't use AtomicU32 fetch_add because it is undefined behavior to
        // access across mutually distrusting address spaces, which happens for the seq lock.
        // SAFETY: sequence_addr is a valid pointer because `map_addr` is sized to fit
        // `H` and `T` and unmapped when `self` is dropped.
        let old_sequence = unsafe { atomic_fetch_add_u32_acq_rel(sequence_addr, 1) };
        // Old `sequence` value must always be even (i.e. unlocked) before writing.
        assert!((old_sequence % 2) == 0, "expected sequence to be unlocked");

        // Process and write to memory in u32 chunks.
        for i in 0..(value_as_u8_bytes.len() / size_of::<u32>()) {
            let current_value_addr = starting_addr + (i * size_of::<u32>());
            // SAFETY: We checked alignment and size above so we know that this points to
            // the valid current u32 value.
            let current_value = unsafe { *value_ptr_in_u32.add(i) };

            // Use asm to write u32 chunk so that the values are being written
            // atomically between processes. Don't use std::sync::atomic because that
            // only syncs writes within the Rust abstract machine.
            // SAFETY: Caller has verified that no one else is writing to this exact memory, and
            // that both currrent_value_addr and value_as_u64 are valid.
            unsafe { atomic_store_u32_release(current_value_addr, current_value) };
        }

        // Unlock after all writing is done.
        // SAFETY: sequence_addr is a valid pointer as per above SAFETY comment.
        let _ = unsafe { atomic_fetch_add_u32_acq_rel(sequence_addr, 1) };
    }

    /// Retrieves the memory address of the beginning of the handle part of the VMO.
    /// You can use this to point to a param you want to edit (e.g. with an offset).
    pub fn get_map_address(&mut self) -> *const T {
        let address = self.map_addr;
        return std::ptr::with_exposed_provenance::<T>(address);
    }
}

/// This performs an atomic store-release of a 32-bit value to `addr`.
/// Use this if you have a u32 or your struct is align(4).
///
/// Rust's memory model defines how atomics work across threads, but
/// doesn't account for the way Starnix handles access across mutually distrusting
/// address spaces.
/// This Seqlock is intended to be mapped and read by different address spaces. Rust's
/// guarantees do not apply and reading across these address spaces is undefined behavior.
/// Theoretically the Rust compiler could determine that the atomic is never read
/// from within the process and optimize out the store. We work around this by directly
/// including the assembly an atomic would generate to prevent the compiler from
/// "helpfully" optimizing it away.
///
/// # Safety
///
/// 1. The caller must ensure `addr` is valid and 4-byte aligned. The `addr` must be
///    writable by the current process. You can check: if std::ptr::write_volatile()
///    is able to write successfully to this `addr`, then this should work too.
/// 2. The caller must ensure that no other non-atomic operations are
///    occurring on this memory address simultaneously.
pub unsafe fn atomic_store_u32_release(addr: usize, value: u32) {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    compile_error!("This architecture is not supported");

    // SAFETY: Caller must provide a valid `addr` and `value`. The asm directly
    // stores the value to that addr. The original value may not have been a u32
    // (e.g. it's a SeLinuxStatusValue struct); caller is responsible to break struct
    // into valid u32 chunks.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            asm!(
                "mov [{ptr}], {val:e}",
                ptr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            asm!(
                "stlr {val:w}, [{ptr}]",
                ptr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "riscv64")]
        {
            asm!(
                "fence rw, w",
                "sw {val}, 0({ptr})",
                ptr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
    }
}

/// This performs an atomic fetch-add with Acquire and Release ordering of `val`
/// to a 32-bit value at `ptr`. Use this to update the u32 lock.
///
/// Rust's memory model defines how atomics work across threads, but
/// doesn't account for the way Starnix handles access across mutually distrusting
/// address spaces.
/// This Seqlock is intended to be mapped and read by different address spaces. Rust's
/// guarantees do not apply and reading across these address spaces is undefined behavior.
/// Theoretically the Rust compiler could determine that the atomic is never read
/// from within the process and optimize out the store. We work around this by directly
/// including the assembly an atomic would generate to prevent the compiler from
/// "helpfully" optimizing it away.
///
/// # Safety
/// The caller must ensure `ptr` is valid. The `ptr` must be writable by the current process.
pub unsafe fn atomic_fetch_add_u32_acq_rel(ptr: *mut u32, value: u32) -> u32 {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    compile_error!("This architecture is not supported");

    let old_value: u32;
    // SAFETY: Caller must provide a valid `ptr` and `value`. The asm directly
    // updates the value at that ptr.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            asm!(
                "lock xadd [{ptr}], {val:e}",
                ptr = in(reg) ptr,
                val = inout(reg) value => old_value,
                options(nostack),
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            asm!(
                "1:",
                "ldaxr {old:w}, [{ptr}]",
                "add {tmp:w}, {old:w}, {val:w}",
                "stlxr {status:w}, {tmp:w}, [{ptr}]",
                "cbnz {status:w}, 1b",
                ptr = in(reg) ptr,
                val = in(reg) value,
                old = out(reg) old_value,
                tmp = out(reg) _,
                status = out(reg) _,
                options(nostack),
            );
        }
        #[cfg(target_arch = "riscv64")]
        {
            asm!(
                "amoadd.w.aqrl {old}, {val}, ({ptr})",
                ptr = in(reg) ptr,
                val = in(reg) value,
                old = out(reg) old_value,
                options(nostack),
            );
        }
    }
    old_value
}

impl<H: IntoBytes + Immutable, T: IntoBytes + Immutable> Drop for SeqLock<H, T> {
    fn drop(&mut self) {
        // SAFETY: `self` owns the mapping, and does not dispense any references
        // to it.
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.map_addr, vmo_size::<H, T>())
                .expect("failed to unmap SeqLock");
        }
    }
}
