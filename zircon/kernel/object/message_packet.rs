// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Message packets are stored in lists of fixed-size buffers (`BufferChain`) rather than contiguous
//! blocks of memory to reduce heap fragmentation.

use super::buffer_chain::{BufferChain, CONTIGUOUS_SIZE};
use super::handle::HandleOwner;
use crate::object::Handle;
use crate::user_copy::{UserInPtr, UserOutPtr};
use core::ffi::c_void;
use core::mem::{self, MaybeUninit, align_of, size_of};
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::{cmp, slice};
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode, ManagedPtr, MutPtr, PtrTraits};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zr::{static_assert, static_assert_size_and_align};
use zx_status::Status;
use zx_types::{
    ZX_CHANNEL_MAX_MSG_BYTES, ZX_CHANNEL_MAX_MSG_HANDLES, ZX_CHANNEL_MAX_MSG_IOVEC,
    zx_channel_iovec_t, zx_txid_t,
};

// The number of iovecs to read and process at a time. If number of iovecs <= IOVEC_CHUNK_SIZE,
// then the message buf size will be computed and the minimum required number of pages will be
// allocated. Otherwise, a large enough buffer for the largest possible message will be allocated.
const IOVEC_CHUNK_SIZE: usize = 16;

// Handles are stored just after the MessagePacket.
const HANDLES_OFFSET: usize = size_of::<MessagePacket>();

// The MessagePacket object, its handles and zx_txid_t must all fit in the first buffer.
const MIN_CONTIGUOUS_SIZE: usize = HANDLES_OFFSET
    + (ZX_CHANNEL_MAX_MSG_HANDLES as usize * size_of::<*mut c_void>())
    + size_of::<zx_txid_t>();
static_assert!(MIN_CONTIGUOUS_SIZE <= CONTIGUOUS_SIZE);

#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes, IntoBytes, Immutable)]
struct RawChannelIovec {
    buffer: usize,
    capacity: u32,
    reserved: u32,
}

static_assert_size_and_align!(
    RawChannelIovec,
    size_of::<zx_channel_iovec_t>(),
    align_of::<zx_channel_iovec_t>()
);

// MessagePackets have special allocation requirements because they can contain a variable number of
// handles and a variable size payload.
//
// To reduce heap fragmentation, MessagePackets are stored in a lists of fixed size buffers
// (BufferChains) rather than a contiguous blocks of memory. These lists and buffers are allocated
// from the PMM.
//
// The first buffer in a MessagePacket's BufferChain contains the MessagePacket object, followed by
// its handles (if any), and finally its payload data (if any).
#[derive(DoublyLinkedListContainable)]
#[repr(C)]
pub struct MessagePacket {
    #[dll_node]
    node: DoublyLinkedListNode<MessagePacket>,
    buffer_chain: NonNull<BufferChain>,
    data_size: u32,
    num_handles: u16,
    owns_handles: bool,
}

// SAFETY: `MessagePacket` owns its handles array and backing `BufferChain`. It does not use
// thread-local storage or thread-bound resources and can be transferred across threads.
unsafe impl Send for MessagePacket {}

// SAFETY: All concurrent operations on `&MessagePacket` (such as `data_size`, `handles`,
// `start_of_payload`) access immutable or internally synchronized state.
unsafe impl Sync for MessagePacket {}

impl MessagePacket {
    /// Creates a message packet containing the provided data copied from userspace and space for
    /// `num_handles` handles. The handles array is uninitialized and must be completely overwritten
    /// by clients.
    #[inline]
    pub fn create_from_user(
        data: UserInPtr<u8>,
        data_size: usize,
        num_handles: usize,
    ) -> Result<MessagePacketPtr, Status> {
        let mut new_msg = Self::create_common(data_size, num_handles)?;
        if data_size == 0 {
            return Ok(new_msg);
        }
        let dst = new_msg.start_of_payload_uninit_mut();
        if dst.len() == data_size {
            data.copy_slice_from_user(dst)?;
        } else {
            new_msg.buffer_chain_mut().append_user(data, data_size)?;
        }
        Ok(new_msg)
    }

