// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::user_copy::{UserInPtr, UserOutPtr};
use crate::vm::page::VmPagePtr;
use crate::vm::page_state::VmPageState;
use crate::vm::{physmap, pmm};
use core::cmp::min;
use core::convert::Infallible;
use core::ffi::c_char;
use core::mem::{ManuallyDrop, MaybeUninit, align_of, size_of};
use core::pin::Pin;
use core::ptr::drop_in_place;
use core::slice::{from_raw_parts, from_raw_parts_mut};
use counters_rs::define_kcounter;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode};
use page;
use page_bindings::vm_page_state;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use zr::static_assert;
use zx_status::Status;

// Total amount of memory occupied by MBuf objects.
define_kcounter!(MBUF_TOTAL_BYTES_COUNT, "mbuf.total_bytes", Sum);

/// An MBuf is a small fixed-size chainable memory buffer.
#[repr(C)]
#[derive(DoublyLinkedListContainable)]
struct MBuf {
    #[dll_node]
    node: ManuallyDrop<DoublyLinkedListNode<MBuf>>,

    /// Length of the valid `data` in this buffer. Writes can append more to `data` and increment
    /// this length.
    len: u32,

    /// `pkt_len` is set to the total number of bytes in a packet when a socket is in
    /// `ZX_SOCKET_DATAGRAM` mode. A `pkt_len` of 0 means this `MBuf` is part of the body of a
    /// packet.
    ///
    /// Always 0 in `ZX_SOCKET_STREAM` mode.
    pkt_len: u32,

    /// Back-pointer to the `vm_page_t` this `MBuf` was allocated from. Recording this is just an
    /// optimization as it should always be the case that:
    /// `Pmm::Node().PaddrToPage(physmap_to_paddr(this)) == page_`
    page: VmPagePtr,

    /// The data field is left uninitialized as the caller is going to immediately overwrite with
    /// the payload, and is trusted to not access any uninitialized portions.
    /// TODO: maybe union data with char* blocks for large messages
    data: MaybeUninit<[u8; MBuf::PAYLOAD_SIZE]>,
}

impl MBuf {
    // 16 for the linked list 16 for the explicit fields.
    pub const HEADER_SIZE: usize = 32;
    pub const PAYLOAD_SIZE: usize = page::SIZE - Self::HEADER_SIZE;

    /// Calculate the number of MBuf objects needed to store a payload of the given size.
    pub const fn num_buffers_for_payload(payload: usize) -> usize {
        payload.div_ceil(Self::PAYLOAD_SIZE)
    }

    /// Allocates and initializes a single `MBuf` page from PMM.
    pub fn new() -> Result<*mut MBuf, Status> {
        let (page, paddr) = pmm::alloc_page(0)?;
        MBUF_TOTAL_BYTES_COUNT.add(size_of::<MBuf>() as i64);

        // SAFETY: `page` was just allocated from `pmm::alloc_page` and is mapped in the physmap.
        // `buf_ptr.write(...)` initializes the memory without dropping previous contents.
        let buf_ptr = physmap::paddr_to_physmap(paddr).0 as *mut MBuf;
        unsafe {
            page.set_state(VmPageState(vm_page_state::IPC));
            buf_ptr.write(MBuf {
                node: ManuallyDrop::new(DoublyLinkedListNode::new()),
                len: 0,
                pkt_len: 0,
                page,
                data: MaybeUninit::uninit(),
            });
        }

        Ok(buf_ptr)
    }

    /// Returns number of bytes of free space in this MBuf.
    pub fn available_space(&self) -> usize {
        Self::PAYLOAD_SIZE - (self.len as usize)
    }

    /// Returns a slice of valid initialized data starting at `offset` up to `self.len`,
    /// typed as `c_char` for user memory copy operations.
    pub fn read(&self, offset: usize) -> &[c_char] {
        let len = self.len as usize;
        if offset >= len {
            return &[];
        }
        let valid_len = len - offset;
        // SAFETY: `self.data` has length `PAYLOAD_SIZE >= len >= offset + valid_len`.
        // The memory from `0..len` has been initialized by prior writes.
        // `c_char` and `u8` have identical size (1 byte), alignment (1 byte), and all bit patterns
        // are valid.
        unsafe {
            let ptr = self.data.as_ptr().cast::<c_char>().add(offset);
            from_raw_parts(ptr, valid_len)
        }
    }

    /// Returns a mutable slice of uninitialized capacity starting after current `len` up to
    /// `max_len` (capped by `available_space()`), typed as `MaybeUninit<c_char>` for user copies.
    pub fn extend(&mut self, max_len: usize) -> &mut [MaybeUninit<c_char>] {
        let copy_len = min(self.available_space(), max_len);
        let offset = self.len as usize;
        // SAFETY: `offset + copy_len <= PAYLOAD_SIZE`.
        // `MaybeUninit<c_char>` and `u8` have identical size (1 byte) and alignment (1 byte).
        unsafe {
            let ptr = self.data.as_mut_ptr().cast::<MaybeUninit<c_char>>().add(offset);
            from_raw_parts_mut(ptr, copy_len)
        }
    }

