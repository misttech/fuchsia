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

const SEQUENCE_SIZE: usize = size_of::<AtomicU32>();

/// Byte size to use when incrementally writing out T in [`set_value()`]. Determined
/// by the params in T.
/// Four -> write in u32 chunks.
/// Eight -> write in u64 chunks, although the first 8 bytes may be two u32s (one
/// of which is the `sequence`).
#[derive(PartialEq)]
pub enum WriteSize {
    Four,
    Eight,
}

/// Types that are safe to be synchronized across address spaces using a Seqlock.
///
/// A type implementing this trait can optionally include the sequence as
/// its first field, indicated by `HAS_INLINE_SEQUENCE`. If it does not, [`SeqLock`]
/// will place a u32 atomic sequence number in between the header and value.
///
/// # Safety
///
/// Types implementing this trait guarantee that they can be safely written
/// to shared memory in chunks of `WRITE_SIZE` without introducing undefined
/// behavior for concurrent readers in other address spaces.
pub unsafe trait SeqLockable: IntoBytes + Immutable {
    /// The chunk size to use when writing to memory, either 4 or 8 bytes.
    const WRITE_SIZE: WriteSize;

    /// Indicates whether the type includes the u32 sequence as its first field.
    const HAS_INLINE_SEQUENCE: bool;

    /// Name used to identify the VMO for debugging.
    const VMO_NAME: &'static [u8];
}

/// Declare an instance of [`SeqLock`] by supplying header([`H`]) and value([`T`]) types,
/// which should be configured with C-style layout & alignment.
/// The value T can optionally include the sequence param as its first field (HAS_INLINE_SEQUENCE).
/// If you choose not to do that, [`SeqLock`] will place a u32 atomic sequence number
/// in between the header and value, in a VMO, shifting the value payload by `SEQUENCE_SIZE`.
pub struct SeqLock<H: IntoBytes + Immutable, T: SeqLockable> {
    map_addr: usize,
    readonly_vmo: Arc<zx::Vmo>,
    _phantom_data: PhantomData<(H, T)>,
}

impl<H: IntoBytes + Default + Immutable, T: SeqLockable + Default> SeqLock<H, T> {
    pub fn new_default() -> Result<Self, zx::Status> {
        Self::new(H::default(), T::default())
    }
}

/// Points to the sequence (lock) address.
/// This is always right after the H struct.
const fn sequence_offset<H>() -> usize {
    let offset = size_of::<H>();
    assert!(offset % align_of::<AtomicU32>() == 0, "Sequence must be correctly aligned");
    offset
}

impl<H: IntoBytes + Immutable, T: SeqLockable> SeqLock<H, T> {
    /// Points to the value address, adding any required padding if `sequence` is not inline.
    ///
    /// Example with inline sequence (HAS_INLINE_SEQUENCE = true):
    ///   H: 0
    ///   H: 4
    ///   T: 8 <-- points here, because `sequence` is the first param of T.
    ///   T: 12
    ///
    /// Example without inline sequence (HAS_INLINE_SEQUENCE = false):
    ///   H: 0
    ///   H: 4
    ///   [sequence]: 8
    ///   T: 12 <-- points here, after the added sequence.
    ///
    /// Some implementations (SeLinuxStatusValue) rely on SeqLock to track `sequence`, while
    /// some others (PerfMetadataValue) track `sequence` in T so that they can refer to it.
    const fn value_offset() -> usize {
        let offset = sequence_offset::<H>();
        assert!(
            offset % align_of::<T>() == 0,
            "Value alignment must allow packing without padding"
        );
        offset + if T::HAS_INLINE_SEQUENCE { 0 } else { SEQUENCE_SIZE }
    }

    /// Returns the total size of the VMO required to store the header, value, and sequence.
    const fn vmo_size() -> usize {
        Self::value_offset() + size_of::<T>()
    }