    /// Creates a message packet containing data gathered from userspace iovecs and space for
    /// `num_handles` handles.
    pub fn create_from_iovecs(
        user_iovecs: UserInPtr<zx_channel_iovec_t>,
        mut num_iovecs: usize,
        num_handles: usize,
    ) -> Result<MessagePacketPtr, Status> {
        if num_iovecs > ZX_CHANNEL_MAX_MSG_IOVEC as usize {
            return Err(Status::OUT_OF_RANGE);
        }

        let mut user_iovecs = user_iovecs.reinterpret::<RawChannelIovec>();
        let mut iovecs = [MaybeUninit::<RawChannelIovec>::uninit(); IOVEC_CHUNK_SIZE];

        if num_iovecs <= IOVEC_CHUNK_SIZE {
            let iovecs = if num_iovecs > 0 {
                user_iovecs.copy_slice_from_user(&mut iovecs[..num_iovecs])?
            } else {
                &mut []
            };
            let mut message_size = 0;
            for iovec in &*iovecs {
                if iovec.reserved != 0 {
                    return Err(Status::INVALID_ARGS);
                }
                message_size += iovec.capacity as usize;
            }
            let mut msg = Self::create_common(message_size, num_handles)?;
            let mut dst = msg.start_of_payload_uninit_mut();
            if dst.len() == message_size {
                for iovec in &*iovecs {
                    let len = iovec.capacity as usize;
                    if len > 0 {
                        let (chunk_dst, rest) = dst.split_at_mut(len);
                        dst = rest;
                        let src = UserInPtr::new(ptr::with_exposed_provenance(iovec.buffer));
                        src.copy_slice_from_user(chunk_dst)?;
                    }
                }
            } else {
                for iovec in &*iovecs {
                    let src = UserInPtr::new(ptr::with_exposed_provenance(iovec.buffer));
                    msg.buffer_chain_mut().append_user(src, iovec.capacity as usize)?;
                }
            }
            return Ok(msg);
        }

        let mut msg = Self::create_common(ZX_CHANNEL_MAX_MSG_BYTES as usize, num_handles)?;
        let mut message_size = 0;
        while num_iovecs > 0 {
            let chunk = cmp::min(num_iovecs, IOVEC_CHUNK_SIZE);
            let chunk_slice = user_iovecs.copy_slice_from_user(&mut iovecs[..chunk])?;
            for iovec in chunk_slice {
                if iovec.reserved != 0 {
                    return Err(Status::INVALID_ARGS);
                }
                message_size += iovec.capacity as usize;
                let src = UserInPtr::new(ptr::with_exposed_provenance(iovec.buffer));
                msg.buffer_chain_mut().append_user(src, iovec.capacity as usize)?;
            }
            num_iovecs -= chunk;
            user_iovecs = user_iovecs.element_offset(chunk);
        }

        msg.buffer_chain_mut().free_unused_buffers();
        msg.data_size = message_size as u32;
        Ok(msg)
    }

    /// Creates a message packet containing the provided data copied from kernel space and space
    /// for `num_handles` handles.
    pub fn create_from_kernel(data: &[u8], num_handles: usize) -> Result<MessagePacketPtr, Status> {
        let mut new_msg = Self::create_common(data.len(), num_handles)?;
        let dst = new_msg.start_of_payload_uninit_mut();
        if dst.len() == data.len() {
            // SAFETY: `dst` and `data` have equal length (`dst.len() == data.len()`) and do not
            // overlap (`dst` is inside the freshly allocated `BufferChain`).
            unsafe {
                ptr::copy_nonoverlapping(data.as_ptr(), dst.as_mut_ptr().cast::<u8>(), data.len());
            }
        } else {
            new_msg.buffer_chain_mut().append_kernel(data)?;
        }
        Ok(new_msg)
    }

    /// Returns payload data size in bytes.
    #[inline]
    pub fn data_size(&self) -> usize {
        self.data_size as usize
    }

    /// Copies the packet's `data_size` bytes to `buf`.
    ///
    /// Returns an error if `buf` points to a bad user address.
    #[inline]
    pub fn copy_data_to(&self, buf: UserOutPtr<u8>) -> Result<(), Status> {
        let data_size = self.data_size as usize;
        if data_size == 0 {
            return Ok(());
        }
        let first_chunk = self.start_of_payload();
        if first_chunk.len() == data_size {
            return buf.copy_slice_to_user(first_chunk);
        }
        // SAFETY: `self.buffer_chain` is non-null and points to the BufferChain inside the first
        // buffer.
        unsafe { self.buffer_chain.as_ref() }.copy_out(buf, self.payload_offset(), data_size)
    }

    /// Returns the number of handles attached to this message packet.
    #[inline]
    pub fn num_handles(&self) -> usize {
        self.num_handles as usize
    }

    /// Returns the transaction ID stored in the payload header (`zx_channel_call` treats the
    /// leading bytes of the payload as a transaction ID of type `zx_txid_t`).
    #[inline]
    pub fn get_txid(&self) -> zx_txid_t {
        if (self.data_size as usize) < size_of::<zx_txid_t>() {
            return 0;
        }
        // The first few bytes of the payload are a zx_txid_t.
        // SAFETY: `MIN_CONTIGUOUS_SIZE <= CONTIGUOUS_SIZE` guarantees that the `zx_txid_t` at
        // `payload_offset()` fits within the first buffer, and `payload_offset()` is
        // pointer-aligned (`>= align_of::<zx_txid_t>()`).
        unsafe {
            *ptr::from_ref(self)
                .cast::<u8>()
                .wrapping_add(self.payload_offset())
                .cast::<zx_txid_t>()
        }
    }