    /// Copies up to `len - *pos` bytes from user pointer `src` at `*pos` into this `MBuf`,
    /// advancing `*pos` and `self.len` by the number of bytes copied.
    pub fn write_from_user(
        &mut self,
        src: UserInPtr<c_char>,
        pos: &mut usize,
        len: usize,
    ) -> Result<(), Status> {
        let dst_slice = self.extend(len - *pos);
        let copy_len = dst_slice.len();
        src.byte_offset(*pos as isize).copy_slice_from_user(dst_slice)?;
        *pos += copy_len;
        self.len += copy_len as u32;
        Ok(())
    }
}

impl Drop for MBuf {
    fn drop(&mut self) {
        MBUF_TOTAL_BYTES_COUNT.add(-(size_of::<MBuf>() as i64));

        // SAFETY: We explicitly drop `self.node` while the page memory is still valid.
        // Because `self.node` is `ManuallyDrop`, it will not be dropped again after `drop` returns.
        // We then return the backing page to PMM.
        unsafe {
            ManuallyDrop::drop(&mut self.node);
            pmm::free_page(self.page);
        }
    }
}

static_assert!(size_of::<MBuf>() == page::SIZE);
static_assert!(align_of::<MBuf>() == 8);

/// Helper function to allocate `num` `MBuf` buffers into a `DoublyLinkedList`.
///
/// If allocation of any buffer fails, all buffers in `bufs` are freed and the error is returned.
fn alloc_mbufs(num: usize, bufs: &mut DoublyLinkedList<*mut MBuf>) -> Result<(), Status> {
    for _ in 0..num {
        let buf_ptr = match MBuf::new() {
            Ok(ptr) => ptr,
            Err(err) => {
                free_mbufs(bufs);
                return Err(err);
            }
        };
        // SAFETY: `buf_ptr` was allocated by `MBuf::new` and is valid and unaliased.
        unsafe {
            bufs.push_back_raw(buf_ptr);
        }
    }
    Ok(())
}

/// Helper function to free all `MBuf` buffers in a `DoublyLinkedList`.
fn free_mbufs(bufs: &mut DoublyLinkedList<*mut MBuf>) {
    while let Some(buf) = bufs.pop_front() {
        // SAFETY: `buf` was popped from `bufs` and points to a valid, initialized, and unaliased
        // `MBuf` allocated from PMM. Dropping in place invokes `MBuf::drop`, returning the page to
        // PMM.
        unsafe {
            drop_in_place(buf);
        }
    }
}

/// Helper function to free the front `MBuf` buffer in a `DoublyLinkedList`.
fn free_front_mbuf(bufs: &mut DoublyLinkedList<*mut MBuf>) {
    if let Some(buf) = bufs.pop_front() {
        // SAFETY: `buf` was popped from `bufs` and points to a valid, initialized, and unaliased
        // `MBuf` allocated from PMM. Dropping in place invokes `MBuf::drop`, returning the page to
        // PMM.
        unsafe {
            drop_in_place(buf);
        }
    }
}

/// MBufChain is a container for storing a stream of bytes or a sequence of datagrams.
///
/// It's designed to back sockets and channels. Don't simultaneously store stream data and datagrams
/// in a single instance.
#[pin_data(PinnedDrop)]
pub struct MBufChain {
    /// The MBufs are placed in a doubly linked list so that both the front and back of the list can
    /// be manipulated and to allow for efficiently splicing lists into each other.
    ///
    /// The active buffers that make up this chain. buffers.front() + read_cursor_off is the read
    /// cursor. buffers.back() is the write cursor.
    #[pin]
    buffers: DoublyLinkedList<*mut MBuf>,
    /// The byte offset of the read cursor in next MBuf.
    read_cursor_off: usize,
    size: usize,
}

impl MBufChain {
    /// Although the maximum size of the data in an MBuf is a kernel implementation detail, it is
    /// visible to user space. To avoid unintentionally changing it when modifying other data
    /// structures round up the currently chosen size (256KiB) to the next multiple of the MBuf
    /// payload size.
    pub const MAX_SIZE: usize = (256usize * 1024).div_ceil(MBuf::PAYLOAD_SIZE) * MBuf::PAYLOAD_SIZE;

