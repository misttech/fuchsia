// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::binder::TransactionError;
use starnix_core::mm::memory::MemoryObject;
use starnix_logging::{log_error, log_trace};
use starnix_types::user_buffer::UserBuffer;
use starnix_uapi::errors::Errno;
use starnix_uapi::math::round_up_to_increment;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{binder_uintptr_t, errno, error};
use std::collections::BTreeMap;
use zerocopy::IntoBytes;
use zx;

/// The mapped VMO shared between userspace and the binder driver.
///
/// The binder driver copies messages from one process to another, which essentially amounts to
/// a copy between VMOs. It is not possible to copy directly between VMOs without an intermediate
/// copy, and the binder driver must only perform one copy for performance reasons.
///
/// The memory allocated to a binder process is shared with the binder driver, and mapped into
/// the kernel's address space so that a VMO read operation can copy directly into the mapped VMO.
#[derive(Debug)]
pub struct SharedMemory {
    /// The address in kernel address space where the VMO is mapped.
    kernel_address: *mut u8,
    /// The address in user address space where the VMO is mapped.
    pub user_address: UserAddress,
    /// The length of the shared memory mapping in bytes.
    pub length: usize,
    /// The map from offset to size of all the currently active allocations, ordered in ascending
    /// order.
    ///
    /// This is used by the allocator to find new allocations.
    ///
    /// TODO(qsr): This should evolved into a better allocator for performance reason. Currently,
    /// each new allocation is done in O(n) where n is the number of currently active allocations.
    allocations: BTreeMap<usize, usize>,
}

/// The user buffers containing the data to send to the recipient of a binder transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionBuffers {
    /// The buffer containing the data of the transaction.
    pub data: UserBuffer,
    /// The buffer containing the offsets of objects inside the `data` buffer.
    pub offsets: UserBuffer,
    /// An optional buffer pointing to the security context of the client of the transaction.
    pub security_context: Option<UserBuffer>,
}

/// Contains the allocations for a transaction.
#[derive(Debug)]
pub struct SharedMemoryAllocation<'a> {
    pub data_buffer: SharedBuffer<'a, u8>,
    pub offsets_buffer: SharedBuffer<'a, binder_uintptr_t>,
    pub scatter_gather_buffer: SharedBuffer<'a, u8>,
    pub security_context_buffer: Option<SharedBuffer<'a, u8>>,
}

impl From<SharedMemoryAllocation<'_>> for TransactionBuffers {
    fn from(value: SharedMemoryAllocation<'_>) -> Self {
        Self {
            data: value.data_buffer.user_buffer(),
            offsets: value.offsets_buffer.user_buffer(),
            security_context: value.security_context_buffer.map(|x| x.user_buffer()),
        }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        log_trace!("Dropping shared memory allocation {:?}", self);
        let kernel_root_vmar = fuchsia_runtime::vmar_root_self();

        // SAFETY: This object hands out references to the mapped memory, but the borrow checker
        // ensures correct lifetimes.
        let res = unsafe { kernel_root_vmar.unmap(self.kernel_address as usize, self.length) };
        match res {
            Ok(()) => {}
            Err(status) => {
                log_error!("failed to unmap shared binder region from kernel: {:?}", status);
            }
        }
    }
}

// SAFETY: SharedMemory has exclusive ownership of the `kernel_address` pointer, so it is safe to
// send across threads.
unsafe impl Send for SharedMemory {}

impl SharedMemory {
    pub fn map(
        memory: &MemoryObject,
        user_address: UserAddress,
        length: usize,
    ) -> Result<Self, Errno> {
        // Map the VMO into the kernel's address space.
        let kernel_root_vmar = fuchsia_runtime::vmar_root_self();
        let kernel_address = memory
            .map_in_vmar(
                &kernel_root_vmar,
                0,
                0,
                length,
                zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
            )
            .map_err(|status| {
                log_error!("failed to map shared binder region in kernel: {:?}", status);
                errno!(ENOMEM)
            })?;
        Ok(Self {
            kernel_address: kernel_address as *mut u8,
            user_address,
            length,
            allocations: Default::default(),
        })
    }

    /// Allocate a buffer of size `length` from this memory block.
    fn allocate(&mut self, length: usize) -> Result<usize, TransactionError> {
        // The current candidate for an allocation.
        let mut candidate = 0;
        for (&ptr, &size) in &self.allocations {
            // If there is enough room at the current candidate location, stop looking.
            if ptr - candidate >= length {
                break;
            }
            // Otherwise, check after the current allocation.
            candidate = ptr + size;
        }
        // At this point, either `candidate` is correct, or the only remaining position is at the
        // end of the buffer. In both case, the allocation succeed if there is enough room between
        // the candidate and the end of the buffer.
        if self.length - candidate < length {
            return Err(TransactionError::Failure);
        }
        self.allocations.insert(candidate, length);
        Ok(candidate)
    }