    /// Sets the transaction ID in the payload header.
    #[inline]
    pub fn set_txid(&mut self, txid: zx_txid_t) {
        if (self.data_size as usize) >= size_of::<zx_txid_t>() {
            // SAFETY: `MIN_CONTIGUOUS_SIZE <= CONTIGUOUS_SIZE` guarantees that the `zx_txid_t` at
            // `payload_offset()` fits within the first buffer, and `payload_offset()` is
            // pointer-aligned (`>= align_of::<zx_txid_t>()`).
            unsafe {
                *ptr::from_mut(self)
                    .cast::<u8>()
                    .wrapping_add(self.payload_offset())
                    .cast::<zx_txid_t>() = txid;
            }
        }
    }

    // A private destructor helps to make sure that only our custom deleter is ever used to destroy
    // this object which, in turn, makes it very difficult to not properly recycle the object.
    /// Recycles a `MessagePacket` by running its destructor in place and freeing its backing
    /// `BufferChain`.
    ///
    /// # Safety
    ///
    /// - `packet` must be a valid, uniquely owned pointer to a `MessagePacket` allocated inside
    ///   its backing `BufferChain`.
    /// - The packet must not be currently linked in any intrusive container
    ///   (`!packet.in_container()`).
    /// - If `packet.owns_handles` is true, every non-null entry in `packet.handles` must be a
    ///   valid, owned `Handle*`.
    #[inline]
    pub unsafe fn recycle(packet: *mut MessagePacket) {
        // SAFETY:
        // - `packet` is a valid, uniquely owned, aligned pointer to an allocated `MessagePacket`
        //   that is not linked in any container.
        // - We read `buffer_chain` before dropping `packet` in place so the backing memory can be
        //   freed.
        // - `chain` is a valid pointer originally returned from `BufferChain::alloc`.
        unsafe {
            // Grab the buffer chain for this packet before dropping.
            let chain = (*packet).buffer_chain;

            // Manually destruct the packet. Do not delete it; its memory did not come from new, it
            // is contained as part of the buffer chain.
            ptr::drop_in_place(packet);

            // Now return the buffer chain to where it came from.
            BufferChain::free(chain);
        }
    }

    // A private constructor ensures that users must use the static factory create methods to
    // create a MessagePacket. This, in turn, guarantees that when a user creates a MessagePacket,
    // they end up with the proper MessagePacketPtr type for managing the message packet's life
    // cycle.
    //
    // Creates a MessagePacket sufficient to hold `data_size` bytes and `num_handles`.
    //
    // Note: This method does not write the payload into the MessagePacket.
    //
    // Returns Ok(MessagePacketPtr) on success.
    #[inline]
    fn create_common(data_size: usize, num_handles: usize) -> Result<MessagePacketPtr, Status> {
        if data_size > ZX_CHANNEL_MAX_MSG_BYTES as usize
            || num_handles > ZX_CHANNEL_MAX_MSG_HANDLES as usize
        {
            return Err(Status::OUT_OF_RANGE);
        }

        // The payload comes after the handles.
        let payload_offset = Self::calc_payload_offset(num_handles);

        // MessagePackets lives *inside* a list of buffers. The first buffer holds the
        // MessagePacket object, followed by its handles (if any), and finally the payload data.
        let mut chain = BufferChain::alloc(payload_offset + data_size)?;
        // SAFETY: `chain` is valid and pinned inside the first buffer.
        let mut chain_pin = unsafe { Pin::new_unchecked(chain.as_mut()) };
        debug_assert!(!chain_pin.is_empty());
        chain_pin.as_mut().skip(payload_offset);

        // Construct the MessagePacket into the first buffer immediately after `BufferChain`.
        // SAFETY: `BufferChain` occupies the first `BUFFER_CHAIN_SIZE` bytes of the first buffer's
        // `raw_data`, so `chain.as_ptr().add(1)` points directly to `first_buffer_data_mut()`,
        // which is 8-byte aligned and has `CONTIGUOUS_SIZE` usable bytes.
        let packet = unsafe { chain.as_ptr().add(1).cast::<MessagePacket>() };
        debug_assert_eq!(packet.cast::<u8>(), chain_pin.first_buffer_data_mut());

        // The MessagePacket now owns the BufferChain and msg owns the MessagePacket.
        // SAFETY: `packet` points into the first buffer and is properly aligned and sized.
        unsafe {
            packet.write(MessagePacket {
                node: DoublyLinkedListNode::new(),
                buffer_chain: chain,
                data_size: data_size as u32,
                num_handles: num_handles as u16,
                owns_handles: false,
            });
            Ok(MessagePacketPtr::from_raw(packet))
        }
    }

    #[inline]
    const fn calc_payload_offset(num_handles: usize) -> usize {
        HANDLES_OFFSET + num_handles * size_of::<*mut c_void>()
    }

    #[inline]
    fn payload_offset(&self) -> usize {
        Self::calc_payload_offset(self.num_handles as usize)
    }

