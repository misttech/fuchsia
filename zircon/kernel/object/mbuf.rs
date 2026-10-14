// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::user_copy::{UserInPtr, UserOutPtr};
use crate::vm::page::{VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::page_state::VmPageState;
use crate::vm::pmm_node::PmmOptDelayReuse;
use crate::vm::{physmap, pmm};
use core::cmp::min;
use core::convert::Infallible;
use core::ffi::c_char;
use core::mem::{MaybeUninit, align_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::slice::{from_raw_parts, from_raw_parts_mut};
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
#[pin_data]
struct MBuf {
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<MBuf>,

    /// Length of the valid `data` in this buffer. Writes can append more to `data` and increment
    /// this length.
    // Safety invariant: self.len as usize <= Self::PAYLOAD_SIZE.
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
    // Safety invariant: self.page is the PMM-allocated VmPage backing this MBuf in the physmap and
    // is unlinked while this MBuf is alive.
    page: VmPagePtr,

    /// The data field is left uninitialized as the caller is going to immediately overwrite with
    /// the payload, and is trusted to not access any uninitialized portions.
    /// TODO: maybe union data with char* blocks for large messages
    // Safety invariant: self.data[0..self.len as usize] is initialized.
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

    /// Returns number of bytes of free space in this MBuf.
    #[inline]
    pub fn available_space(&self) -> usize {
        // Use `wrapping_sub` instead of `-` to avoid emitting an overflow panic branch under
        // `-Cdebug-assertions=yes` (enabled by `zx_assert_level = 2`), since `self.len` is
        // guaranteed by `MBuf` invariants to be `<= Self::PAYLOAD_SIZE`.
        Self::PAYLOAD_SIZE.wrapping_sub(self.len as usize)
    }

    /// Copies up to `max_len` bytes starting at `offset` (capped by `self.len - offset`) from this
    /// `MBuf` to `dst`, returning the number of bytes copied.
    ///
    /// # Safety
    ///
    /// `offset` must be `<= self.len as usize`.
    #[inline]
    pub unsafe fn read_to_user(
        &self,
        offset: usize,
        dst: UserOutPtr<c_char>,
        max_len: usize,
    ) -> Result<usize, Status> {
        // SAFETY: Caller guarantees `offset <= self.len as usize`. By `MBuf` safety invariants,
        // `self.len as usize <= PAYLOAD_SIZE` and `self.data[0..self.len as usize]` is initialized.
        // `c_char` and `u8` have identical size (1 byte), alignment (1 byte), and all bit patterns
        // are valid.
        let copy_len = min((self.len as usize).wrapping_sub(offset), max_len);
        let slice = unsafe {
            let ptr = self.data.as_ptr().cast::<c_char>().wrapping_add(offset);
            from_raw_parts(ptr, copy_len)
        };
        dst.copy_slice_to_user(slice)?;
        Ok(copy_len)
    }

    /// Copies up to `max_len` bytes (capped by `available_space()`) from `src` into this `MBuf`,
    /// advancing `self.len` and returning the number of bytes copied.
    #[inline]
    pub fn write_from_user(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        max_len: usize,
    ) -> Result<usize, Status> {
        let copy_len = min(self.available_space(), max_len);
        let this = self.project();
        let offset = *this.len as usize;
        // SAFETY: `offset + copy_len <= PAYLOAD_SIZE`.
        // `MaybeUninit<c_char>` and `u8` have identical size (1 byte) and alignment (1 byte).
        let dst = unsafe {
            let ptr = this.data.as_mut_ptr().cast::<MaybeUninit<c_char>>().wrapping_add(offset);
            from_raw_parts_mut(ptr, copy_len)
        };
        src.copy_slice_from_user(dst)?;
        *this.len = this.len.wrapping_add(copy_len as u32);
        Ok(copy_len)
    }

    /// Initializes an uninitialized `MBuf` in the physical map of the given `page`.
    ///
    /// Returns a non-null pointer to the initialized `MBuf`.
    ///
    /// # Safety
    ///
    /// `page` must be an allocated `VmPage` whose physical memory is mapped in the physmap, and
    /// the caller must possess conceptual ownership of the page to set its state.
    #[inline]
    unsafe fn init_in_page(page: VmPagePtr) -> NonNull<Self> {
        // SAFETY: Caller guarantees `page` is an allocated `VmPage` and possesses conceptual
        // ownership of it. `buf_ptr` points to the non-null mapped physical memory of the
        // allocated page and is initialized without dropping.
        unsafe {
            page.set_state(VmPageState(vm_page_state::IPC));
            let buf_ptr = physmap::paddr_to_physmap(page.paddr()).0 as *mut Self;
            buf_ptr.write(Self {
                node: DoublyLinkedListNode::new(),
                len: 0,
                pkt_len: 0,
                page,
                data: MaybeUninit::uninit(),
            });
            NonNull::new_unchecked(buf_ptr)
        }
    }

    /// Allocates and initializes a single `MBuf` from PMM.
    #[inline]
    fn alloc() -> Option<NonNull<Self>> {
        let page = pmm::node().alloc_page(0).ok()?;
        MBUF_TOTAL_BYTES_COUNT.add(size_of::<Self>() as i64);
        // SAFETY: `page` was newly allocated by `PmmNode::alloc_page` and is exclusively owned by
        // the caller until linked or freed.
        unsafe { Some(Self::init_in_page(page)) }
    }

    /// Frees a single unlinked `MBuf` to PMM.
    ///
    /// # Safety
    ///
    /// `buf` must be an unlinked `MBuf` allocated from PMM whose backing page is not in any list,
    /// and `buf` must not be accessed after this call.
    #[inline]
    unsafe fn free(buf: *mut Self) {
        MBUF_TOTAL_BYTES_COUNT.add(-(size_of::<Self>() as i64));
        // SAFETY: Caller guarantees `buf` is a valid, unlinked `MBuf` allocated from PMM.
        unsafe {
            pmm::node().free_page((*buf).page, PmmOptDelayReuse::Default);
        }
    }
}

static_assert!(size_of::<MBuf>() == page::SIZE);
static_assert!(align_of::<MBuf>() == 8);

/// Pops the front `MBuf` (if any) and frees it directly to PMM.
///
/// # Safety
///
/// Every element in `buffers` must be a valid, exclusively owned `*mut MBuf` allocated from PMM.
#[inline]
unsafe fn pop_and_free_front(buffers: Pin<&mut DoublyLinkedList<*mut MBuf>>) {
    if let Some(buf) = buffers.pop_front() {
        // SAFETY: Caller guarantees `buf` popped from `buffers` is a valid, unlinked `MBuf`
        // allocated from PMM.
        unsafe {
            MBuf::free(buf);
        }
    }
}

/// Helper function to allocate `num` `MBuf` buffers into a `DoublyLinkedList`.
///
/// If allocation of any buffer fails, the error is returned and `bufs` is left unmodified.
fn alloc_mbufs(num: usize, mut bufs: Pin<&mut DoublyLinkedList<*mut MBuf>>) -> Result<(), Status> {
    if num == 0 {
        return Ok(());
    }

    stack_pin_init!(let pages = VmPageDoublyLinkedList::new());
    pmm::node().alloc_pages(num, 0, pages.as_mut())?;

    while let Some(page) = pages.as_mut().pop_front() {
        // SAFETY: `page` was popped from `pages` allocated by `PmmNode::alloc_pages` and is valid.
        // The resulting `buf_ptr` is newly initialized and not currently in any list.
        unsafe {
            let buf_ptr = MBuf::init_in_page(VmPagePtr::new(page)).as_ptr();
            bufs.as_mut().push_front_raw(buf_ptr);
        }
    }

    MBUF_TOTAL_BYTES_COUNT.add((num * size_of::<MBuf>()) as i64);
    Ok(())
}

/// Helper function to free all `MBuf` buffers in a `DoublyLinkedList`.
///
/// # Safety
///
/// Every element in `bufs` must be a valid, exclusively owned `*mut MBuf` allocated from PMM.
unsafe fn free_mbufs(mut bufs: Pin<&mut DoublyLinkedList<*mut MBuf>>) {
    if bufs.is_empty() {
        return;
    }

    stack_pin_init!(let pages = VmPageDoublyLinkedList::new());

    let mut count = 0usize;
    while let Some(buf_ptr) = bufs.as_mut().pop_front() {
        // SAFETY: Caller guarantees `buf_ptr` popped from `bufs` points to a valid, initialized
        // `MBuf` allocated from PMM whose backing page is not in any list.
        unsafe {
            let page_ptr = (*buf_ptr).page;
            pages.as_mut().push_front_raw(page_ptr.as_non_null());
        }
        count += 1;
    }

    MBUF_TOTAL_BYTES_COUNT.add(-((count * size_of::<MBuf>()) as i64));

    // SAFETY: All pages in `pages` were allocated from PMM and are being returned to PMM.
    unsafe {
        pmm::node().free_list(pages, PmmOptDelayReuse::Default);
    }
}

/// An RAII guard for a temporary linked list of `MBuf` pointers.
///
/// Any `MBuf`s remaining in the list when dropped are automatically freed to PMM via `free_mbufs`
/// (splicing the list into a destination `MBufChain` leaves it empty so `drop` becomes a no-op).
struct MBufListGuard<'a>(
    // Safety invariant: every element in `self.0` is a valid, exclusively owned `*mut MBuf`
    // allocated from PMM via `MBuf::init_in_page`.
    Pin<&'a mut DoublyLinkedList<*mut MBuf>>,
);

impl<'a> MBufListGuard<'a> {
    /// Creates a guard wrapping a pinned `list`.
    ///
    /// # Safety
    ///
    /// Any elements in `list` must be valid, exclusively owned `*mut MBuf`s allocated from PMM.
    #[inline]
    unsafe fn new(list: Pin<&'a mut DoublyLinkedList<*mut MBuf>>) -> Self {
        Self(list)
    }

    /// Pushes an unlinked `MBuf` to the front of the list.
    ///
    /// # Safety
    ///
    /// `buf` must be a valid, unlinked, exclusively owned `MBuf` allocated from PMM.
    #[inline]
    unsafe fn push(&mut self, buf: *mut MBuf) {
        // SAFETY: Caller guarantees `buf` is a valid, unlinked `MBuf` allocated from PMM.
        unsafe {
            self.0.as_mut().push_front_raw(buf);
        }
    }

    /// Returns a pinned mutable reference to the underlying list.
    #[inline]
    fn as_mut(&mut self) -> Pin<&mut DoublyLinkedList<*mut MBuf>> {
        self.0.as_mut()
    }
}

impl Drop for MBufListGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: By the `MBufListGuard` safety invariant, every element in `self.0` is a valid,
        // exclusively owned `*mut MBuf` allocated from PMM.
        unsafe {
            free_mbufs(self.0.as_mut());
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
    // Safety invariant: every element in `buffers` is a valid, exclusively owned `*mut MBuf`
    // allocated from PMM via `MBuf::init_in_page`.
    #[pin]
    buffers: DoublyLinkedList<*mut MBuf>,
    /// The byte offset of the read cursor in next MBuf.
    // Safety invariant: `read_cursor_off <= buffers.front().map_or(0, |b| b.len as usize)`.
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

    /// Allocates a single `MBuf`, copies `len` bytes from `src`, and appends it to `self.buffers`.
    ///
    /// Precondition: `len <= MBuf::PAYLOAD_SIZE`.
    #[inline]
    fn write_single_mbuf(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
        pkt_len: u32,
    ) -> (Result<(), Status>, usize) {
        let Some(mut buf) = MBuf::alloc() else {
            return (Err(Status::SHOULD_WAIT), 0);
        };
        // SAFETY: `buf` is backed by a newly allocated physmap page, is exclusively owned, and is
        // never moved.
        if let Err(err) = unsafe { Pin::new_unchecked(buf.as_mut()) }.write_from_user(src, len) {
            // SAFETY: `buf` was just allocated by `MBuf::alloc` and is not in any list.
            unsafe {
                MBuf::free(buf.as_ptr());
            }
            return (Err(err), 0);
        }
        let buf_ptr = buf.as_ptr();
        let this = self.project();
        // SAFETY: `buf_ptr` is a valid, unlinked, exclusively owned `MBuf` allocated from PMM.
        unsafe {
            (*buf_ptr).pkt_len = pkt_len;
            this.buffers.push_back_raw(buf_ptr);
        }
        *this.size = this.size.wrapping_add(len);
        (Ok(()), len)
    }

    /// Writes `len` bytes of stream data from `src`.
    ///
    /// Returns an error on failure, although some data may still have been written, in which case
    /// `written` is set with the amount.
    ///
    /// Returns `(res, written)` indicating the operation status and the number of bytes written.
    #[inline]
    pub fn write_stream(
        mut self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        // Cap len by the max we are allowed to write.
        let len = min(Self::MAX_SIZE.wrapping_sub(self.size), len);
        if len == 0 {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let mut this = self.as_mut().project();
        if let Some(tail) = this.buffers.as_mut().back_mut().filter(|b| b.available_space() > 0) {
            if len <= tail.available_space() {
                if let Err(err) = tail.write_from_user(src, len) {
                    return (Err(err), 0);
                }
                *this.size = this.size.wrapping_add(len);
                return (Ok(()), len);
            }
        } else if len <= MBuf::PAYLOAD_SIZE {
            return self.write_single_mbuf(src, len, 0);
        }

        self.write_stream_slow(src, len)
    }

    #[cold]
    fn write_stream_slow(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        let mut this = self.project();
        let avail = this.buffers.back().map_or(0, |b| b.available_space());
        let num_buffers = MBuf::num_buffers_for_payload(len - avail);

        stack_pin_init!(let bufs = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is newly initialized and empty.
        let mut bufs_guard = unsafe { MBufListGuard::new(bufs) };

        if alloc_mbufs(num_buffers, bufs_guard.as_mut()).is_err() {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let mut pos = 0usize;
        if avail > 0
            && let Some(tail) = this.buffers.as_mut().back_mut()
        {
            match tail.write_from_user(src, len) {
                Ok(n) => {
                    pos += n;
                    *this.size += n;
                }
                Err(err) => return (Err(err), 0),
            }
        }
        let tail_written = pos;

        for buf in bufs_guard.as_mut().iter_mut() {
            match buf.write_from_user(src.byte_offset(pos as isize), len - pos) {
                Ok(n) => pos += n,
                Err(err) => {
                    // TODO(https://fxbug.dev/42109418): Note that although we set |written| for the
                    // benefit of the socket dispatcher updating signals, ultimately we're not
                    // indicating to the caller that data added so far in previous copies was
                    // written successfully. This means the caller may try to re-send the same data
                    // again, leading to duplicate data. Consider changing the socket dispatcher to
                    // forward this partial write information to the caller, or consider not
                    // committing any of the new data until we can ensure success, or consider
                    // putting the socket in a state where it can't succeed a subsequent write.
                    return (Err(err), tail_written);
                }
            }
        }

        this.buffers.splice(bufs_guard.as_mut());
        *this.size += pos - tail_written;
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
    #[inline]
    pub fn write_datagram(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        if len == 0 {
            return (Err(Status::INVALID_ARGS), 0);
        }
        if len > Self::MAX_SIZE {
            return (Err(Status::OUT_OF_RANGE), 0);
        }
        if Self::MAX_SIZE.wrapping_sub(self.size) < len {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        if len <= MBuf::PAYLOAD_SIZE {
            return self.write_single_mbuf(src, len, len as u32);
        }

        self.write_datagram_slow(src, len)
    }

    #[cold]
    fn write_datagram_slow(
        self: Pin<&mut Self>,
        src: UserInPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        let this = self.project();
        let num_buffers = MBuf::num_buffers_for_payload(len);

        stack_pin_init!(let bufs = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is newly initialized and empty.
        let mut bufs_guard = unsafe { MBufListGuard::new(bufs) };

        if alloc_mbufs(num_buffers, bufs_guard.as_mut()).is_err() {
            return (Err(Status::SHOULD_WAIT), 0);
        }

        let mut pos = 0usize;
        for buf in bufs_guard.as_mut().iter_mut() {
            match buf.write_from_user(src.byte_offset(pos as isize), len - pos) {
                Ok(n) => pos += n,
                Err(err) => return (Err(err), 0),
            }
        }

        if let Some(front) = bufs_guard.as_mut().front_mut() {
            *front.project().pkt_len = len as u32;
        }

        // Successfully built the packet mbufs. Splice into this.buffers.
        this.buffers.splice(bufs_guard.as_mut());
        *this.size += len;
        (Ok(()), len)
    }

    /// Reads up to `len` bytes of stream data from the chain into `dst` (no boundaries).
    ///
    /// The actual number of bytes read is returned in `actual`, and this can be non-zero even if
    /// the read itself is an error.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes read.
    #[inline]
    pub fn read_stream(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        let len = min(self.size, len);
        if len == 0 {
            return (Ok(()), 0);
        }

        if let Some(front) = self.buffers.front() {
            let read_off = self.read_cursor_off;
            let avail = (front.len as usize).wrapping_sub(read_off);
            if len <= avail {
                // SAFETY: `read_off <= front.len as usize` by the `MBufChain` `read_cursor_off`
                // safety invariant.
                if let Err(e) = unsafe { front.read_to_user(read_off, dst, len) } {
                    return (Err(e), 0);
                }
                let this = self.project();
                *this.size = this.size.wrapping_sub(len);
                if len < avail {
                    *this.read_cursor_off = read_off.wrapping_add(len);
                } else {
                    *this.read_cursor_off = 0;
                    // SAFETY: By the `MBufChain` `buffers` safety invariant, every element in
                    // `this.buffers` is a valid, exclusively owned `*mut MBuf` allocated from PMM.
                    unsafe {
                        pop_and_free_front(this.buffers);
                    }
                }
                return (Ok(()), len);
            }
        }

        self.read_stream_slow(dst, len)
    }

    #[cold]
    fn read_stream_slow(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        let mut this = self.project();
        let mut pos = 0usize;

        stack_pin_init!(let free_list = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `free_list` is newly initialized and empty.
        let mut free_list = unsafe { MBufListGuard::new(free_list) };

        while pos < len
            && let Some(front) = this.buffers.front()
        {
            // SAFETY: `*this.read_cursor_off <= front.len as usize` by the `MBufChain`
            // `read_cursor_off` safety invariant.
            let copy_len = match unsafe {
                front.read_to_user(*this.read_cursor_off, dst.byte_offset(pos as isize), len - pos)
            } {
                Ok(n) => n,
                Err(err) => return (Err(err), pos),
            };

            pos += copy_len;
            *this.read_cursor_off += copy_len;
            *this.size -= copy_len;

            if *this.read_cursor_off == front.len as usize {
                if let Some(buf) = this.buffers.as_mut().pop_front() {
                    // SAFETY: `buf` was popped from `this.buffers` and is a valid, unlinked `MBuf`
                    // allocated from PMM.
                    unsafe {
                        free_list.push(buf);
                    }
                }
                *this.read_cursor_off = 0;
            }
        }

        (Ok(()), pos)
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
    #[inline]
    pub fn read_datagram(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        if self.size == 0 || len == 0 {
            return (Ok(()), 0);
        }

        if let Some(front) = self.buffers.front()
            && front.pkt_len <= front.len
        {
            // SAFETY: `0 <= front.len as usize`.
            let res = unsafe { front.read_to_user(0, dst, len) };
            let front_len = front.len as usize;
            let this = self.project();
            *this.size = this.size.wrapping_sub(front_len);
            // SAFETY: By the `MBufChain` `buffers` safety invariant, every element in
            // `this.buffers` is a valid, exclusively owned `*mut MBuf` allocated from PMM.
            unsafe {
                pop_and_free_front(this.buffers);
            }
            return (res.map(|_| ()), res.unwrap_or(0));
        }

        self.read_datagram_slow(dst, len)
    }

    #[cold]
    fn read_datagram_slow(
        self: Pin<&mut Self>,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        let len = min(len, self.datagram_size());
        let mut this = self.project();
        let mut pos = 0usize;

        stack_pin_init!(let free_list = DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `free_list` is newly initialized and empty.
        let mut free_list = unsafe { MBufListGuard::new(free_list) };

        let mut res = Ok(());
        while pos < len
            && let Some(front) = this.buffers.front()
        {
            // SAFETY: `0 <= front.len as usize`.
            let copy_res =
                unsafe { front.read_to_user(0, dst.byte_offset(pos as isize), len - pos) };

            // In datagram mode, each visited buffer is popped and discarded completely.
            *this.size -= front.len as usize;
            if let Some(buf) = this.buffers.as_mut().pop_front() {
                // SAFETY: `buf` was popped from `this.buffers` and is a valid, unlinked `MBuf`
                // allocated from PMM.
                unsafe {
                    free_list.push(buf);
                }
            }

            match copy_res {
                Ok(copy_len) => pos += copy_len,
                Err(err) => {
                    res = Err(err);
                    break;
                }
            }
        }

        // Drain any leftover mbufs in the datagram packet if we're consuming data, even if we fail
        // to read bytes.
        while let Some(front) = this.buffers.front()
            && front.pkt_len == 0
        {
            *this.size -= front.len as usize;
            if let Some(buf) = this.buffers.as_mut().pop_front() {
                // SAFETY: `buf` was popped from `this.buffers` and is a valid, unlinked `MBuf`
                // allocated from PMM.
                unsafe {
                    free_list.push(buf);
                }
            }
        }

        (res, pos)
    }

    /// Same as `read_stream()`/`read_datagram()` but leaves the bytes in the chain instead of
    /// consuming them, even if an error occurs.
    ///
    /// Peeks up to `len` bytes of stream data from the chain into `dst` without consuming them.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes peeked.
    pub fn peek_stream(&self, dst: UserOutPtr<c_char>, len: usize) -> (Result<(), Status>, usize) {
        let len = min(self.size, len);
        if len == 0 {
            return (Ok(()), 0);
        }

        let mut pos = 0usize;
        let mut read_off = self.read_cursor_off;

        for buf in self.buffers.iter() {
            if pos >= len {
                break;
            }
            // SAFETY: `read_off <= buf.len as usize` by the `MBufChain` `read_cursor_off` safety
            // invariant on the first iteration and `read_off == 0` on subsequent iterations.
            match unsafe { buf.read_to_user(read_off, dst.byte_offset(pos as isize), len - pos) } {
                Ok(n) => {
                    pos += n;
                    read_off = 0;
                }
                Err(err) => return (Err(err), pos),
            }
        }

        (Ok(()), pos)
    }

    /// Peeks at most one datagram from the chain into `dst` without consuming it.
    ///
    /// Returns `(res, actual)` indicating the operation status and number of bytes peeked.
    pub fn peek_datagram(
        &self,
        dst: UserOutPtr<c_char>,
        len: usize,
    ) -> (Result<(), Status>, usize) {
        if self.size == 0 || len == 0 {
            return (Ok(()), 0);
        }

        let len = min(len, self.datagram_size());
        self.peek_stream(dst, len)
    }

    /// Returns number of bytes stored in the chain.
    #[inline]
    pub fn stream_size(&self) -> usize {
        self.size
    }

    /// Returns number of bytes stored in the first datagram, or 0 if empty.
    #[inline]
    pub fn datagram_size(&self) -> usize {
        if let Some(front) = self.buffers.front() { front.pkt_len as usize } else { 0 }
    }

    /// Returns true if chain is full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.size >= Self::MAX_SIZE
    }

    /// Returns true if chain is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

#[pinned_drop]
impl PinnedDrop for MBufChain {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: By the `MBufChain` `buffers` safety invariant, every element in `self.buffers`
        // is a valid, exclusively owned `*mut MBuf` allocated from PMM.
        unsafe {
            free_mbufs(self.project().buffers);
        }
    }
}

/// In-tree kernel unit tests for `MBufChain`.
#[cfg(ktest)]
#[unittest::suite(name = "mbuf_rust")]
mod tests {
    use super::{MBuf, MBufChain, MBufListGuard, alloc_mbufs};
    use crate::user_copy::{UserInPtr, UserOutPtr};
    use crate::user_memory::UserMemory;
    use core::ffi::c_char;
    use core::mem::MaybeUninit;
    use core::pin::Pin;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true, unwrap_ok};
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
        let mem = UserMemory::create(alloc_size)?;
        mem.commit_and_map(0..alloc_size).ok()?;
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
        let mem = UserMemory::create(alloc_size)?;
        mem.commit_and_map(0..alloc_size).ok()?;
        let ptr = UserOutPtr::new(mem.base() as *mut c_char);
        Some((mem, ptr))
    }

    fn verify_user_mem(mem: &UserMemory, size: usize, pattern: impl Fn(usize) -> u8) -> bool {
        let mut chunk = [MaybeUninit::<u8>::uninit(); 512];
        let mut offset = 0;
        while offset < size {
            let to_read = core::cmp::min(chunk.len(), size - offset);
            let Ok(read_bytes) = mem.vmo_read(&mut chunk[..to_read], offset as u64) else {
                return false;
            };
            for (i, &b) in read_bytes.iter().enumerate() {
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
    fn read_helper<'a>(
        chain: &mut Pin<&mut MBufChain>,
        buf: &'a mut [MaybeUninit<u8>],
        len: usize,
        msg_type: MessageType,
        read_type: ReadType,
        actual: &mut usize,
    ) -> Result<&'a mut [u8], Status> {
        let (mem, dst) = make_user_out(len).ok_or(Status::NO_MEMORY)?;
        let (res, nread) = match (read_type, msg_type) {
            (ReadType::Read, MessageType::Datagram) => chain.as_mut().read_datagram(dst, len),
            (ReadType::Read, MessageType::Stream) => chain.as_mut().read_stream(dst, len),
            (ReadType::Peek, MessageType::Datagram) => chain.peek_datagram(dst, len),
            (ReadType::Peek, MessageType::Stream) => chain.peek_stream(dst, len),
        };
        *actual = nread;
        res?;
        if nread > 0 {
            let copy_len = core::cmp::min(nread, buf.len());
            mem.vmo_read(&mut buf[..copy_len], 0)
        } else {
            Ok(&mut [])
        }
    }

    /// Tests initial state of MBufChain.
    #[test]
    fn test_initial_state() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let chain = chain_pin.as_ref();
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.stream_size(), 0);
    }

    /// Tests reading stream when empty.
    #[test]
    fn test_stream_read_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut buf,
            1,
            MessageType::Stream,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
    }

    /// Tests reading stream with zero length.
    #[test]
    fn test_stream_read_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "x", MessageType::Stream));

        let mut buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut buf,
            0,
            MessageType::Stream,
            ReadType::Read,
            &mut actual
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
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
            expect_eq!(chain.stream_size(), (i + 1) * WRITE_LEN);
        }

        // Read it all back in one call.
        const TOTAL_LEN: usize = WRITE_LEN * NUM_WRITES;
        expect_eq!(chain.stream_size(), TOTAL_LEN);

        let (mem_out, dst) = make_user_out(TOTAL_LEN).unwrap();
        let (res, actual) = chain.as_mut().read_stream(dst, TOTAL_LEN);
        expect_ok!(res);
        expect_eq!(actual, TOTAL_LEN);
        expect_true!(chain.is_empty());
        expect_false!(chain.is_full());
        expect_eq!(chain.stream_size(), 0);

        // Verify result.
        expect_true!(verify_user_mem(&mem_out, TOTAL_LEN, |offset| {
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
        expect_eq!(chain.stream_size(), 0);
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
        expect_eq!(total_written, chain.stream_size());

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
        expect_eq!(chain.stream_size(), 0);
        expect_eq!(total_written, total_read);
    }

    /// Tests stream peeking data.
    #[test]
    fn test_stream_peek() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Stream));
        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Stream));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 10];
        let mut actual = 0;

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"a");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            4,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc1");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            6,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc123");

        expect_eq!(chain.stream_size(), 6);
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            6,
            MessageType::Stream,
            ReadType::Read,
            &mut actual,
        ));
        expect_true!(bytes == b"abc123");
    }

    /// Tests stream peeking when empty.
    #[test]
    fn test_stream_peek_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
    }

    /// Tests stream peeking zero length.
    #[test]
    fn test_stream_peek_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "a", MessageType::Stream));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
    }

    /// Tests stream peeking with underflow.
    #[test]
    fn test_stream_peek_underflow() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();

        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Stream));
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 10];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");

        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Stream));
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Stream,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc123");
    }

    /// Tests datagram reading when empty.
    #[test]
    fn test_datagram_read_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
        expect_true!(chain.is_empty());
    }

    /// Tests datagram reading zero length.
    #[test]
    fn test_datagram_read_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "x", MessageType::Datagram));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
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
        expect_eq!(chain.stream_size(), WRITE_LEN);
        expect_false!(chain.is_empty());

        let (_mem_b, src_b) = make_user_in_byte(WRITE_LEN, b'B').unwrap();
        let (res_b, written_b) = chain.as_mut().write_datagram(src_b, WRITE_LEN);
        expect_ok!(res_b);
        expect_eq!(written_b, WRITE_LEN);
        expect_eq!(chain.stream_size(), 2 * WRITE_LEN);
        expect_false!(chain.is_empty());

        let mut read_buf = [MaybeUninit::<u8>::uninit(); WRITE_LEN];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(actual, 1);
        expect_eq!(bytes[0], b'A');
        expect_false!(chain.is_empty());

        expect_eq!(chain.stream_size(), WRITE_LEN);
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            WRITE_LEN,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(actual, WRITE_LEN);
        expect_true!(chain.is_empty());
        expect_eq!(chain.stream_size(), 0);
        expect_true!(bytes.iter().all(|&b| b == b'B'));
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
        expect_eq!(chain.datagram_size(), 1);
        expect_eq!(chain.stream_size(), total_written);

        // Read them back and verify their contents.
        for i in 1..=NUM_DATAGRAMS {
            expect_eq!(chain.datagram_size(), i);
            let mut read_buf = [MaybeUninit::<u8>::uninit(); 100];
            let mut actual = 0;
            let bytes = unwrap_ok!(read_helper(
                &mut chain,
                &mut read_buf[..i],
                i,
                MessageType::Datagram,
                ReadType::Read,
                &mut actual,
            ));
            expect_eq!(actual, i);
            expect_true!(bytes.iter().all(|&b| b == (i as u8)));
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.stream_size(), 0);
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
        expect_eq!(chain.datagram_size(), 0);
        expect_eq!(chain.stream_size(), 0);
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
        expect_eq!(chain.stream_size(), WRITE_LEN * num_datagrams_written);

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
        expect_eq!(chain.stream_size(), 0);
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
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(bytes[0], b'a');
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(bytes[0], b'b');

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
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(bytes[0], b'c');

        // Reading again should give us the second datagram we wrote, as the remaining of the first
        // should have been discarded.
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_eq!(bytes[0], b'd');

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

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 10];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"a");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");

        // Make sure peeking didn't affect an actual read.
        expect_eq!(chain.stream_size(), 3);
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");
    }

    /// Tests datagram peeking empty.
    #[test]
    fn test_datagram_peek_empty() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            1,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
    }

    /// Tests datagram peeking zero length.
    #[test]
    fn test_datagram_peek_zero() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "a", MessageType::Datagram));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 1];
        let mut actual = 0;
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            0,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_eq!(actual, 0);
        expect_true!(bytes.is_empty());
    }

    /// Tests datagram peeking underflow.
    #[test]
    fn test_datagram_peek_underflow() {
        stack_pin_init!(let chain_pin = MBufChain::new());
        let mut chain = chain_pin.as_mut();
        expect_true!(write_helper(chain.as_mut(), "abc", MessageType::Datagram));
        expect_true!(write_helper(chain.as_mut(), "123", MessageType::Datagram));

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 10];
        let mut actual = 0;

        // Datagram peeks should not return more than a single message.
        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            3,
            MessageType::Datagram,
            ReadType::Read,
            &mut actual,
        ));
        expect_true!(bytes == b"abc");

        let bytes = unwrap_ok!(read_helper(
            &mut chain,
            &mut read_buf,
            10,
            MessageType::Datagram,
            ReadType::Peek,
            &mut actual,
        ));
        expect_true!(bytes == b"123");
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
        expect_eq!(chain.stream_size(), LARGE_PAYLOAD);
        expect_eq!(chain.datagram_size(), LARGE_PAYLOAD);

        // Read only enough to span into the second buffer, leaving the third buffer untouched.
        const READ_LEN: usize = MBuf::PAYLOAD_SIZE + 20;
        let (mem_out, dst) = make_user_out(READ_LEN).unwrap();
        let (res_r, actual) = chain.as_mut().read_datagram(dst, READ_LEN);
        expect_ok!(res_r);
        expect_eq!(actual, READ_LEN);
        expect_true!(verify_user_mem(&mem_out, READ_LEN, |_| b'Z'));

        // All remaining bytes (including continuation buffer 3) should have been discarded.
        expect_true!(chain.is_empty());
        expect_eq!(chain.stream_size(), 0);
        expect_eq!(chain.datagram_size(), 0);
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
        expect_eq!(chain.stream_size(), TOTAL_BYTES);

        let mut total_read = 0usize;
        let chunk_sizes = [100, MBuf::PAYLOAD_SIZE - 50, 500, MBuf::PAYLOAD_SIZE, 1000];

        for &chunk in &chunk_sizes {
            if total_read >= TOTAL_BYTES {
                break;
            }
            let to_read = core::cmp::min(chunk, TOTAL_BYTES - total_read);
            let (mem_out, dst) = make_user_out(to_read).unwrap();
            let (res_r, actual) = chain.as_mut().read_stream(dst, to_read);
            expect_ok!(res_r);
            expect_eq!(actual, to_read);
            expect_true!(verify_user_mem(&mem_out, actual, |i| { ((total_read + i) % 251) as u8 }));
            total_read += actual;
            expect_eq!(chain.stream_size(), TOTAL_BYTES - total_read);
        }

        // Read the remaining bytes.
        if total_read < TOTAL_BYTES {
            let rem = TOTAL_BYTES - total_read;
            let (mem_rem, rem_dst) = make_user_out(rem).unwrap();
            let (res_rem, actual) = chain.as_mut().read_stream(rem_dst, rem);
            expect_ok!(res_rem);
            expect_eq!(actual, rem);
            expect_true!(verify_user_mem(&mem_rem, actual, |i| { ((total_read + i) % 251) as u8 }));
        }

        expect_true!(chain.is_empty());
        expect_eq!(chain.stream_size(), 0);
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
        expect_eq!(chain.stream_size(), FILL_SIZE);

        // Attempting to write a 101-byte datagram exceeds MAX_SIZE.
        let (_overflow_mem, overflow_src) = make_user_in_byte(101, b'V').unwrap();
        let (res_overflow, overflow_written) = chain.as_mut().write_datagram(overflow_src, 101);
        expect_true!(res_overflow == Err(Status::SHOULD_WAIT));
        expect_eq!(overflow_written, 0);
        expect_eq!(chain.stream_size(), FILL_SIZE);
    }

    /// Tests that MBuf allocation, insertion, removal, and freeing preserves node state.
    #[test]
    fn test_mbuf_alloc_free_node_lifecycle() {
        stack_pin_init!(let bufs = fbl::DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is newly initialized and empty.
        let mut bufs = unsafe { MBufListGuard::new(bufs) };

        expect_ok!(alloc_mbufs(10, bufs.as_mut()));

        stack_pin_init!(let other = fbl::DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `other` is newly initialized and empty.
        let mut other = unsafe { MBufListGuard::new(other) };

        while let Some(buf) = bufs.as_mut().pop_front() {
            // SAFETY: `buf` was popped from `bufs` and is a valid, unlinked `MBuf` allocated from
            // PMM.
            unsafe {
                other.push(buf);
            }
        }

        expect_true!(bufs.as_mut().is_empty());
        expect_false!(other.as_mut().is_empty());
    }

    /// Tests allocating and freeing MBufs using alloc_mbufs and free_mbufs helpers.
    #[test]
    fn test_alloc_free_mbufs() {
        stack_pin_init!(let bufs = fbl::DoublyLinkedList::<*mut MBuf>::new());
        // SAFETY: `bufs` is newly initialized and empty.
        let mut bufs = unsafe { MBufListGuard::new(bufs) };

        expect_ok!(alloc_mbufs(5, bufs.as_mut()));
        expect_false!(bufs.as_mut().is_empty());
    }
}