    /// Allocates three buffers large enough to hold the requested data, offsets, and scatter-gather
    /// buffer lengths, inserting padding between data and offsets as needed. `offsets_length` and
    /// `sg_buffers_length` must be 8-byte aligned.
    ///
    /// NOTE: When `data_length` is zero, a minimum data buffer size of 8 bytes is still allocated.
    /// This is because clients expect their buffer addresses to be uniquely associated with a
    /// transaction. Returning the same address for different transactions will break oneway
    /// transactions that have no payload.
    //
    // This is a temporary implementation of an allocator and should be replaced by something
    // more sophisticated. It currently implements a bump allocator strategy.
    pub fn allocate_buffers(
        &mut self,
        data_length: usize,
        offsets_length: usize,
        sg_buffers_length: usize,
        security_context_buffer_length: usize,
    ) -> Result<SharedMemoryAllocation<'_>, TransactionError> {
        // Round `data_length` up to the nearest multiple of 8, so that the offsets buffer is
        // aligned when we pack it next to the data buffer.
        let data_cap = round_up_to_increment(data_length, std::mem::size_of::<binder_uintptr_t>())?;
        // Ensure that we allocate at least 8 bytes, so that each buffer returned is uniquely
        // associated with a transaction. Otherwise, multiple zero-sized allocations will have the
        // same address and there will be no way of distinguishing which transaction they belong to.
        let data_cap = std::cmp::max(data_cap, std::mem::size_of::<binder_uintptr_t>());
        // Ensure that the offsets and buffers lengths are valid.
        if offsets_length % std::mem::size_of::<binder_uintptr_t>() != 0
            || sg_buffers_length % std::mem::size_of::<binder_uintptr_t>() != 0
            || security_context_buffer_length % std::mem::size_of::<binder_uintptr_t>() != 0
        {
            return Err(TransactionError::Malformed(errno!(EINVAL)));
        }
        let total_length = data_cap
            .checked_add(offsets_length)
            .and_then(|v| v.checked_add(sg_buffers_length))
            .and_then(|v| v.checked_add(security_context_buffer_length))
            .ok_or_else(|| errno!(EINVAL))?;
        let base_offset = self.allocate(total_length)?;
        let security_context_buffer = if security_context_buffer_length > 0 {
            Some(SharedBuffer::new(
                self,
                base_offset + data_cap + offsets_length + sg_buffers_length,
                security_context_buffer_length,
            )?)
        } else {
            None
        };

        Ok(SharedMemoryAllocation {
            data_buffer: SharedBuffer::new(self, base_offset, data_length)?,
            offsets_buffer: SharedBuffer::new(self, base_offset + data_cap, offsets_length)?,
            scatter_gather_buffer: SharedBuffer::new(
                self,
                base_offset + data_cap + offsets_length,
                sg_buffers_length,
            )?,
            security_context_buffer,
        })
    }

    // Reclaim the buffer so that it can be reused.
    pub fn free_buffer(&mut self, buffer: UserAddress) -> Result<(), Errno> {
        // Sanity check that the buffer being freed came from this memory region.
        if buffer < self.user_address || buffer >= (self.user_address + self.length)? {
            return error!(EINVAL);
        }
        let offset = buffer - self.user_address;
        self.allocations.remove(&offset);
        Ok(())
    }
}

/// A buffer of memory allocated from a binder process' [`SharedMemory`].
#[derive(Debug)]
pub struct SharedBuffer<'a, T> {
    pub memory: &'a SharedMemory,
    /// Offset into the shared memory region where the buffer begins.
    pub offset: usize,
    /// The length of the buffer in bytes.
    pub length: usize,
    /// The underlying buffer.
    user_buffer: UserBuffer,
    // A zero-sized type that satisfies the compiler's need for the struct to reference `T`, which
    // is used in `as_mut_bytes` and `as_bytes`.
    _phantom_data: std::marker::PhantomData<T>,
}

impl<'a, T: IntoBytes> SharedBuffer<'a, T> {
    /// Creates a new `SharedBuffer`, which represents a sub-region of `memory` starting at `offset`
    /// bytes, with `length` bytes. Will return EFAULT if the sub-region is not within memory bounds.
    /// The caller is responsible for ensuring it is not aliased.
    fn new(memory: &'a SharedMemory, offset: usize, length: usize) -> Result<Self, Errno> {
        let memory_address = (memory.user_address + offset)?;
        // Validate that the entire buffer length is valid as well.
        let _ = (memory_address + length)?;
        let user_buffer = UserBuffer { address: memory_address, length: length };
        Ok(Self { memory, offset, length, user_buffer, _phantom_data: std::marker::PhantomData })
    }

    /// Returns a mutable slice of the buffer.
    pub fn as_mut_bytes(&mut self) -> &'a mut [T] {
        // SAFETY: `offset + length` was bounds-checked by `new`, and the memory region pointed to
        // was zero-allocated by mapping a new VMO in `allocate_buffers`.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.memory.kernel_address.add(self.offset) as *mut T,
                self.length / std::mem::size_of::<T>(),
            )
        }
    }

    /// Returns an immutable slice of the buffer.
    pub fn as_bytes(&self) -> &'a [T] {
        // SAFETY: `offset + length` was bounds-checked by `new`, and the memory region pointed to
        // was zero-allocated by mapping a new VMO in `allocate_buffers`.
        unsafe {
            std::slice::from_raw_parts(
                self.memory.kernel_address.add(self.offset) as *const T,
                self.length / std::mem::size_of::<T>(),
            )
        }
    }

    /// The userspace address and length of the buffer.
    pub fn user_buffer(&self) -> UserBuffer {
        self.user_buffer
    }
}