    /// Returns an instance with initial values and a read-only VMO handle.
    /// May fail if the VMO backing the structure cannot be created, duplicated
    /// read-only, or mapped.
    pub fn new(header: H, value: T) -> Result<Self, zx::Status> {
        // Create a VMO sized to hold the header H, value T, and sequence number.
        let vmo_size = Self::vmo_size();
        let writable_vmo = with_zx_name(zx::Vmo::create(vmo_size as u64)?, T::VMO_NAME);

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
        let value_offset = Self::value_offset();
        let vmo_size = Self::vmo_size();
        // Populate the initial default values.
        writable_vmo.write(header.as_bytes(), 0)?;
        writable_vmo.write(value.as_bytes(), value_offset as u64)?;

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
                vmo_size,
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

    /// Returns a read-only copy of the value as a T struct object.
    ///
    /// # Safety
    ///
    /// Only safe to use if there are no concurrent calls to `set_value()`.
    pub unsafe fn get(&self) -> T {
        let addr_ptr = (self.map_addr) as *const u8;
        // SAFETY: `addr` is formatted as H, u32, T, so we should be able to point to
        // the start of T by shifting by the value_offset().
        let value_ptr = unsafe { addr_ptr.add(Self::value_offset()) as *const T };
        // SAFETY: We know the data starting at the offset is a T struct.
        let value: T = unsafe { std::ptr::read_unaligned(value_ptr) };
        value
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
        let starting_addr = self.map_addr + Self::value_offset();

        // Convert T to u8s so that we can process in u32 or u64 chunks.
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

        // Process and write to memory in u32 or u64 chunks.
        const { assert!(align_of::<T>() == 4 || align_of::<T>() == 8) };
        // If T included the sequence number, we shouldn't write to it
        // (overwrite it) here. We should just skip it.
        let mut start_index = 0;
        if T::HAS_INLINE_SEQUENCE {
            start_index = 1;
        }

        if T::WRITE_SIZE == WriteSize::Four {
            assert!(align_of::<T>() == 4);
            for i in start_index..(value_as_u8_bytes.len() / size_of::<u32>()) {
                let current_value_addr = starting_addr + (i * size_of::<u32>());
                // SAFETY: We checked alignment and size above so we know that this points to
                // the valid current u32 value.
                let current_value = unsafe { *value_ptr_in_u32.add(i) };

                // Use asm to write u32 chunk so that the values are being written
                // atomically between address spaces. Don't use std::sync::atomic because that
                // only syncs writes within the Rust abstract machine.
                // SAFETY: Caller has verified that no one else is writing to this exact memory, and
                // that both currrent_value_addr and value_as_u64 are valid.
                unsafe { atomic_store_u32_release(current_value_addr as *mut u32, current_value) };
            }
        } else if T::WRITE_SIZE == WriteSize::Eight {
            assert!(align_of::<T>() == 8 && size_of::<T>() % 8 == 0);

            // When `WRITE_SIZE` is `Eight`, the memory is 8-byte aligned.
            // If `HAS_INLINE_SEQUENCE` is true, the 4-byte sequence lock occupies the
            // first half of an 8-byte block. We must skip that 4-byte sequence, perform a
            // 4-byte store for the remainder of that block, and then proceed with 8-byte stores.
            let mut offset_index = 0;

            if start_index == 1 {
                // Skip first u32 (sequence). Write next u32.
                let addr = starting_addr + (start_index * size_of::<u32>());
                // SAFETY: As a `SeqLockable`, the caller guarantees via `HAS_INLINE_SEQUENCE` that
                // the u32 sequence spans the first half of the 8-byte aligned block. This means that
                // getting the next u32 value (to sum up to a complete u64) is safe.
                let value = unsafe { *value_ptr_in_u32.add(start_index) };
                // SAFETY: Caller has verified that no one else is writing to this exact memory, and
                // that both addr and value are valid.
                unsafe { atomic_store_u32_release(addr as *mut u32, value) };

                offset_index += 1;
            }

            // Write the rest of the data using 8-byte stores.
            let value_ptr_in_u64 = value.as_bytes().as_ptr().cast::<u64>();
            for i in offset_index..(value_as_u8_bytes.len() / size_of::<u64>()) {
                let addr = starting_addr + (i * size_of::<u64>());
                // SAFETY: We checked alignment and size above so we know that this points to
                // the valid current u64 value.
                let value = unsafe { *value_ptr_in_u64.add(i) };

                // Use asm to write u64 chunk so that the values are being written
                // atomically between address spaces. Don't use std::sync::atomic because that
                // only syncs writes within the Rust abstract machine.
                // SAFETY: Caller has verified that no one else is writing to this exact memory, and
                // that both addr and value are valid.
                unsafe { atomic_store_u64_release(addr as *mut u64, value) };
            }
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
/// 1. The caller must ensure `addr` points to an address ptr that is valid and 4-byte
///    aligned. The `addr` must be writable by the current process.
/// 2. The caller must ensure that no other non-atomic operations are
///    occurring on this memory address simultaneously.
pub unsafe fn atomic_store_u32_release(addr: *mut u32, value: u32) {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    compile_error!("This architecture is not supported");

    // SAFETY: Caller must provide a valid `addr` and `value` as defined in the # Safety
    // section above. The asm directly stores the value to that ptr. The original value
    // may not have been a u32 (e.g. it's a SeLinuxStatusValue struct); caller is
    // responsible to break struct into valid u32 chunks.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            asm!(
                "mov [{addr}], {val:e}",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            asm!(
                "stlr {val:w}, [{addr}]",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "riscv64")]
        {
            asm!(
                "fence rw, w",
                "sw {val}, 0({addr})",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
    }
}

/// This performs an atomic fetch-add with Acquire and Release ordering of `val`
/// to a 32-bit value at `addr`. Use this to update the u32 lock.
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
/// The caller must ensure `addr` is valid. The `addr` must be writable by the current process.
pub unsafe fn atomic_fetch_add_u32_acq_rel(addr: *mut u32, value: u32) -> u32 {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    compile_error!("This architecture is not supported");

    let old_value: u32;
    // SAFETY: Caller must provide a valid `addr` and `value`. The asm directly
    // updates the value at that ptr.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            asm!(
                "lock xadd [{addr}], {val:e}",
                addr = in(reg) addr,
                val = inout(reg) value => old_value,
                options(nostack),
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            asm!(
                "1:",
                "ldaxr {old:w}, [{addr}]",
                "add {tmp:w}, {old:w}, {val:w}",
                "stlxr {status:w}, {tmp:w}, [{addr}]",
                "cbnz {status:w}, 1b",
                addr = in(reg) addr,
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
                "amoadd.w.aqrl {old}, {val}, ({addr})",
                addr = in(reg) addr,
                val = in(reg) value,
                old = out(reg) old_value,
                options(nostack),
            );
        }
    }
    old_value
}

/// This performs an atomic store-release of a 64-bit value to `addr`.
/// Use this if you have a u64 or your struct is align(8).
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
/// 1. The caller must ensure `addr` points to an address ptr that is valid and 8-byte
///    aligned. The `addr` must be writable by the current process.
/// 2. The caller must ensure that no other non-atomic operations are
///    occurring on this memory address simultaneously.
pub unsafe fn atomic_store_u64_release(addr: *mut u64, value: u64) {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    compile_error!("This architecture is not supported");

    // SAFETY: Caller must provide a valid `addr` and `value` as defined in the # Safety
    // section above. The asm directly stores the value to that ptr. The original value
    // may not have been a u64 (e.g. it's a PerfMetadataValue struct); caller is
    // responsible to break struct into valid u64 chunks.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            asm!(
                "mov [{addr}], {val}",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "aarch64")]
        {
            asm!(
                // Add memory barrier.
                "dmb ishst",
                // Use str instead of stlr to explicitly write only.
                // Otherwise stlr attempts to read first and we don't have permissions.
                "str {val}, [{addr}]",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "riscv64")]
        {
            asm!(
                "fence rw, w",
                "sd {val}, 0({addr})",
                addr = in(reg) addr,
                val = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
    }
}

impl<H: IntoBytes + Immutable, T: SeqLockable> Drop for SeqLock<H, T> {
    fn drop(&mut self) {
        // SAFETY: `self` owns the mapping, and does not dispense any references
        // to it.
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.map_addr, Self::vmo_size())
                .expect("failed to unmap SeqLock");
        }
    }
}