    #[inline]
    fn buffer_chain_mut(&mut self) -> Pin<&mut BufferChain> {
        // SAFETY: `self.buffer_chain` is uniquely owned and points to the pinned BufferChain
        // inside the first buffer.
        unsafe { Pin::new_unchecked(self.buffer_chain.as_mut()) }
    }

    #[inline]
    fn contiguous_payload_size(&self) -> usize {
        // The first Buffer of a BufferChain will contain the handles (if any are present) and at
        // least some of the message's payload. How much of message payload? Up to CONTIGUOUS_SIZE
        // minus the payload's offset.
        cmp::min(CONTIGUOUS_SIZE - self.payload_offset(), self.data_size as usize)
    }

    /// Returns a const pointer to the array of handle pointers attached to this message packet.
    #[inline]
    pub fn handles(&self) -> *const *mut c_void {
        // Handles are stored immediately after `MessagePacket` at `HANDLES_OFFSET` inside the
        // first buffer's contiguous data region.
        ptr::from_ref(self).cast::<u8>().wrapping_add(HANDLES_OFFSET).cast()
    }

    /// Returns a mutable pointer to the array of handle pointers attached to this message packet.
    #[inline]
    pub fn handles_mut(&mut self) -> *mut *mut c_void {
        // Handles are stored immediately after `MessagePacket` at `HANDLES_OFFSET` inside the
        // first buffer's contiguous data region.
        ptr::from_mut(self).cast::<u8>().wrapping_add(HANDLES_OFFSET).cast()
    }

    /// Sets whether this packet owns its attached handles and should delete them on recycle.
    #[inline]
    pub fn set_owns_handles(&mut self, owns_handles: bool) {
        self.owns_handles = owns_handles;
    }

    /// Returns a slice referencing the first chunk of payload stored contiguously in the first
    /// buffer backing the message packet.
    #[inline]
    pub fn start_of_payload(&self) -> &[u8] {
        // The first chunk of payload. Eventually we'd want to actually get the whole message out.
        // SAFETY: `self` is at the start of the first buffer's data, which contains at least
        // `payload_offset() + contiguous_payload_size()` contiguous bytes.
        unsafe {
            let ptr = ptr::from_ref(self).cast::<u8>().wrapping_add(self.payload_offset());
            slice::from_raw_parts(ptr, self.contiguous_payload_size())
        }
    }

    #[inline]
    fn start_of_payload_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        // SAFETY: `self` is at the start of the first buffer's data, which contains at least
        // `payload_offset() + contiguous_payload_size()` contiguous bytes.
        unsafe {
            let ptr =
                ptr::from_mut(self).cast::<MaybeUninit<u8>>().wrapping_add(self.payload_offset());
            slice::from_raw_parts_mut(ptr, self.contiguous_payload_size())
        }
    }

    #[cold]
    fn drop_handles(&mut self) {
        // SAFETY: `handles_mut()` points to `num_handles` handle pointers owned by `self`.
        let handles = unsafe { slice::from_raw_parts_mut(self.handles_mut(), self.num_handles()) };
        for &mut raw in handles {
            // Delete the handle via HandleOwner dtor.
            // SAFETY: Each entry is a valid handle pointer (or null) owned by `self`.
            unsafe {
                drop(HandleOwner::from_raw(raw.cast::<Handle>()));
            }
        }
    }
}

// A private destructor helps to make sure that only our custom deleter is ever used to destroy this
// object which, in turn, makes it very difficult to not properly recycle the object.
impl Drop for MessagePacket {
    #[inline]
    fn drop(&mut self) {
        if self.owns_handles {
            self.drop_handles();
        }
    }
}

// Definition of a MessagePacket's specific pointer type. Message packets must be managed using this
// specific type of pointer, because MessagePackets have a specific custom deletion requirement.
pub struct MessagePacketPtr {
    ptr: NonNull<MessagePacket>,
}

// SAFETY: `MessagePacket` is safe to send between threads via `MessagePacketPtr`.
unsafe impl Send for MessagePacketPtr {}
unsafe impl Sync for MessagePacketPtr {}

impl MessagePacketPtr {
    /// Constructs a `MessagePacketPtr` from a non-null raw pointer.
    ///
    /// # Safety
    ///
    /// `raw` must be a valid, uniquely owned `MessagePacket*`.
    #[inline]
    pub unsafe fn from_raw(raw: *mut MessagePacket) -> Self {
        debug_assert!(!raw.is_null());
        // SAFETY: `raw` is non-null.
        Self { ptr: unsafe { NonNull::new_unchecked(raw) } }
    }

    /// Consumes the pointer and returns the raw pointer without running destructor.
    #[inline]
    pub fn into_raw(self) -> *mut MessagePacket {
        let ptr = self.ptr.as_ptr();
        mem::forget(self);
        ptr
    }
}