    /// Constructs a new `MBufChain`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            buffers <- DoublyLinkedList::<*mut MBuf>::new(),
            read_cursor_off: 0,
            size: 0,
        })
    }

    /// Writes `len` bytes of stream data from `src`.
    ///
    /// Returns an error on failure, although some data may still have been written, in which case
    /// `written` is set with the amount.
    ///
    /// Returns `(res, written)` indicating the operation status and the number of bytes written.
    pub fn write_stream(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        mut len: usize,
    ) -> (Result<(), Status>, usize) {
        // SAFETY: We hold `Pin<&mut Self>` and obtain `&mut Self` to perform buffer writes without
        // moving `Self`.
        let this = unsafe { self.get_unchecked_mut() };
        // Cap len by the max we are allowed to write.
        len = min(Self::MAX_SIZE - this.size, len);
        if len == 0 {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let avail = this.buffers.back().map_or(0, |b| b.available_space());
        let num_buffers = MBuf::num_buffers_for_payload(len.saturating_sub(avail));

        stack_pin_init!(let bufs = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is pinned on stack; obtaining mutable reference to the list is safe.
        let bufs_list = unsafe { bufs.get_unchecked_mut() };

        if alloc_mbufs(num_buffers, bufs_list).is_err() {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let mut pos = 0usize;
        let mut tail_written = 0usize;

        let tail = this.buffers.back_mut().filter(|b| b.available_space() > 0);
        let bufs =
            tail.into_iter().map(|b| (b, true)).chain(bufs_list.iter_mut().map(|b| (b, false)));

        for (buf, is_tail) in bufs {
            let res = buf.write_from_user(src, &mut pos, len);
            if is_tail {
                this.size += pos;
                tail_written = pos;
            }
            if let Err(err) = res {
                // TODO(https://fxbug.dev/42109418): Note that although we set |written| for the
                // benefit of the socket dispatcher updating signals, ultimately we're not
                // indicating to the caller that data added so far in previous copies was written
                // successfully. This means the caller may try to re-send the same data again,
                // leading to duplicate data. Consider changing the socket dispatcher to forward
                // this partial write information to the caller, or consider not committing any of
                // the new data until we can ensure success, or consider putting the socket in a
                // state where it can't succeed a subsequent write.
                free_mbufs(bufs_list);
                return (Err(err), tail_written);
            }
        }

        this.buffers.splice(bufs_list);
        this.size += pos - tail_written;
        (Ok(()), pos)
    }

    /// Writes a datagram of `len` bytes from `src`.
    ///
    /// This operation is atomic in that either the entire datagram is written successfully or the
    /// chain is unmodified.
    ///
    /// Writing a zero-length datagram is an error.
    ///
    /// Returns an error on failure, although some data may still have been written, in which case
    /// `written` is set with the amount.
    ///
    /// Returns `(res, written)` indicating the operation status and the number of bytes written.
    pub fn write_datagram(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        // SAFETY: We hold `Pin<&mut Self>` and obtain `&mut Self` to perform buffer writes without
        // moving `Self`.
        let this = unsafe { self.get_unchecked_mut() };
        if len == 0 {
            return (Err(Status::INVALID_ARGS), 0);
        }
        if len > Self::MAX_SIZE {
            return (Err(Status::OUT_OF_RANGE), 0);
        }
        if Self::MAX_SIZE - this.size < len {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let num_buffers = MBuf::num_buffers_for_payload(len);

        stack_pin_init!(let bufs = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is pinned on stack; obtaining mutable reference to the list is safe.
        let bufs_list = unsafe { bufs.get_unchecked_mut() };

        if alloc_mbufs(num_buffers, bufs_list).is_err() {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let mut pos = 0usize;
        for buf in bufs_list.iter_mut() {
            if let Err(err) = buf.write_from_user(src, &mut pos, len) {
                free_mbufs(bufs_list);
                return (Err(err), 0);
            }
        }

        bufs_list.front_mut().unwrap().pkt_len = len as u32;

        // Successfully built the packet mbufs. Splice into this.buffers.
        this.buffers.splice(bufs_list);
        this.size += len;
        (Ok(()), len)
    }

    /// Reads up to `len` bytes of stream data from the chain into `dst` (no boundaries).
    ///
    /// The actual number of bytes read is returned in `actual`, and this can be non-zero even if
    /// the read itself is an error.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes read.
    pub fn read_stream(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        // SAFETY: We hold `Pin<&mut Self>` and obtain `&mut Self` to read and remove buffers from
        // `Self`.
        let this = unsafe { self.get_unchecked_mut() };
        if this.size == 0 || len == 0 {
            return (Ok(()), 0);
        }

        let mut pos = 0usize;
        let mut read_off = this.read_cursor_off;

        let res = (|| {
            while pos < len
                && let Some(front) = this.buffers.front()
            {
                let slice = front.read(read_off);
                let copy_len = min(slice.len(), len - pos);

                dst.byte_offset(pos as isize).copy_slice_to_user(&slice[..copy_len])?;

                pos += copy_len;
                read_off += copy_len;
                this.size -= copy_len;

                if read_off == front.len as usize {
                    free_front_mbuf(&mut this.buffers);
                    read_off = 0;
                }
            }
            Ok(())
        })();

        // Record the fact that some data might have been read, even if the overall operation is
        // considered a failure.
        this.read_cursor_off = read_off;
        (res, pos)
    }

    /// Reads at most one datagram from the chain into `dst`.
    ///
    /// If `len` is too small to read a complete datagram, a partial datagram is returned and its
    /// remaining bytes are discarded.
    ///
    /// The actual number of bytes read is returned in `actual`, and this can be non-zero even if
    /// the read itself is an error.
    ///
    /// Returns an error on failure. If an error occurs while copying a datagram to `dst`, the
    /// datagram is dropped.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes read.
    pub fn read_datagram(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        mut len: usize,
    ) -> (Result<(), Status>, usize) {
        // SAFETY: We hold `Pin<&mut Self>` and obtain `&mut Self` to read and remove buffers from
        // `Self`.
        let this = unsafe { self.get_unchecked_mut() };
        if this.size == 0 || len == 0 {
            return (Ok(()), 0);
        }

        len = min(len, this.buffers.front().unwrap().pkt_len as usize);
        let mut pos = 0usize;

        let res = (|| {
            while pos < len
                && let Some(front) = this.buffers.front()
            {
                let slice = front.read(0);
                let copy_len = min(slice.len(), len - pos);

                let copy_res = dst.byte_offset(pos as isize).copy_slice_to_user(&slice[..copy_len]);

                // In datagram mode, each visited buffer is popped and discarded completely.
                this.size -= front.len as usize;
                free_front_mbuf(&mut this.buffers);

                copy_res?;
                pos += copy_len;
            }
            Ok(())
        })();

        // Drain any leftover mbufs in the datagram packet if we're consuming data, even
        // if we fail to read bytes.
        while let Some(front) = this.buffers.front()
            && front.pkt_len == 0
        {
            this.size -= front.len as usize;
            free_front_mbuf(&mut this.buffers);
        }

        (res, pos)
    }

    /// Same as `read_stream()`/`read_datagram()` but leaves the bytes in the chain instead of
    /// consuming them, even if an error occurs.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes peeked.
    pub fn peek(
        &self,
        dst: UserOutPtr<c_char>,
        mut len: usize,
        datagram: bool,
    ) -> (Result<(), Status>, usize) {
        if self.size == 0 || len == 0 {
            return (Ok(()), 0);
        }

        if datagram {
            len = min(len, self.buffers.front().unwrap().pkt_len as usize);
        }
        let mut pos = 0usize;
        let mut read_off = self.read_cursor_off;

        let res = (|| {
            for buf in self.buffers.iter() {
                if pos >= len {
                    break;
                }
                let slice = buf.read(read_off);
                let copy_len = min(slice.len(), len - pos);

                dst.byte_offset(pos as isize).copy_slice_to_user(&slice[..copy_len])?;

                pos += copy_len;
                read_off = 0;
            }
            Ok(())
        })();

        (res, pos)
    }

    /// Returns number of bytes stored in the chain.
    /// When `datagram` is true, return only the number of bytes in the first
    /// datagram, or 0 if in `ZX_SOCKET_STREAM` mode.
    pub fn size(&self, datagram: bool) -> usize {
        if datagram && let Some(front) = self.buffers.front() {
            front.pkt_len as usize
        } else {
            self.size
        }
    }

    /// Returns true if chain is full.
    pub fn is_full(&self) -> bool {
        self.size >= Self::MAX_SIZE
    }

    /// Returns true if chain is empty.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

#[pinned_drop]
impl PinnedDrop for MBufChain {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `self` is being dropped and will not be accessed again. Obtaining `&mut Self`
        // allows freeing remaining buffers.
        let this = unsafe { self.get_unchecked_mut() };
        free_mbufs(&mut this.buffers);
    }
}

/// In-tree kernel unit tests for `MBufChain`.
#[cfg(ktest)]
#[unittest::suite(name = "mbuf_rust")]
mod tests {
    use super::{MBuf, MBufChain, alloc_mbufs, free_mbufs};
    use crate::user_copy::{UserInPtr, UserOutPtr};
    use core::ffi::c_char;
    use core::pin::Pin;
    use pin_init::stack_pin_init;
    use unittest::{UserMemory, expect_eq, expect_false, expect_ok, expect_true};
    use zx_status::Status;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum MessageType {
        Stream,
        Datagram,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum ReadType {
        Read,
        Peek,
    }

    fn make_user_in_pattern(
        size: usize,
        pattern: impl Fn(usize) -> u8,
    ) -> Option<(UserMemory, UserInPtr<c_char>)> {
        let alloc_size = if size == 0 { 1 } else { size };
        let mut mem = UserMemory::create(alloc_size)?;
        mem.commit_and_map(alloc_size).ok()?;
        let mut chunk = [0u8; 512];
        let mut offset = 0;
        while offset < size {
            let to_write = core::cmp::min(chunk.len(), size - offset);
            for (i, b) in chunk[..to_write].iter_mut().enumerate() {
                *b = pattern(offset + i);
            }
            mem.vmo_write(&chunk[..to_write], offset as u64).ok()?;
            offset += to_write;
        }
        let ptr = UserInPtr::new(mem.base() as *const c_char);
        Some((mem, ptr))
    }

    fn make_user_in_byte(size: usize, val: u8) -> Option<(UserMemory, UserInPtr<c_char>)> {
        make_user_in_pattern(size, |_| val)
    }

    fn make_user_in(data: &[u8]) -> Option<(UserMemory, UserInPtr<c_char>)> {
        make_user_in_pattern(data.len(), |i| data[i])
    }

    fn make_user_out(size: usize) -> Option<(UserMemory, UserOutPtr<c_char>)> {
        let alloc_size = if size == 0 { 1 } else { size };
        let mut mem = UserMemory::create(alloc_size)?;
        mem.commit_and_map(alloc_size).ok()?;
        let ptr = UserOutPtr::new(mem.base() as *mut c_char);
        Some((mem, ptr))
    }

    fn verify_user_mem(mem: &mut UserMemory, size: usize, pattern: impl Fn(usize) -> u8) -> bool {
        let mut chunk = [0u8; 512];
        let mut offset = 0;
        while offset < size {
            let to_read = core::cmp::min(chunk.len(), size - offset);
            if mem.vmo_read(&mut chunk[..to_read], offset as u64).is_err() {
                return false;
            }
            for (i, &b) in chunk[..to_read].iter().enumerate() {
                if b != pattern(offset + i) {
                    return false;
                }
            }
            offset += to_read;
        }
        true
    }

    /// Writes a string slice into `chain`.
    ///
    /// Helps eliminate boilerplate code dealing with copying in and out of user memory to make the
    /// test logic more obvious.
    fn write_helper(chain: Pin<&mut MBufChain>, str_data: &str, msg_type: MessageType) -> bool {
        let len = str_data.len();
        let Some((_mem, src)) = make_user_in(str_data.as_bytes()) else { return false };
        let (res, written) = match msg_type {
            MessageType::Datagram => chain.write_datagram(src, len),
            MessageType::Stream => chain.write_stream(src, len),
        };
        res.is_ok() && written == len
    }

    /// Reads or peeks data from `chain`.
    fn read_helper(
        chain: &mut Pin<&mut MBufChain>,
        buf: &mut [u8],
        len: usize,
        msg_type: MessageType,
        read_type: ReadType,
        actual: &mut usize,
    ) -> Result<(), Status> {
        let (mut mem, dst) = make_user_out(len).ok_or(Status::NO_MEMORY)?;
        let datagram = msg_type == MessageType::Datagram;
        let (res, nread) = match read_type {
            ReadType::Read => {
                if datagram {
                    chain.as_mut().read_datagram(dst, len)
                } else {
                    chain.as_mut().read_stream(dst, len)
                }
            }
            ReadType::Peek => chain.peek(dst, len, datagram),
        };
        *actual = nread;
        if nread > 0 {
            let copy_len = core::cmp::min(nread, buf.len());
            mem.vmo_read(&mut buf[..copy_len], 0)?;
        }
        res
    }

    /// Tests initial state of MBufChain.
    #[test]
    fn test_initial_state() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let chain = chain_pin.as_ref();
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.size(false), 0);
    }

    /// Tests reading stream when empty.
    #[test]
    fn test_stream_read_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut buf = [0u8; 1];
        let mut actual = 0;
        let res =
            read_helper(&mut chain, &mut buf, 1, MessageType::Stream, ReadType::Read, &mut actual);
        expect_ok!(res);
        expect_eq!(actual, 0);
    }

    /// Tests reading stream with zero length.
    #[test]
    fn test_stream_read_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "x", MessageType::Stream));

        let mut buf = [0u8; 1];
        let mut actual = 0;
        let res =
            read_helper(&mut chain, &mut buf, 0, MessageType::Stream, ReadType::Read, &mut actual);
        expect_ok!(res);
        expect_eq!(actual, 0);
    }

    /// Tests basic stream writing and reading.
    #[test]
    fn test_stream_write_basic() {
        const WRITE_LEN: usize = 1024;
        const NUM_WRITES: usize = 5;

        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        // Call write several times with different buffer contents.
        for i in 0..NUM_WRITES {
            let (_mem, src) = make_user_in_byte(WRITE_LEN, b'A' + (i as u8)).unwrap();
            let (res, written) = chain.as_mut().write_stream(src, WRITE_LEN);
            expect_ok!(res);
            expect_eq!(written, WRITE_LEN);
            expect_false!(chain.is_empty());
            expect_false!(chain.is_full());
            expect_eq!(chain.size(false), (i + 1) * WRITE_LEN);
        }

        // Read it all back in one call.
        const TOTAL_LEN: usize = WRITE_LEN * NUM_WRITES;
        expect_eq!(chain.size(false), TOTAL_LEN);

        let (mut mem_out, dst) = make_user_out(TOTAL_LEN).unwrap();
        let (res, actual) = chain.as_mut().read_stream(dst, TOTAL_LEN);
        expect_ok!(res);
        expect_eq!(actual, TOTAL_LEN);
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.size(false), 0);

        // Verify result.
        expect_true!(verify_user_mem(&mut mem_out, TOTAL_LEN, |offset| {
            b'A' + ((offset / WRITE_LEN) as u8)
        }));
    }

    /// Tests stream writing zero length.
    #[test]
    fn test_stream_write_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let (_mem, src) = make_user_in_byte(1, 0).unwrap();
        let (res, written) = chain.as_mut().write_stream(src, 0);
        // TODO(maniscalco): Is ZX_ERR_SHOULD_WAIT really the right error here in this case?
        expect_true!(res == Err(Status::SHOULD_WAIT));
        expect_eq!(written, 0);
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.size(false), 0);
    }

    /// Tests stream writing beyond capacity.
    #[test]
    fn test_stream_write_too_much() {
        const WRITE_LEN: usize = 65536;
        let (_mem_in, src) = make_user_in_byte(WRITE_LEN, 0).unwrap();

        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        // Fill the chain until it refuses to take any more.
        let mut total_written = 0usize;
        while !chain.is_full() {
            let (res, written) = chain.as_mut().write_stream(src, WRITE_LEN);
            if res.is_err() {
                break;
            }
            total_written += written;
        }

        expect_false!(chain.is_empty());
        expect_true!(chain.is_full());
        expect_eq!(total_written, chain.size(false));

        // Read it all back out and see we get back the same number of bytes we wrote.
        let (_mem_out, dst) = make_user_out(WRITE_LEN).unwrap();
        let mut total_read = 0usize;
        while !chain.is_empty() {
            let (res, bytes_read) = chain.as_mut().read_stream(dst, WRITE_LEN);
            if res.is_err() || bytes_read == 0 {
                break;
            }
            total_read += bytes_read;
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
        expect_eq!(total_written, total_read);
    }

    /// Tests stream peeking data.
    #[test]
    fn test_stream_peek() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Stream));
        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Stream));

        let mut read_buf = [0u8; 10];
        let mut actual = 0;

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"a");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            4,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc1");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            6,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc123");

        expect_eq!(chain.size(false), 6);
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            6,
            MessageType::Stream,
            ReadType::Read,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc123");
    }

    /// Tests stream peeking when empty.
    #[test]
    fn test_stream_peek_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_eq!(actual, 0);
    }

    /// Tests stream peeking zero length.
    #[test]
    fn test_stream_peek_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "a", MessageType::Stream));

        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_eq!(actual, 0);
    }

    /// Tests stream peeking with underflow.
    #[test]
    fn test_stream_peek_underflow() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Stream));
        let mut read_buf = [0u8; 10];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");

        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Stream));
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc123");
    }

    /// Tests datagram reading when empty.
    #[test]
    fn test_datagram_read_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, 0);
        expect_true!(chain.is_empty());
    }

    /// Tests datagram reading zero length.
    #[test]
    fn test_datagram_read_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "x", MessageType::Datagram));

        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, 0);
        expect_false!(chain.is_empty());
    }

    /// Tests datagram reading with a small buffer.
    #[test]
    fn test_datagram_read_buffer_too_small() {
        const WRITE_LEN: usize = 32;
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        let (_mem_a, src_a) = make_user_in_byte(WRITE_LEN, b'A').unwrap();
        let (res_a, written_a) = chain.as_mut().write_datagram(src_a, WRITE_LEN);
        expect_ok!(res_a);
        expect_eq!(written_a, WRITE_LEN);
        expect_eq!(chain.size(false), WRITE_LEN);
        expect_false!(chain.is_empty());

        let (_mem_b, src_b) = make_user_in_byte(WRITE_LEN, b'B').unwrap();
        let (res_b, written_b) = chain.as_mut().write_datagram(src_b, WRITE_LEN);
        expect_ok!(res_b);
        expect_eq!(written_b, WRITE_LEN);
        expect_eq!(chain.size(false), 2 * WRITE_LEN);
        expect_false!(chain.is_empty());

        let mut read_buf = [0u8; WRITE_LEN];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, 1);
        expect_eq!(read_buf[0], b'A');
        expect_false!(chain.is_empty());

        expect_eq!(chain.size(false), WRITE_LEN);
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            WRITE_LEN,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, WRITE_LEN);
        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
        expect_true!(read_buf.iter().all(|&b| b == b'B'));
    }

    /// Tests basic datagram writing and reading.
    #[test]
    fn test_datagram_write_basic() {
        const NUM_DATAGRAMS: usize = 100;

        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        let mut total_written = 0;

        // Write a series of datagrams with different sizes.
        for i in 1..=NUM_DATAGRAMS {
            let (_mem, src) = make_user_in_byte(i, i as u8).unwrap();
            let (res, written) = chain.as_mut().write_datagram(src, i);
            expect_ok!(res);
            expect_eq!(written, i);
            total_written += written;
            expect_false!(chain.is_empty());
            expect_false!(chain.is_full());
        }

        // Verify size() returns correctly
        expect_eq!(chain.size(true), 1);
        expect_eq!(chain.size(false), total_written);

        // Read them back and verify their contents.
        for i in 1..=NUM_DATAGRAMS {
            expect_eq!(chain.size(true), i);
            let mut read_buf = [0u8; 100];
            let mut actual = 0;
            expect_ok!(read_helper(
                &mut chain,
                &mut read_buf[..i],
                i,
                MessageType::Datagram,
                ReadType::Read,
                &mut actual
            ));
            expect_eq!(actual, i);
            expect_true!(read_buf[..i].iter().all(|&b| b == (i as u8)));
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
    }

    /// Tests datagram writing zero length.
    #[test]
    fn test_datagram_write_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let (_mem, src) = make_user_in_byte(1, 0).unwrap();
        let (res, written) = chain.as_mut().write_datagram(src, 0);
        expect_true!(res == Err(Status::INVALID_ARGS));
        expect_eq!(written, 0);
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.size(true), 0);
        expect_eq!(chain.size(false), 0);
    }

    /// Tests datagram writing beyond capacity.
    #[test]
    fn test_datagram_write_too_much() {
        const WRITE_LEN: usize = 65536;
        let (_mem_in, src) = make_user_in_byte(WRITE_LEN, 0).unwrap();

        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        // Fill the chain until it refuses to take any more.
        let mut num_datagrams_written = 0;
        while !chain.is_full() {
            let (res, written) = chain.as_mut().write_datagram(src, WRITE_LEN);
            if res.is_err() {
                break;
            }
            num_datagrams_written += 1;
            expect_eq!(written, WRITE_LEN);
        }

        expect_false!(chain.is_empty());
        expect_eq!(chain.size(false), WRITE_LEN * num_datagrams_written);

        // Read it all back out and see that there's none left over.
        let (_mem_out, dst) = make_user_out(WRITE_LEN).unwrap();
        let mut num_datagrams_read = 0;
        while !chain.is_empty() {
            let (res, actual) = chain.as_mut().read_datagram(dst, WRITE_LEN);
            if res.is_err() || actual == 0 {
                break;
            }
            num_datagrams_read += 1;
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
        expect_eq!(num_datagrams_written, num_datagrams_read);
    }

    /// Tests datagram buffer reuse.
    #[test]
    fn test_datagram_reuse_mbuf() {
        let large_write = MBuf::PAYLOAD_SIZE + 10;

        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        // Write two datagrams.
        let (_mem_a, a_src) = make_user_in_byte(1, b'a').unwrap();
        let (res_a, written_a) = chain.as_mut().write_datagram(a_src, 1);
        expect_ok!(res_a);
        expect_eq!(written_a, 1);
        let (_mem_b, b_src) = make_user_in_byte(1, b'b').unwrap();
        let (res_b, written_b) = chain.as_mut().write_datagram(b_src, 1);
        expect_ok!(res_b);
        expect_eq!(written_b, 1);

        // Now read them both out.
        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(read_buf[0], b'a');
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(read_buf[0], b'b');

        // Now write a large datagram that spans two buffers.
        let (_mem_c, c_src) = make_user_in_byte(large_write, b'c').unwrap();
        let (res_c, written_c) = chain.as_mut().write_datagram(c_src, large_write);
        expect_ok!(res_c);
        expect_eq!(written_c, large_write);

        // Write in a second small datagram.
        let (_mem_d, d_src) = make_user_in_byte(1, b'd').unwrap();
        let (res_d, written_d) = chain.as_mut().write_datagram(d_src, 1);
        expect_ok!(res_d);
        expect_eq!(written_d, 1);

        // Do a short read to consume the first datagram.
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(read_buf[0], b'c');

        // Reading again should give us the second datagram we wrote, as the remaining of the first
        // should have been discarded.
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(read_buf[0], b'd');

        // At this point the socket should be empty.
        expect_true!(chain.is_empty());
    }

    /// Tests writing a datagram packet larger than the mbuf's capacity.
    #[test]
    fn test_datagram_write_huge_packet() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        let huge_packet_size = MBufChain::MAX_SIZE + 1;
        let (_mem, src) = make_user_in_byte(1, 0).unwrap();
        let (res, _written) = chain.as_mut().write_datagram(src, huge_packet_size);
        expect_true!(res == Err(Status::OUT_OF_RANGE));
    }

    /// Tests datagram peeking.
    #[test]
    fn test_datagram_peek() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Datagram));

        let mut read_buf = [0u8; 10];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"a");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");

        // Make sure peeking didn't affect an actual read.
        expect_eq!(chain.size(false), 3);
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");
    }

    /// Tests datagram peeking empty.
    #[test]
    fn test_datagram_peek_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_eq!(actual, 0);
    }

    /// Tests datagram peeking zero length.
    #[test]
    fn test_datagram_peek_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "a", MessageType::Datagram));

        let mut read_buf = [0u8; 1];
        let mut actual = 0;
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_eq!(actual, 0);
    }

    /// Tests datagram peeking underflow.
    #[test]
    fn test_datagram_peek_underflow() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Datagram));
        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Datagram));

        let mut read_buf = [0u8; 10];
        let mut actual = 0;

        // Datagram peeks should not return more than a single message.
        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"abc");

        expect_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual
        ));
        expect_true!(&read_buf[..actual] == b"123");
    }

    /// Tests multi-buffer datagram partial read discarding trailing continuation buffers.
    #[test]
    fn test_datagram_read_multibuffer_discard() {
        const LARGE_PAYLOAD: usize = MBuf::PAYLOAD_SIZE * 2 + 50;
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        let (_mem_in, src) = make_user_in_byte(LARGE_PAYLOAD, b'Z').unwrap();
        let (res, written) = chain.as_mut().write_datagram(src, LARGE_PAYLOAD);
        expect_ok!(res);
        expect_eq!(written, LARGE_PAYLOAD);
        expect_eq!(chain.size(false), LARGE_PAYLOAD);
        expect_eq!(chain.size(true), LARGE_PAYLOAD);

        // Read only enough to span into the second buffer, leaving the third buffer untouched.
        const READ_LEN: usize = MBuf::PAYLOAD_SIZE + 20;
        let (mut mem_out, dst) = make_user_out(READ_LEN).unwrap();
        let (res_r, actual) = chain.as_mut().read_datagram(dst, READ_LEN);
        expect_ok!(res_r);
        expect_eq!(actual, READ_LEN);
        expect_true!(verify_user_mem(&mut mem_out, READ_LEN, |_| b'Z'));

        // All remaining bytes (including continuation buffer 3) should have been discarded.
        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
        expect_eq!(chain.size(true), 0);
    }

    /// Tests interleaved partial stream reads across MBuf page boundaries.
    #[test]
    fn test_stream_read_interleaved_chunks() {
        const TOTAL_BYTES: usize = MBuf::PAYLOAD_SIZE * 3;
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        let (_mem_in, src) = make_user_in_pattern(TOTAL_BYTES, |i| (i % 251) as u8).unwrap();
        let (res, written) = chain.as_mut().write_stream(src, TOTAL_BYTES);
        expect_ok!(res);
        expect_eq!(written, TOTAL_BYTES);
        expect_eq!(chain.size(false), TOTAL_BYTES);

        let mut total_read = 0usize;
        let chunk_sizes = [100, MBuf::PAYLOAD_SIZE - 50, 500, MBuf::PAYLOAD_SIZE, 1000];

        for &chunk in &chunk_sizes {
            if total_read >= TOTAL_BYTES {
                break;
            }
            let to_read = core::cmp::min(chunk, TOTAL_BYTES - total_read);
            let (mut mem_out, dst) = make_user_out(to_read).unwrap();
            let (res_r, actual) = chain.as_mut().read_stream(dst, to_read);
            expect_ok!(res_r);
            expect_eq!(actual, to_read);
            expect_true!(verify_user_mem(&mut mem_out, actual, |i| {
                ((total_read + i) % 251) as u8
            }));
            total_read += actual;
            expect_eq!(chain.size(false), TOTAL_BYTES - total_read);
        }

        // Read the remaining bytes.
        if total_read < TOTAL_BYTES {
            let rem = TOTAL_BYTES - total_read;
            let (mut mem_rem, rem_dst) = make_user_out(rem).unwrap();
            let (res_rem, actual) = chain.as_mut().read_stream(rem_dst, rem);
            expect_ok!(res_rem);
            expect_eq!(actual, rem);
            expect_true!(verify_user_mem(&mut mem_rem, actual, |i| {
                ((total_read + i) % 251) as u8
            }));
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.size(false), 0);
    }

    /// Tests helper calculations for MBuf sizing and buffer requirements.
    #[test]
    fn test_mbuf_calculations() {
        expect_eq!(MBuf::num_buffers_for_payload(0), 0);
        expect_eq!(MBuf::num_buffers_for_payload(1), 1);
        expect_eq!(MBuf::num_buffers_for_payload(MBuf::PAYLOAD_SIZE), 1);
        expect_eq!(MBuf::num_buffers_for_payload(MBuf::PAYLOAD_SIZE + 1), 2);
        expect_eq!(MBuf::num_buffers_for_payload(2 * MBuf::PAYLOAD_SIZE), 2);
        expect_eq!(MBuf::num_buffers_for_payload(2 * MBuf::PAYLOAD_SIZE + 1), 3);

        expect_true!(MBufChain::MAX_SIZE >= 256 * 1024);
        expect_eq!(MBufChain::MAX_SIZE % MBuf::PAYLOAD_SIZE, 0);
    }

    /// Tests datagram write failure when exceeding chain capacity.
    #[test]
    fn test_datagram_write_should_wait_boundary() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        // Fill close to maximum capacity.
        const FILL_SIZE: usize = MBufChain::MAX_SIZE - 100;
        let (_mem, src) = make_user_in_byte(FILL_SIZE, b'K').unwrap();
        let (res, written) = chain.as_mut().write_datagram(src, FILL_SIZE);
        expect_ok!(res);
        expect_eq!(written, FILL_SIZE);
        expect_eq!(chain.size(false), FILL_SIZE);

        // Attempting to write a 101-byte datagram exceeds MAX_SIZE.
        let (_overflow_mem, overflow_src) = make_user_in_byte(101, b'V').unwrap();
        let (res_overflow, overflow_written) = chain.as_mut().write_datagram(overflow_src, 101);
        expect_true!(res_overflow == Err(Status::SHOULD_WAIT));
        expect_eq!(overflow_written, 0);
        expect_eq!(chain.size(false), FILL_SIZE);
    }

    /// Tests that MBuf allocation, insertion, removal, and freeing preserves node state.
    #[test]
    fn test_mbuf_alloc_free_node_lifecycle() {
        stack_pin_init!(let bufs = fbl::DoublyLinkedList::<*mut MBuf>::new());
        let bufs = unsafe { bufs.get_unchecked_mut() };

        for _ in 0..10 {
            let buf_ptr = MBuf::new().expect("alloc mbuf");
            unsafe {
                bufs.push_back_raw(buf_ptr);
            }
        }

        while let Some(buf) = bufs.pop_front() {
            unsafe {
                drop_in_place(buf);
            }
        }

        expect_true!(bufs.is_empty());
    }

    /// Tests allocating and freeing MBufs using alloc_mbufs and free_mbufs helpers.
    #[test]
    fn test_alloc_free_mbufs() {
        stack_pin_init!(let bufs = fbl::DoublyLinkedList::<*mut MBuf>::new());
        let bufs = unsafe { bufs.get_unchecked_mut() };

        expect_ok!(alloc_mbufs(5, bufs));
        expect_false!(bufs.is_empty());
        free_mbufs(bufs);
        expect_true!(bufs.is_empty());
    }
}