impl Deref for MessagePacketPtr {
    type Target = MessagePacket;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.ptr` is non-null and points to an initialized MessagePacket.
        unsafe { self.ptr.as_ref() }
    }
}

impl DerefMut for MessagePacketPtr {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `self.ptr` is non-null and valid.
        unsafe { self.ptr.as_mut() }
    }
}

unsafe impl PtrTraits for MessagePacketPtr {
    type Target = MessagePacket;
    const IS_MANAGED: bool = true;

    #[inline]
    fn into_raw(self) -> *mut MessagePacket {
        Self::into_raw(self)
    }

    #[inline]
    unsafe fn from_raw(raw: *mut MessagePacket) -> Self {
        // SAFETY: Caller guarantees `raw` is a valid MessagePacket pointer.
        unsafe { Self::from_raw(raw) }
    }

    #[inline]
    fn get_ref(&self) -> &MessagePacket {
        self
    }
}

unsafe impl ManagedPtr for MessagePacketPtr {}

// SAFETY: `MessagePacketPtr` uniquely owns its `MessagePacket`, so exclusive access to a container
// holding it implies exclusive access to the packet.
unsafe impl MutPtr for MessagePacketPtr {}

impl Drop for MessagePacketPtr {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a valid, uniquely owned MessagePacket pointer.
        unsafe {
            MessagePacket::recycle(self.ptr.as_ptr());
        }
    }
}

/// In-tree kernel unit tests for `MessagePacket`.
#[cfg(ktest)]
#[unittest::suite(name = "message_packet_rust")]
mod tests {
    use super::{
        CONTIGUOUS_SIZE, IOVEC_CHUNK_SIZE, MessagePacket, MessagePacketPtr, RawChannelIovec,
    };
    use crate::user_copy::{UserInPtr, UserOutPtr};
    use crate::user_memory::UserMemory;
    use core::mem::{MaybeUninit, size_of};
    use core::{array, cmp, ptr, slice};
    use fbl::DoublyLinkedList;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true, unwrap_ok};
    use zerocopy::IntoBytes;
    use zx_status::Status;
    use zx_types::{ZX_CHANNEL_MAX_MSG_BYTES, ZX_CHANNEL_MAX_MSG_HANDLES, zx_channel_iovec_t};

    fn create_user_memory(size: usize) -> Result<UserMemory, Status> {
        let mem = UserMemory::create(size).ok_or(Status::NO_MEMORY)?;
        mem.commit_and_map(0..size)?;
        Ok(mem)
    }

    macro_rules! create_user_memory {
        ($size:expr) => {
            unwrap_ok!(create_user_memory($size))
        };
    }

    fn fill_user_memory(mem: &UserMemory, byte: u8, offset: usize, size: usize) -> bool {
        let chunk = [byte; 256];
        let mut curr = offset;
        let end = offset + size;
        while curr < end {
            let to_write = cmp::min(chunk.len(), end - curr);
            if mem.vmo_write(&chunk[..to_write], curr as u64).is_err() {
                return false;
            }
            curr += to_write;
        }
        true
    }

    fn verify_user_memory_byte(mem: &UserMemory, offset: usize, size: usize, expected: u8) -> bool {
        let mut chunk = [MaybeUninit::<u8>::uninit(); 256];
        let mut curr = offset;
        let end = offset + size;
        while curr < end {
            let to_read = cmp::min(chunk.len(), end - curr);
            let Ok(read_bytes) = mem.vmo_read(&mut chunk[..to_read], curr as u64) else {
                return false;
            };
            for &mut b in read_bytes {
                if b != expected {
                    return false;
                }
            }
            curr += to_read;
        }
        true
    }

    /// Tests creating a MessagePacket from kernel data, matching C++ create_void_star.
    #[test]
    fn test_create_from_kernel() {
        const SIZE: usize = 32;
        let data = [b'B'; SIZE];
        let mut packet =
            unwrap_ok!(MessagePacket::create_from_kernel(&data, 0), "failed to create packet");
        expect_eq!(packet.data_size(), data.len());
        expect_eq!(packet.num_handles(), 0);
        expect_true!(packet.get_txid() != 0);

        let mem_out = create_user_memory!(SIZE);
        let user_out = UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(mem_out.base()));
        expect_ok!(packet.copy_data_to(user_out));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); SIZE];
        let read_bytes = unwrap_ok!(mem_out.vmo_read(&mut read_buf, 0));
        expect_true!(read_bytes == data);

        packet.set_txid(999);
        expect_eq!(packet.get_txid(), 999);
    }

    /// Tests creating a zero-length packet from user memory, matching C++ create_zero.
    #[test]
    fn test_create_zero() {
        let mem = create_user_memory!(1);
        let user_in = UserInPtr::new(ptr::with_exposed_provenance::<u8>(mem.base()));
        let user_out = UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(mem.base()));

        let packet = unwrap_ok!(MessagePacket::create_from_user(user_in, 0, 0));
        expect_eq!(packet.data_size(), 0);
        expect_eq!(packet.num_handles(), 0);
        expect_eq!(packet.get_txid(), 0);

        expect_ok!(packet.copy_data_to(user_out));
    }

    /// Tests creating a MessagePacket with zero size payload.
    #[test]
    fn test_create_from_kernel_zero_size() {
        let packet =
            unwrap_ok!(MessagePacket::create_from_kernel(&[], 0), "failed to create packet");
        expect_eq!(packet.data_size(), 0);
        expect_eq!(packet.num_handles(), 0);
        expect_eq!(packet.get_txid(), 0);
    }

    /// Tests that creating a MessagePacket with too many handles fails with OUT_OF_RANGE.
    #[test]
    fn test_create_too_many_handles() {
        let mem = create_user_memory!(1);
        let user_in = UserInPtr::new(ptr::with_exposed_provenance::<u8>(mem.base()));
        let res_user =
            MessagePacket::create_from_user(user_in, 1, ZX_CHANNEL_MAX_MSG_HANDLES as usize + 1);
        expect_true!(matches!(res_user, Err(Status::OUT_OF_RANGE)));

        let res_kernel =
            MessagePacket::create_from_kernel(&[], ZX_CHANNEL_MAX_MSG_HANDLES as usize + 1);
        expect_true!(matches!(res_kernel, Err(Status::OUT_OF_RANGE)));
    }

    /// Tests set_owns_handles(true) and set_owns_handles(false).
    #[test]
    fn test_set_owns_handles() {
        let mut packet = unwrap_ok!(MessagePacket::create_from_kernel(&[], 2));
        expect_eq!(packet.handles(), packet.handles_mut().cast_const());
        // Initialize handles to null pointers so recycle doesn't attempt to drop invalid pointers.
        unsafe {
            slice::from_raw_parts_mut(packet.handles_mut(), 2).fill(ptr::null_mut());
        }
        packet.set_owns_handles(true);
        packet.set_owns_handles(false);

        let mut packet2 = unwrap_ok!(MessagePacket::create_from_kernel(&[], 2));
        unsafe {
            slice::from_raw_parts_mut(packet2.handles_mut(), 2).fill(ptr::null_mut());
        }
        packet2.set_owns_handles(true);
    }

    /// Tests creating a MessagePacket from user memory and copying data out.
    #[test]
    fn test_create_from_user_and_copy_out() {
        let size = 128;
        let mem_in = create_user_memory!(size);
        let payload: [u8; 128] = array::from_fn(|i| (i as u8).wrapping_mul(3));
        unwrap_ok!(mem_in.vmo_write(&payload, 0), "failed to write payload");

        let user_in = UserInPtr::new(ptr::with_exposed_provenance::<u8>(mem_in.base()));
        let packet = unwrap_ok!(
            MessagePacket::create_from_user(user_in, size, 0),
            "failed to create packet"
        );
        expect_eq!(packet.data_size(), size);
        expect_eq!(packet.num_handles(), 0);

        let mem_out = create_user_memory!(size);
        let user_out = UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(mem_out.base()));
        expect_ok!(packet.copy_data_to(user_out));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 128];
        let read_bytes =
            unwrap_ok!(mem_out.vmo_read(&mut read_buf, 0), "failed to read back payload");
        expect_true!(read_bytes == payload);
    }

    /// Tests creating a large MessagePacket from user memory matching C++ create().
    #[test]
    fn test_create_large_user_message() {
        const SIZE: usize = 62234;
        const NUM_HANDLES: usize = 64;

        let mem_in = create_user_memory!(SIZE);
        expect_true!(fill_user_memory(&mem_in, b'A', 0, SIZE));

        let user_in = UserInPtr::new(ptr::with_exposed_provenance::<u8>(mem_in.base()));
        let packet = unwrap_ok!(MessagePacket::create_from_user(user_in, SIZE, NUM_HANDLES));
        expect_eq!(packet.data_size(), SIZE);
        expect_eq!(packet.num_handles(), NUM_HANDLES);
        expect_true!(packet.get_txid() != 0);

        let mem_out = create_user_memory!(SIZE);
        let user_out = UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(mem_out.base()));
        expect_ok!(packet.copy_data_to(user_out));

        expect_true!(verify_user_memory_byte(&mem_out, 0, SIZE, b'A'));
    }

    /// Tests creating a MessagePacket from iovecs and copying data out.
    #[test]
    fn test_create_from_iovecs() {
        let chunk1 = b"chunk one ";
        let chunk2 = b"chunk two";
        let total_size = chunk1.len() + chunk2.len();

        let mem = create_user_memory!(4096);

        let offset_iovecs = 0;
        let offset_chunk1 = 128;
        let offset_chunk2 = 256;

        unwrap_ok!(mem.vmo_write(chunk1, offset_chunk1), "failed to write chunk1");
        unwrap_ok!(mem.vmo_write(chunk2, offset_chunk2), "failed to write chunk2");

        let base = mem.base();
        let iovecs = [
            RawChannelIovec {
                buffer: base + offset_chunk1 as usize,
                capacity: chunk1.len() as u32,
                reserved: 0,
            },
            RawChannelIovec {
                buffer: base + offset_chunk2 as usize,
                capacity: chunk2.len() as u32,
                reserved: 0,
            },
        ];
        unwrap_ok!(mem.vmo_write(iovecs.as_bytes(), offset_iovecs), "failed to write iovecs");

        let user_iovecs = UserInPtr::new(ptr::with_exposed_provenance::<zx_channel_iovec_t>(
            base + offset_iovecs as usize,
        ));
        let packet = unwrap_ok!(
            MessagePacket::create_from_iovecs(user_iovecs, 2, 0),
            "failed to create packet from iovecs"
        );
        expect_eq!(packet.data_size(), total_size);

        let mem_out = create_user_memory!(total_size);
        let user_out = UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(mem_out.base()));
        expect_ok!(packet.copy_data_to(user_out));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 19];
        let read_bytes =
            unwrap_ok!(mem_out.vmo_read(&mut read_buf, 0), "failed to read back payload");
        expect_true!(&read_bytes[..10] == chunk1);
        expect_true!(&read_bytes[10..] == chunk2);
    }

    macro_rules! check_create_iovec {
        ($num_iovecs:expr, $num_handles:expr) => {{
            let n_iovecs = $num_iovecs;
            let n_handles = $num_handles;
            let num_bytes: usize = n_iovecs * (n_iovecs - 1) / 2;
            let bytes_mem = create_user_memory!(num_bytes);

            // Populate bytes with incrementing values: bytes[i] = i as u8.
            let mut chunk = [0u8; 256];
            let mut written = 0;
            while written < num_bytes {
                let to_write = cmp::min(chunk.len(), num_bytes - written);
                for j in 0..to_write {
                    chunk[j] = (written + j) as u8;
                }
                unwrap_ok!(bytes_mem.vmo_write(&chunk[..to_write], written as u64));
                written += to_write;
            }

            // Create iovecs where iovec[i] has capacity i.
            let iovec_mem_size = n_iovecs * size_of::<zx_channel_iovec_t>();
            let iovec_mem = create_user_memory!(iovec_mem_size);

            let mut byte_offset: usize = 0;
            for i in 0..n_iovecs {
                let iovec = RawChannelIovec {
                    buffer: bytes_mem.base() + byte_offset,
                    capacity: i as u32,
                    reserved: 0,
                };
                byte_offset += i;
                unwrap_ok!(
                    iovec_mem
                        .vmo_write(iovec.as_bytes(), (i * size_of::<zx_channel_iovec_t>()) as u64,)
                );
            }

            let user_iovecs = UserInPtr::new(ptr::with_exposed_provenance::<zx_channel_iovec_t>(
                iovec_mem.base(),
            ));
            let packet =
                unwrap_ok!(MessagePacket::create_from_iovecs(user_iovecs, n_iovecs, n_handles));

            expect_eq!(packet.num_handles(), n_handles);
            expect_eq!(packet.data_size(), num_bytes);

            let result_mem = create_user_memory!(num_bytes);
            let user_out =
                UserOutPtr::new(ptr::with_exposed_provenance_mut::<u8>(result_mem.base()));
            expect_ok!(packet.copy_data_to(user_out));

            // Verify result matches expected bytes.
            let mut read_buf = [MaybeUninit::<u8>::uninit(); 256];
            let mut verified = 0;
            while verified < num_bytes {
                let to_verify = cmp::min(read_buf.len(), num_bytes - verified);
                let read_bytes =
                    unwrap_ok!(result_mem.vmo_read(&mut read_buf[..to_verify], verified as u64));
                for (j, &b) in read_bytes.iter().enumerate() {
                    expect_eq!(b, (verified + j) as u8);
                }
                verified += to_verify;
            }
        }};
    }

    /// Tests bounded iovecs (16 iovecs) with 0 handles, matching C++ create_iovec_bounded.
    #[test]
    fn test_create_iovec_bounded() {
        check_create_iovec!(IOVEC_CHUNK_SIZE, 0);
    }

    /// Tests bounded iovecs (16 iovecs) with 3 handles, matching C++ create_iovec_bounded_handles.
    #[test]
    fn test_create_iovec_bounded_handles() {
        check_create_iovec!(IOVEC_CHUNK_SIZE, 3);
    }

    /// Tests unbounded iovecs (32 iovecs) with 0 handles, matching C++ create_iovec_unbounded.
    #[test]
    fn test_create_iovec_unbounded() {
        check_create_iovec!(2 * IOVEC_CHUNK_SIZE, 0);
    }

    /// Tests unbounded iovecs (32 iovecs, 3 handles) matching C++ create_iovec_unbounded_handles.
    #[test]
    fn test_create_iovec_unbounded_handles() {
        check_create_iovec!(2 * IOVEC_CHUNK_SIZE, 3);
    }

    /// Tests that passing an iovec with non-zero reserved returns Status::INVALID_ARGS.
    #[test]
    fn test_iovec_non_zero_reserved() {
        // Test bounded path (1 iovec).
        let mem = create_user_memory!(4096);
        let base = mem.base();

        let iovec = RawChannelIovec { buffer: base + 1024, capacity: 16, reserved: 1 };
        unwrap_ok!(mem.vmo_write(iovec.as_bytes(), 0));

        let user_iovecs = UserInPtr::new(ptr::with_exposed_provenance::<zx_channel_iovec_t>(base));
        let res_bounded = MessagePacket::create_from_iovecs(user_iovecs, 1, 0);
        expect_true!(matches!(res_bounded, Err(Status::INVALID_ARGS)));

        // Test unbounded path (> IOVEC_CHUNK_SIZE iovecs).
        let num_iovecs = IOVEC_CHUNK_SIZE + 1;
        let mem_unbounded = create_user_memory!(num_iovecs * size_of::<zx_channel_iovec_t>());
        unwrap_ok!(mem_unbounded.vmo_write(iovec.as_bytes(), 0));
        let user_iovecs_unbounded = UserInPtr::new(ptr::with_exposed_provenance::<
            zx_channel_iovec_t,
        >(mem_unbounded.base()));
        let res_unbounded = MessagePacket::create_from_iovecs(user_iovecs_unbounded, num_iovecs, 0);
        expect_true!(matches!(res_unbounded, Err(Status::INVALID_ARGS)));
    }

    /// Tests bad user memory handling, verifying both null and kernel addresses are rejected.
    #[test]
    fn test_bad_user_memory() {
        let null_in = UserInPtr::<u8>::new(ptr::null());
        let res = MessagePacket::create_from_user(null_in, 64, 0);
        expect_true!(matches!(res, Err(Status::INVALID_ARGS)));

        let kernel_buf = [0u8; 64];
        let kernel_in = UserInPtr::<u8>::new(kernel_buf.as_ptr());
        let res = MessagePacket::create_from_user(kernel_in, 64, 0);
        expect_true!(matches!(res, Err(Status::INVALID_ARGS)));

        let packet = unwrap_ok!(MessagePacket::create_from_kernel(b"test", 0));
        let null_out = UserOutPtr::<u8>::new(ptr::null_mut());
        let res = packet.copy_data_to(null_out);
        expect_true!(matches!(res, Err(Status::INVALID_ARGS)));

        let mut kernel_out_buf = [0u8; 4];
        let kernel_out = UserOutPtr::<u8>::new(kernel_out_buf.as_mut_ptr());
        let res = packet.copy_data_to(kernel_out);
        expect_true!(matches!(res, Err(Status::INVALID_ARGS)));
    }

    /// Verifies that start_of_payload returns a contiguous prefix for a maximum sized packet.
    #[test]
    fn test_start_of_payload_max_message() {
        let max_msg_bytes = ZX_CHANNEL_MAX_MSG_BYTES as usize;
        let mem = create_user_memory!(max_msg_bytes);
        expect_true!(fill_user_memory(&mem, b'A', 0, max_msg_bytes));

        let user_in = UserInPtr::new(ptr::with_exposed_provenance::<u8>(mem.base()));
        let packet = unwrap_ok!(MessagePacket::create_from_user(user_in, max_msg_bytes, 0));
        expect_eq!(packet.data_size(), max_msg_bytes);

        let expected_len = CONTIGUOUS_SIZE - packet.payload_offset();
        let start = packet.start_of_payload();
        expect_eq!(start.len(), expected_len);
        for &b in start {
            expect_eq!(b, b'A');
        }
    }

    /// Tests start_of_payload.
    #[test]
    fn test_start_of_payload() {
        let payload = [0x55u8; 64];
        let packet = unwrap_ok!(MessagePacket::create_from_kernel(&payload, 0));
        let start = packet.start_of_payload();
        expect_eq!(start.len(), 64);
        expect_true!(start == payload);
    }

    /// Tests MessagePacket with DoublyLinkedList and in_container check.
    #[test]
    fn test_message_packet_doubly_linked_list() {
        let packet1 = unwrap_ok!(MessagePacket::create_from_kernel(b"first", 0));
        let packet2 = unwrap_ok!(MessagePacket::create_from_kernel(b"second", 0));

        expect_false!(packet1.node.in_container());
        expect_false!(packet2.node.in_container());

        stack_pin_init!(let list = DoublyLinkedList::<MessagePacketPtr>::new());
        expect_true!(list.is_empty());

        list.as_mut().push_back(packet1);
        list.as_mut().push_back(packet2);
        expect_false!(list.is_empty());

        let popped1 = list.as_mut().pop_front().expect("expected packet1");
        expect_eq!(popped1.data_size(), 5);
        expect_false!(popped1.node.in_container());

        let popped2 = list.as_mut().pop_front().expect("expected packet2");
        expect_eq!(popped2.data_size(), 6);
        expect_false!(popped2.node.in_container());

        expect_true!(list.is_empty());
    }
}
