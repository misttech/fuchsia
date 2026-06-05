// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fuchsia netdevice buffer pool.

use fuchsia_sync::Mutex;
use futures::task::AtomicWaker;
use std::borrow::Borrow;
use std::collections::VecDeque;
use std::convert::TryInto as _;
use std::fmt::Debug;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::num::TryFromIntError;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool, AtomicU64};
use std::task::Poll;

use arrayvec::ArrayVec;
use explicit::ResultExt as _;
use fidl_fuchsia_hardware_network as netdev;
use fuchsia_runtime::vmar_root_self;
use futures::channel::oneshot::{Receiver, Sender, channel};

use super::{ChainLength, DescId, DescRef, DescRefMut, Descriptors};
use crate::error::{Error, Result};
use crate::session::{BufferLayout, Config, Pending, Port};

/// Responsible for managing [`Buffer`]s for a [`Session`](crate::session::Session).
pub(in crate::session) struct Pool {
    /// Base address of the pool.
    // Note: This field requires us to manually implement `Sync` and `Send`.
    base: NonNull<u8>,
    /// The length of the pool in bytes.
    bytes: usize,
    /// The descriptors allocated for the pool.
    descriptors: Descriptors,
    /// Shared state for allocation.
    tx_alloc_state: Mutex<TxAllocState>,
    /// The free rx descriptors pending to be sent to driver.
    pub(in crate::session) rx_pending: Pending<Rx>,
    /// The buffer layout.
    buffer_layout: BufferLayout,
    /// State-keeping allowing sessions to handle rx leases.
    rx_leases: RxLeaseHandlingState,
}

// `Pool` is `Send` and `Sync`, and this allows the compiler to deduce `Buffer`
// to be `Send`. These impls are safe because we can safely share `Pool` and
// `&Pool`: the implementation would never allocate the same buffer to two
// callers at the same time.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

/// The shared state which keeps track of available buffers and tx buffers.
struct TxAllocState {
    /// All pending tx allocation requests.
    requests: VecDeque<TxAllocReq>,
    free_list: TxFreeList,
}

/// We use a linked list to maintain the tx free descriptors - they are linked
/// through their `nxt` fields, note this differs from the chaining expected
/// by the network device protocol:
/// - You can chain more than [`netdev::MAX_DESCRIPTOR_CHAIN`] descriptors
///   together.
/// - the free-list ends when the `nxt` field is 0xff, while the normal chain
///   ends when `chain_length` becomes 0.
struct TxFreeList {
    /// The head of a linked list of available descriptors that can be allocated
    /// for tx.
    head: Option<DescId<Tx>>,
    /// How many free descriptors are there in the pool.
    len: u16,
}

impl Pool {
    /// Creates a new [`Pool`] and its backing [`zx::Vmo`]s.
    ///
    /// Returns [`Pool`] and the [`zx::Vmo`]s for descriptors and data, in that
    /// order.
    pub(in crate::session) fn new(config: Config) -> Result<(Arc<Self>, zx::Vmo, zx::Vmo)> {
        let Config { buffer_stride, num_rx_buffers, num_tx_buffers, options, buffer_layout } =
            config;
        let num_buffers = num_rx_buffers.get() + num_tx_buffers.get();
        let (descriptors, descriptors_vmo, tx_free, mut rx_free) =
            Descriptors::new(num_tx_buffers, num_rx_buffers, buffer_stride)?;

        // Construct the free list.
        let free_head = tx_free.into_iter().rev().fold(None, |head, mut curr| {
            descriptors.borrow_mut(&mut curr).set_nxt(head);
            Some(curr)
        });

        for rx_desc in rx_free.iter_mut() {
            descriptors.borrow_mut(rx_desc).initialize(
                ChainLength::ZERO,
                0,
                buffer_layout.length.try_into().unwrap(),
                0,
            );
        }

        let tx_alloc_state = TxAllocState {
            free_list: TxFreeList { head: free_head, len: num_tx_buffers.get() },
            requests: VecDeque::new(),
        };

        let size = buffer_stride.get() * u64::from(num_buffers);
        let data_vmo = zx::Vmo::create(size).map_err(|status| Error::Vmo("data", status))?;

        const VMO_NAME: zx::Name =
            const_unwrap::const_unwrap_result(zx::Name::new("netdevice:data"));
        data_vmo.set_name(&VMO_NAME).map_err(|status| Error::Vmo("set name", status))?;
        // `as` is OK because `size` is positive and smaller than isize::MAX.
        // This is following the practice of rust stdlib to ensure allocation
        // size never reaches isize::MAX.
        // https://doc.rust-lang.org/std/primitive.pointer.html#method.add-1.
        let len = isize::try_from(size).expect("VMO size larger than isize::MAX") as usize;
        // The returned address of zx_vmar_map on success must be non-zero:
        // https://fuchsia.dev/fuchsia-src/reference/syscalls/vmar_map
        let base = NonNull::new(
            vmar_root_self()
                .map(0, &data_vmo, 0, len, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
                .map_err(|status| Error::Map("data", status))? as *mut u8,
        )
        .unwrap();

        Ok((
            Arc::new(Pool {
                base,
                bytes: len,
                descriptors,
                tx_alloc_state: Mutex::new(tx_alloc_state),
                rx_pending: Pending::new(rx_free),
                buffer_layout,
                rx_leases: RxLeaseHandlingState::new_with_flags(options),
            }),
            descriptors_vmo,
            data_vmo,
        ))
    }

    /// Allocates `num_parts` tx descriptors.
    ///
    /// It will block if there are not enough descriptors. Note that the
    /// descriptors are not initialized, you need to call [`AllocGuard::init()`]
    /// on the returned [`AllocGuard`] if you want to send it to the driver
    /// later. See [`AllocGuard<Rx>::into_tx()`] for an example where
    /// [`AllocGuard::init()`] is not needed because the tx allocation will be
    /// returned to the pool immediately and won't be sent to the driver.
    pub(in crate::session) async fn alloc_tx(
        self: &Arc<Self>,
        num_parts: ChainLength,
    ) -> AllocGuard<Tx> {
        let receiver = {
            let mut state = self.tx_alloc_state.lock();
            match state.free_list.try_alloc(num_parts, &self.descriptors) {
                Some(allocated) => {
                    return AllocGuard::new(allocated, self.clone());
                }
                None => {
                    let (request, receiver) = TxAllocReq::new(num_parts);
                    state.requests.push_back(request);
                    receiver
                }
            }
        };
        // The sender must not be dropped.
        receiver.await.unwrap()
    }

    /// Tries to allocate a [`SinglePartTxBuffer`].
    ///
    /// Returns `Ok(None)` if there is no available buffer, or `Err(Error::TxLength)`
    /// if the requested size cannot meet the device requirement.
    pub(in crate::session) fn try_alloc_single_part_tx_buffer(
        self: &Arc<Self>,
        num_bytes: usize,
    ) -> Result<Option<SinglePartTxBuffer>> {
        let BufferLayout { min_tx_data: _, min_tx_head, min_tx_tail, length: buffer_length } =
            self.buffer_layout;
        if num_bytes > buffer_length - usize::from(min_tx_head) - usize::from(min_tx_tail) {
            return Err(Error::TxLength);
        }
        self.tx_alloc_state
            .lock()
            .free_list
            .try_alloc(ChainLength::try_from(1u8).unwrap(), &self.descriptors)
            .map(|allocated| -> Result<SinglePartTxBuffer> {
                let mut alloc = AllocGuard::new(allocated, self.clone());
                alloc.init(num_bytes)?;
                let buffer = Buffer::from(alloc);
                Ok(SinglePartTxBuffer::new(buffer, num_bytes).expect("must be single part"))
            })
            .transpose()
    }

    /// Allocates a tx [`Buffer`].
    ///
    /// The returned buffer will have `num_bytes` as its capacity, the method
    /// will block if there are not enough buffers. An error will be returned if
    /// the requested size cannot meet the device requirement, for example, if
    /// the size of the head or tail region will become unrepresentable in u16.
    pub(in crate::session) async fn alloc_tx_buffer(
        self: &Arc<Self>,
        num_bytes: usize,
    ) -> Result<Buffer<Tx>> {
        self.alloc_tx_buffers(num_bytes).await?.next().unwrap()
    }

    /// Waits for at least one TX buffer to be available and returns an iterator
    /// of buffers with `num_bytes` as capacity.
    ///
    /// The returned iterator is guaranteed to yield at least one item (though
    /// it might be an error if the requested size cannot meet the device
    /// requirement).
    ///
    /// # Note
    ///
    /// Given a `Buffer<Tx>` is returned to the pool when it's dropped, the
    /// returned iterator will seemingly yield infinite items if the yielded
    /// `Buffer`s are dropped while iterating.
    pub(in crate::session) async fn alloc_tx_buffers<'a>(
        self: &'a Arc<Self>,
        num_bytes: usize,
    ) -> Result<impl Iterator<Item = Result<Buffer<Tx>>> + 'a> {
        let BufferLayout { min_tx_data, min_tx_head, min_tx_tail, length: buffer_length } =
            self.buffer_layout;
        let tx_head = usize::from(min_tx_head);
        let tx_tail = usize::from(min_tx_tail);
        let total_bytes = num_bytes.max(min_tx_data) + tx_head + tx_tail;
        let num_parts = (total_bytes + buffer_length - 1) / buffer_length;
        let chain_length = ChainLength::try_from(num_parts)?;
        let first = self.alloc_tx(chain_length).await;
        let iter = std::iter::once(first)
            .chain(std::iter::from_fn(move || {
                let mut state = self.tx_alloc_state.lock();
                state
                    .free_list
                    .try_alloc(chain_length, &self.descriptors)
                    .map(|allocated| AllocGuard::new(allocated, self.clone()))
            }))
            // Fuse afterwards so we're guaranteeing we can't see a new entry
            // after having yielded `None` once.
            .fuse()
            .map(move |mut alloc| {
                alloc.init(num_bytes)?;
                Ok(alloc.into())
            });
        Ok(iter)
    }

    /// Frees rx descriptors.
    pub(in crate::session) fn free_rx(&self, descs: impl IntoIterator<Item = DescId<Rx>>) {
        self.rx_pending.extend(descs.into_iter().map(|mut desc| {
            self.descriptors.borrow_mut(&mut desc).initialize(
                ChainLength::ZERO,
                0,
                self.buffer_layout.length.try_into().unwrap(),
                0,
            );
            desc
        }));
    }

    /// Frees tx descriptors.
    ///
    /// # Panics
    ///
    /// Panics if given an empty chain.
    fn free_tx(self: &Arc<Self>, chain: Chained<DescId<Tx>>) {
        // We store any pending request that need to be fulfilled in the stack
        // here, to fulfill them only once we drop the lock, guaranteeing an
        // AllocGuard can't be dropped while the lock is held.
        let mut to_fulfill = ArrayVec::<
            (TxAllocReq, AllocGuard<Tx>),
            { netdev::MAX_DESCRIPTOR_CHAIN as usize },
        >::new();

        let mut state = self.tx_alloc_state.lock();

        {
            let mut descs = chain.into_iter();
            // The following can't overflow because we can have at most u16::MAX
            // descriptors: free_len + #(to_free) + #(descs in use) <= u16::MAX,
            // Thus free_len + #(to_free) <= u16::MAX.
            state.free_list.len += u16::try_from(descs.len()).unwrap();
            let head = descs.next();
            let old_head = std::mem::replace(&mut state.free_list.head, head);
            let mut tail = descs.last();
            let mut tail_ref = self.descriptors.borrow_mut(
                tail.as_mut().unwrap_or_else(|| state.free_list.head.as_mut().unwrap()),
            );
            tail_ref.set_nxt(old_head);
        }

        // After putting the chain back into the free list, we try to fulfill
        // any pending tx allocation requests.
        while let Some(req) = state.requests.front() {
            // Skip a request that we know is canceled.
            //
            // This is an optimization for long-ago dropped requests, since the
            // receiver side can be dropped between here and fulfillment later.
            if req.sender.is_canceled() {
                let _cancelled: Option<TxAllocReq> = state.requests.pop_front();
                continue;
            }
            let size = req.size;
            match state.free_list.try_alloc(size, &self.descriptors) {
                Some(descs) => {
                    // The unwrap is safe because we know requests is not empty.
                    let req = state.requests.pop_front().unwrap();
                    to_fulfill.push((req, AllocGuard::new(descs, self.clone())));

                    // If we're full temporarily release the lock to go again
                    // later. Fulfilling a request must _always_ be done without
                    // holding the lock.
                    if to_fulfill.is_full() {
                        drop(state);
                        for (req, alloc) in to_fulfill.drain(..) {
                            req.fulfill(alloc)
                        }
                        state = self.tx_alloc_state.lock();
                    }
                }
                None => break,
            }
        }

        // Make sure we're not holding the state lock when fulfilling requests.
        drop(state);
        // Fulfill any ready requests.
        for (req, alloc) in to_fulfill {
            req.fulfill(alloc)
        }
    }

    /// Frees the completed tx descriptors chained by head to the pool.
    ///
    /// Call this function when the driver hands back a completed tx descriptor.
    pub(in crate::session) fn tx_completed(self: &Arc<Self>, head: DescId<Tx>) -> Result<()> {
        let chain = self.descriptors.chain(head).collect::<Result<Chained<_>>>()?;
        Ok(self.free_tx(chain))
    }

    /// Creates a [`Buffer<Rx>`] corresponding to the completed rx descriptors.
    ///
    /// Whenever the driver hands back a completed rx descriptor, this function
    /// can be used to create the buffer that is represented by those chained
    /// descriptors.
    pub(in crate::session) fn rx_completed(
        self: &Arc<Self>,
        head: DescId<Rx>,
    ) -> Result<Buffer<Rx>> {
        let descs = self.descriptors.chain(head).collect::<Result<Chained<_>>>()?;
        let alloc = AllocGuard::new(descs, self.clone());
        Ok(alloc.into())
    }

    fn get_slice<'a, K: AllocKind>(&self, desc: &'a DescId<K>) -> &'a [u8] {
        let desc = self.descriptors.borrow(desc);
        let offset = usize::try_from(desc.offset() + u64::from(desc.head_length()))
            .expect("usize must hold u64");
        let len = usize::try_from(desc.data_length()).expect("usize must hold u32");
        // Safety: The descriptor is describing a buffer from this pool. It must
        // be valid to create a slice into that region. We hold a immutable
        // reference to the underlying descriptor, this means no one else should
        // have mutable reference to this memory region.
        unsafe {
            let ptr = self.base.as_ptr().add(offset);
            std::slice::from_raw_parts(ptr, len)
        }
    }

    fn get_slice_mut<'a, K: AllocKind>(&self, desc: &'a mut DescId<K>) -> &'a mut [u8] {
        let desc = self.descriptors.borrow_mut(desc);
        let offset = usize::try_from(desc.offset() + u64::from(desc.head_length()))
            .expect("usize must hold u64");
        let len = usize::try_from(desc.data_length()).expect("usize must hold u32");
        // Safety: The descriptor is describing a buffer from this pool. It must
        // be valid to create a slice into that region. We hold a mutable
        // reference to the underlying descriptor, this means we are currently
        // the only one has access to this memory region.
        unsafe {
            let ptr = self.base.as_ptr().add(offset);
            std::slice::from_raw_parts_mut(ptr, len)
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            vmar_root_self()
                .unmap(self.base.as_ptr() as usize, self.bytes)
                .expect("failed to unmap VMO for Pool")
        }
    }
}

impl TxFreeList {
    /// Tries to allocate tx descriptors.
    ///
    /// Returns [`None`] if there are not enough descriptors.
    fn try_alloc(
        &mut self,
        num_parts: ChainLength,
        descriptors: &Descriptors,
    ) -> Option<Chained<DescId<Tx>>> {
        if u16::from(num_parts.get()) > self.len {
            return None;
        }

        let free_list = std::iter::from_fn(|| -> Option<DescId<Tx>> {
            let new_head = self.head.as_ref().and_then(|head| {
                let nxt = descriptors.borrow(head).nxt();
                nxt.map(|id| unsafe {
                    // Safety: This is the nxt field of head of the free list,
                    // it must be a tx descriptor id.
                    DescId::from_raw(id)
                })
            });
            std::mem::replace(&mut self.head, new_head)
        });
        let allocated = free_list.take(num_parts.get().into()).collect::<Chained<_>>();
        assert_eq!(allocated.len(), num_parts.into());
        self.len -= u16::from(num_parts.get());
        Some(allocated)
    }
}

/// The buffer that can be used by the [`Session`](crate::session::Session).
pub struct Buffer<K: AllocKind> {
    /// The descriptors allocation.
    alloc: AllocGuard<K>,
}

impl<K: AllocKind> Buffer<K> {
    /// Returns the length of data region of the buffer.
    pub fn len(&self) -> usize {
        self.parts().map(|s| s.len()).sum()
    }

    /// Returns an iterator over the data slices of the buffer parts.
    fn parts(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.alloc.descs.iter().map(|desc| self.alloc.pool.get_slice(desc))
    }

    /// Returns an iterator over the mutable valid data slices of the buffer parts.
    fn parts_mut(&mut self) -> impl Iterator<Item = &mut [u8]> + '_ {
        self.alloc.descs.iter_mut().map(|desc| self.alloc.pool.get_slice_mut(desc))
    }

    /// Leaks the underlying buffer descriptors to the driver.
    pub(in crate::session) fn leak(mut self) -> DescId<K> {
        let descs = std::mem::replace(&mut self.alloc.descs, Chained::empty());
        descs.into_iter().next().unwrap()
    }

    /// Retrieves the frame type of the buffer.
    pub fn frame_type(&self) -> Result<netdev::FrameType> {
        self.alloc.descriptor().frame_type()
    }

    /// Retrieves the buffer's source port.
    pub fn port(&self) -> Port {
        self.alloc.descriptor().port()
    }

    /// Returns the buffer data as a slice.
    pub fn as_slice(&self) -> Option<&[u8]> {
        if self.alloc.len() != 1 {
            return None;
        }
        self.parts().next()
    }

    /// Returns the buffer data as a mutable slice.
    pub fn as_slice_mut(&mut self) -> Option<&mut [u8]> {
        if self.alloc.len() != 1 {
            return None;
        }
        self.parts_mut().next()
    }

    /// Returns a wrapper for read-only operations.
    pub fn io(&self) -> BufferIORef<'_, K> {
        let mut len = 0;
        let parts: Chained<&[u8]> = self.parts().inspect(|s| len += s.len()).collect();
        BufferIO { parts, pos: 0, len, _marker: std::marker::PhantomData }
    }

    /// Returns a wrapper for read-write operations.
    pub fn io_mut(&mut self) -> BufferIOMut<'_, K> {
        let mut len = 0;
        let parts: Chained<&mut [u8]> = self.parts_mut().inspect(|s| len += s.len()).collect();
        BufferIO { parts, pos: 0, len, _marker: std::marker::PhantomData }
    }
}

impl Buffer<Tx> {
    /// Sets the buffer's destination port.
    pub fn set_port(&mut self, port: Port) {
        self.alloc.descriptor_mut().set_port(port)
    }

    /// Sets the frame type of the buffer.
    pub fn set_frame_type(&mut self, frame_type: netdev::FrameType) {
        self.alloc.descriptor_mut().set_frame_type(frame_type)
    }

    /// Sets TxFlags of a Tx buffer.
    pub fn set_tx_flags(&mut self, flags: netdev::TxFlags) {
        self.alloc.descriptor_mut().set_tx_flags(flags)
    }

    /// Shrinks the buffer.
    ///
    /// This method shrinks the buffer length to the larger of
    ///   - requested new length
    ///   - device required minimum Tx data length
    ///
    /// It is an error to try to increase the buffer length.
    pub fn shrink_to(&mut self, mut new_len: usize) -> Result<()> {
        let current_len = self.len();

        if new_len > current_len {
            return Err(Error::TxLength);
        }

        let min_tx_data = usize::from(self.alloc.pool.buffer_layout.min_tx_data);
        new_len = new_len.max(min_tx_data);

        let layouts = self.alloc.calculate_descriptor_layouts(new_len)?;

        for (desc_id, DescriptorLayout { data_length, tail_length, .. }) in
            self.alloc.descs.iter_mut().zip(layouts)
        {
            let mut descriptor = self.alloc.pool.descriptors.borrow_mut(desc_id);
            descriptor.set_data_length(data_length);
            descriptor.set_tail_length(tail_length);
        }
        Ok(())
    }
}

impl Buffer<Rx> {
    /// Turns an rx buffer into a tx one.
    pub async fn into_tx(self) -> Buffer<Tx> {
        let Buffer { alloc } = self;
        Buffer { alloc: alloc.into_tx().await }
    }

    /// Retrieves RxFlags of an Rx Buffer.
    pub fn rx_flags(&self) -> Result<netdev::RxFlags> {
        self.alloc.descriptor().rx_flags()
    }
}

impl<K: AllocKind> Debug for Buffer<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { alloc } = self;
        f.debug_struct("Buffer").field("alloc", alloc).finish()
    }
}

/// A witness type that proves the buffer is backed by one part only
/// and thus can be converted into `&[u8]`.
pub struct SinglePartTxBuffer(Buffer<Tx>);

impl SinglePartTxBuffer {
    /// Creates a new [`SinglePartTxBuffer`] from a [`Buffer<Tx>`] if it is
    /// backed by one part only.
    pub fn new(buffer: Buffer<Tx>, len: usize) -> Option<Self> {
        if buffer.alloc.len() != 1 {
            None
        } else {
            let cap = usize::try_from(buffer.alloc.descriptor().data_length())
                .expect("u32 must fit in a usize");
            if cap < len {
                return None;
            }
            Some(Self(buffer))
        }
    }

    /// Converts back to a Tx buffer.
    pub fn into_inner(self) -> Buffer<Tx> {
        let Self(buffer) = self;
        buffer
    }
}

impl AsRef<[u8]> for SinglePartTxBuffer {
    fn as_ref(&self) -> &[u8] {
        // Safety: `SinglePartTxBuffer` is guaranteed to have exactly one part
        // (verified on creation), so the first descriptor is always initialized.
        let desc = unsafe { self.0.alloc.descs.storage[0].assume_init_ref() };
        self.0.alloc.pool.get_slice(desc)
    }
}

impl AsMut<[u8]> for SinglePartTxBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // Safety: `SinglePartTxBuffer` is guaranteed to have exactly one part
        // (verified on creation), so the first descriptor is always initialized.
        let desc = unsafe { self.0.alloc.descs.storage[0].assume_init_mut() };
        self.0.alloc.pool.get_slice_mut(desc)
    }
}

impl packet::FragmentedBuffer for SinglePartTxBuffer {
    fn len(&self) -> usize {
        let desc = self.0.alloc.descriptor();
        usize::try_from(desc.data_length()).expect("u32 must fit in a usize")
    }

    fn with_bytes<'a, R, F>(&'a self, f: F) -> R
    where
        F: for<'b> FnOnce(packet::FragmentedBytes<'b, 'a>) -> R,
    {
        f(packet::FragmentedBytes::new(&mut [self.as_ref()][..]))
    }
}

/// A wrapper around [`Buffer`] for sequential I/O.
///
/// `T` must be a slice reference type, typically `&'a [u8]` for read-only
/// operations, or `&'a mut [u8]` for read-write operations.
pub struct BufferIO<T, K: AllocKind> {
    parts: Chained<T>,
    pos: usize,
    len: usize,
    _marker: std::marker::PhantomData<K>,
}

pub type BufferIORef<'a, K> = BufferIO<&'a [u8], K>;
pub type BufferIOMut<'a, K> = BufferIO<&'a mut [u8], K>;

impl<T> BufferIO<T, Tx>
where
    T: AsMut<[u8]>,
{
    /// Writes data from `src` into the TX buffer starting at the specified `offset`.
    ///
    /// This method is infallible. It returns the number of bytes successfully written.
    ///
    /// If the specified `offset` is greater than or equal to the total length of the
    /// buffer, or if the buffer has no remaining capacity at the offset, `0` bytes
    /// will be written.
    ///
    /// If `src` is larger than the remaining capacity of the buffer starting at
    /// `offset`, a short write occurs: only the bytes that fit within the buffer
    /// are written, and the returned value will be less than `src.len()`.
    pub fn write_at(&mut self, mut offset: usize, src: &[u8]) -> usize {
        let mut total = 0;

        for slice in self.parts.iter_mut() {
            let slice = slice.as_mut();
            if offset < slice.len() {
                let available = slice.len() - offset;
                let to_copy = std::cmp::min(src.len() - total, available);
                slice[offset..offset + to_copy].copy_from_slice(&src[total..total + to_copy]);
                total += to_copy;
                offset = 0;
                if total == src.len() {
                    break;
                }
            } else {
                offset -= slice.len();
            }
        }
        total
    }
}

impl<T, K: AllocKind> BufferIO<T, K>
where
    T: AsRef<[u8]>,
{
    /// Reads data from the buffer starting at the specified `offset` into `dst`.
    ///
    /// This method is infallible. It returns the number of bytes successfully read.
    ///
    /// If the specified `offset` is greater than or equal to the total length of the
    /// buffer, `0` bytes will be read.
    ///
    /// If the remaining data in the buffer starting at `offset` is less than the
    /// size of `dst`, a short read occurs: only the available bytes are copied,
    /// and the returned value will be less than `dst.len()`.
    pub fn read_at(&self, mut offset: usize, dst: &mut [u8]) -> usize {
        let mut total = 0;

        for slice in self.parts.iter() {
            let slice = slice.as_ref();
            if offset < slice.len() {
                let available = slice.len() - offset;
                let to_copy = std::cmp::min(dst.len() - total, available);
                dst[total..total + to_copy].copy_from_slice(&slice[offset..offset + to_copy]);
                total += to_copy;
                offset = 0;
                if total == dst.len() {
                    break;
                }
            } else {
                offset -= slice.len();
            }
        }
        total
    }
}

impl AllocGuard<Rx> {
    /// Turns a tx allocation into an rx one.
    ///
    /// To achieve this we have to convert the same amount of descriptors from
    /// the tx pool to the rx pool to compensate for us being converted to tx
    /// descriptors from rx ones.
    async fn into_tx(mut self) -> AllocGuard<Tx> {
        let mut tx = self.pool.alloc_tx(self.descs.len).await;
        // [MaybeUninit<DescId<Tx>; 4] and [MaybeUninit<DescId<Rx>; 4] have the
        // same memory layout because DescId is repr(transparent). So it is safe
        // to transmute and swap the values between the storages. After the swap
        // the drop implementation of self will return the descriptors back to
        // rx pool.
        std::mem::swap(&mut self.descs.storage, unsafe {
            std::mem::transmute(&mut tx.descs.storage)
        });
        tx
    }
}

/// A non-empty container that has at most [`netdev::MAX_DESCRIPTOR_CHAIN`] elements.
struct Chained<T> {
    storage: [MaybeUninit<T>; netdev::MAX_DESCRIPTOR_CHAIN as usize],
    len: ChainLength,
}

impl<T> Deref for Chained<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        // Safety: `self.storage[..self.len]` is already initialized.
        unsafe { std::mem::transmute(&self.storage[..self.len.into()]) }
    }
}

impl<T> DerefMut for Chained<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: `self.storage[..self.len]` is already initialized.
        unsafe { std::mem::transmute(&mut self.storage[..self.len.into()]) }
    }
}

impl<T> Drop for Chained<T> {
    fn drop(&mut self) {
        // Safety: `self.deref_mut()` is a slice of all initialized elements.
        unsafe {
            std::ptr::drop_in_place(self.deref_mut());
        }
    }
}

impl<T: Debug> Debug for Chained<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> Chained<T> {
    #[allow(clippy::uninit_assumed_init)]
    fn empty() -> Self {
        // Create an uninitialized array of `MaybeUninit`. The `assume_init` is
        // safe because the type we are claiming to have initialized here is a
        // bunch of `MaybeUninit`s, which do not require initialization.
        // TODO(https://fxbug.dev/42160423): use MaybeUninit::uninit_array once it
        // is stablized.
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#method.uninit_array
        Self { storage: unsafe { MaybeUninit::uninit().assume_init() }, len: ChainLength::ZERO }
    }
}

impl<T> FromIterator<T> for Chained<T> {
    /// # Panics
    ///
    /// if the iterator can yield more than MAX_DESCRIPTOR_CHAIN elements.
    fn from_iter<I: IntoIterator<Item = T>>(elements: I) -> Self {
        let mut result = Self::empty();
        let mut len = 0u8;
        for (idx, e) in elements.into_iter().enumerate() {
            result.storage[idx] = MaybeUninit::new(e);
            len += 1;
        }
        // `len` can not be larger than `MAX_DESCRIPTOR_CHAIN`, otherwise we can't
        // get here due to the bound checks on `result.storage`.
        result.len = ChainLength::try_from(len).unwrap();
        result
    }
}

impl<T> IntoIterator for Chained<T> {
    type Item = T;
    type IntoIter = ChainedIter<T>;

    fn into_iter(mut self) -> Self::IntoIter {
        let len = self.len;
        self.len = ChainLength::ZERO;
        // Safety: we have reset the length to zero, it is now safe to move out
        // the values and set them to be uninitialized. The `assume_init` is
        // safe because the type we are claiming to have initialized here is a
        // bunch of `MaybeUninit`s, which do not require initialization.
        // TODO(https://fxbug.dev/42160423): use MaybeUninit::uninit_array once it
        // is stablized.
        #[allow(clippy::uninit_assumed_init)]
        let storage =
            std::mem::replace(&mut self.storage, unsafe { MaybeUninit::uninit().assume_init() });
        ChainedIter { storage, len, consumed: 0 }
    }
}

struct ChainedIter<T> {
    storage: [MaybeUninit<T>; netdev::MAX_DESCRIPTOR_CHAIN as usize],
    len: ChainLength,
    consumed: u8,
}

impl<T> Iterator for ChainedIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.consumed < self.len.get() {
            // Safety: it is safe now to replace that slot with an uninitialized
            // value because we will advance consumed by 1.
            let value = unsafe {
                std::mem::replace(
                    &mut self.storage[usize::from(self.consumed)],
                    MaybeUninit::uninit(),
                )
                .assume_init()
            };
            self.consumed += 1;
            Some(value)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = usize::from(self.len.get() - self.consumed);
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for ChainedIter<T> {}

impl<T> Drop for ChainedIter<T> {
    fn drop(&mut self) {
        // Safety: `self.storage[self.consumed..self.len]` is initialized.
        unsafe {
            std::ptr::drop_in_place(std::mem::transmute::<_, &mut [T]>(
                &mut self.storage[self.consumed.into()..self.len.into()],
            ));
        }
    }
}

/// Guards the allocated descriptors; they will be freed when dropped.
pub(in crate::session) struct AllocGuard<K: AllocKind> {
    descs: Chained<DescId<K>>,
    pool: Arc<Pool>,
}

impl<K: AllocKind> Debug for AllocGuard<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { descs, pool: _ } = self;
        f.debug_struct("AllocGuard").field("descs", descs).finish()
    }
}

impl<K: AllocKind> AllocGuard<K> {
    fn new(descs: Chained<DescId<K>>, pool: Arc<Pool>) -> Self {
        Self { descs, pool }
    }

    /// Iterates over references to the descriptors.
    fn descriptors(&self) -> impl Iterator<Item = DescRef<'_, K>> + '_ {
        self.descs.iter().map(move |desc| self.pool.descriptors.borrow(desc))
    }

    /// Iterates over mutable references to the descriptors.
    fn descriptors_mut(&mut self) -> impl Iterator<Item = DescRefMut<'_, K>> + '_ {
        let descriptors = &self.pool.descriptors;
        self.descs.iter_mut().map(move |desc| descriptors.borrow_mut(desc))
    }

    /// Gets a reference to the head descriptor.
    fn descriptor(&self) -> DescRef<'_, K> {
        self.descriptors().next().expect("descriptors must not be empty")
    }

    /// Gets a mutable reference to the head descriptor.
    fn descriptor_mut(&mut self) -> DescRefMut<'_, K> {
        self.descriptors_mut().next().expect("descriptors must not be empty")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DescriptorLayout {
    chain_length: ChainLength,
    head_length: u16,
    data_length: u32,
    tail_length: u16,
}

impl AllocGuard<Tx> {
    /// Calculates the layout for each descriptor in this allocation chain.
    ///
    /// The layouts are calculated to satisfy the requested `target_len`, while
    /// ensuring the session's `min_tx_head` and `min_tx_tail` requirements are
    /// met.
    ///
    /// Returns `Err(Error::TxLength)` if the requirements cannot be met (e.g. if the
    /// required tail padding overflows `u16`).
    fn calculate_descriptor_layouts(&self, target_len: usize) -> Result<Chained<DescriptorLayout>> {
        let len = self.len();
        let BufferLayout { min_tx_head, min_tx_tail, length: buffer_length, .. } =
            self.pool.buffer_layout;

        let mut remaining_target = target_len;
        (0..len)
            .rev()
            .map(|clen| {
                let chain_length = ChainLength::try_from(clen).unwrap();
                let head_length = if clen + 1 == len { min_tx_head } else { 0 };
                let mut tail_length = if clen == 0 { min_tx_tail } else { 0 };

                // head_length and tail_length. The check was done when the config
                // for pool was created, so the subtraction won't overflow.
                let available_bytes = u32::try_from(
                    buffer_length - usize::from(head_length) - usize::from(tail_length),
                )
                .unwrap();

                let data_length = match u32::try_from(remaining_target) {
                    Ok(target) => {
                        if target < available_bytes {
                            // The target bytes are less than what is available,
                            // we need to put the excess in the tail so that the
                            // user cannot write more than they requested (or padded).
                            let excess = available_bytes - target;
                            tail_length = u16::try_from(excess)
                                .ok_checked::<TryFromIntError>()
                                .and_then(|tail_adjustment| {
                                    tail_length.checked_add(tail_adjustment)
                                })
                                .ok_or(Error::TxLength)?;
                        }
                        target.min(available_bytes)
                    }
                    Err(TryFromIntError { .. }) => available_bytes,
                };

                let data_length_usize =
                    usize::try_from(data_length).expect("u32 must fit in a usize");
                remaining_target = remaining_target.saturating_sub(data_length_usize);

                Ok::<_, Error>(DescriptorLayout {
                    chain_length,
                    head_length,
                    data_length,
                    tail_length,
                })
            })
            .collect()
    }

    /// Initializes descriptors of a tx allocation.
    ///
    /// We choose to enforce and satisfy the `min_tx_data` layout requirement
    /// (imposed by the device/driver) immediately during buffer allocation and
    /// initialization here.
    ///
    /// Consequently, the allocated buffer's capacity (`target_len`) may be
    /// larger than the `requested_bytes` if `requested_bytes` is smaller than
    /// `min_tx_data`.
    ///
    /// While this means we might spend CPU cycles zero-padding buffers that are
    /// subsequently dropped without being sent (a rare occurrence in typical
    /// usage), this guarantees that buffer is always suitable for sending. This
    /// also makes the transmit path (`Session::send`) infallible.
    fn init(&mut self, requested_bytes: usize) -> Result<()> {
        let min_tx_data = self.pool.buffer_layout.min_tx_data;
        let target_len = requested_bytes.max(usize::from(min_tx_data));
        let layouts = self.calculate_descriptor_layouts(target_len)?;

        let mut remaining_requested = requested_bytes;

        for (desc_id, DescriptorLayout { chain_length, head_length, data_length, tail_length }) in
            self.descs.iter_mut().zip(layouts)
        {
            // Initialize the descriptor.
            {
                let mut descriptor = self.pool.descriptors.borrow_mut(desc_id);
                descriptor.initialize(chain_length, head_length, data_length, tail_length);
            }

            let data_length_usize = usize::try_from(data_length).expect("u32 must fit in a usize");
            let requested_in_part = std::cmp::min(remaining_requested, data_length_usize);
            let pad_in_part = data_length_usize - requested_in_part;

            // Zero-pad any excess capacity in this buffer part that was allocated
            // to satisfy the `min_tx_data` layout requirement but not requested by
            // the caller.
            //
            // We decided to pad the buffer on initialization because the lazy commit
            // model can only avoid padding for the following 2 cases:
            // 1) User only allocates but never sends.
            // 2) User writes past their requested size and meets the min_tx_data
            //    requirement.
            // Both should be uncommon, and in case 2) we can fix the client by
            // requesting a larger size to avoid padding.
            if pad_in_part > 0 {
                let slice = self.pool.get_slice_mut(desc_id);
                slice[requested_in_part..requested_in_part + pad_in_part].fill(0);
            }

            remaining_requested -= requested_in_part;
        }
        Ok(())
    }
}

impl<K: AllocKind> Drop for AllocGuard<K> {
    fn drop(&mut self) {
        if self.is_empty() {
            return;
        }
        K::free(private::Allocation(self));
    }
}

impl<K: AllocKind> Deref for AllocGuard<K> {
    type Target = [DescId<K>];

    fn deref(&self) -> &Self::Target {
        self.descs.deref()
    }
}

impl<K: AllocKind> From<AllocGuard<K>> for Buffer<K> {
    fn from(alloc: AllocGuard<K>) -> Self {
        Self { alloc }
    }
}

impl<T, K: AllocKind> Read for BufferIO<T, K>
where
    T: AsRef<[u8]>,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read_len = self.read_at(self.pos, buf);
        self.pos += read_len;
        Ok(read_len)
    }
}

impl<T> Write for BufferIO<T, Tx>
where
    T: AsMut<[u8]>,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let write_len = self.write_at(self.pos, buf);
        self.pos += write_len;
        Ok(write_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<T, K: AllocKind> Seek for BufferIO<T, K> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => {
                let end = i64::try_from(self.len).unwrap();
                u64::try_from(end.wrapping_add(offset)).unwrap()
            }
            SeekFrom::Current(offset) => {
                let current = i64::try_from(self.pos).map_err(|TryFromIntError { .. }| {
                    std::io::Error::from(std::io::ErrorKind::InvalidInput)
                })?;
                u64::try_from(current.wrapping_add(offset)).unwrap()
            }
        };
        self.pos = usize::try_from(pos).map_err(|TryFromIntError { .. }| {
            std::io::Error::from(std::io::ErrorKind::InvalidInput)
        })?;
        Ok(pos)
    }
}

/// A pending tx allocation request.
struct TxAllocReq {
    sender: Sender<AllocGuard<Tx>>,
    size: ChainLength,
}

impl TxAllocReq {
    fn new(size: ChainLength) -> (Self, Receiver<AllocGuard<Tx>>) {
        let (sender, receiver) = channel();
        (TxAllocReq { sender, size }, receiver)
    }

    /// Fulfills the pending request with an `AllocGuard`.
    ///
    /// If the request is already closed, the guard is simply dropped and
    /// returned to the queue.
    ///
    /// `fulfill` must *not* be called when the `guard`'s pool is holding the tx
    /// lock, since we may deadlock/panic upon the double tx lock acquisition.
    fn fulfill(self, guard: AllocGuard<Tx>) {
        let Self { sender, size: _ } = self;
        match sender.send(guard) {
            Ok(()) => (),
            Err(guard) => {
                // It's ok to just drop the guard here, it'll be returned to the
                // pool.
                drop(guard);
            }
        }
    }
}

/// A module for sealed traits so that the user of this crate can not implement
/// [`AllocKind`] for anything than [`Rx`] and [`Tx`].
mod private {
    use super::{AllocKind, Rx, Tx};
    pub trait Sealed: 'static + Sized {}
    impl Sealed for Rx {}
    impl Sealed for Tx {}

    // We can't leak a private type in a public trait, create an opaque private
    // new type for &mut super::AllocGuard so that we can mention it in the
    // AllocKind trait.
    pub struct Allocation<'a, K: AllocKind>(pub(super) &'a mut super::AllocGuard<K>);
}

/// An allocation can have two kinds, this trait provides a way to project a
/// type ([`Rx`] or [`Tx`]) into a value.
pub trait AllocKind: private::Sealed {
    /// The reflected value of Self.
    const REFL: AllocKindRefl;

    /// frees an allocation of the given kind.
    fn free(alloc: private::Allocation<'_, Self>);
}

/// A tag to related types for Tx allocations.
pub enum Tx {}
/// A tag to related types for Rx allocations.
pub enum Rx {}

/// The reflected value that allows inspection on an [`AllocKind`] type.
pub enum AllocKindRefl {
    Tx,
    Rx,
}

impl AllocKindRefl {
    pub(in crate::session) fn as_str(&self) -> &'static str {
        match self {
            AllocKindRefl::Tx => "Tx",
            AllocKindRefl::Rx => "Rx",
        }
    }
}

impl AllocKind for Tx {
    const REFL: AllocKindRefl = AllocKindRefl::Tx;

    fn free(alloc: private::Allocation<'_, Self>) {
        let private::Allocation(AllocGuard { pool, descs }) = alloc;
        pool.free_tx(std::mem::replace(descs, Chained::empty()));
    }
}

impl AllocKind for Rx {
    const REFL: AllocKindRefl = AllocKindRefl::Rx;

    fn free(alloc: private::Allocation<'_, Self>) {
        let private::Allocation(AllocGuard { pool, descs }) = alloc;
        pool.free_rx(std::mem::replace(descs, Chained::empty()));
        pool.rx_leases.rx_complete();
    }
}

/// An extracted struct containing state pertaining to watching rx leases.
pub(in crate::session) struct RxLeaseHandlingState {
    can_watch_rx_leases: AtomicBool,
    /// Keeps a rolling counter of received rx frames MINUS the target frame
    /// number of the current outstanding lease.
    ///
    /// When no leases are pending (via [`RxLeaseWatcher::wait_until`]),
    /// then this matches exactly the number of received frames.
    ///
    /// Otherwise, the lease is currently waiting for remaining `u64::MAX -
    /// rx_Frame_counter` frames. The logic depends on `AtomicU64` wrapping
    /// around as part of completing rx buffers.
    rx_frame_counter: AtomicU64,
    rx_lease_waker: AtomicWaker,
}

impl RxLeaseHandlingState {
    fn new_with_flags(flags: netdev::SessionFlags) -> Self {
        Self::new_with_enabled(flags.contains(netdev::SessionFlags::RECEIVE_RX_POWER_LEASES))
    }

    fn new_with_enabled(enabled: bool) -> Self {
        Self {
            can_watch_rx_leases: AtomicBool::new(enabled),
            rx_frame_counter: AtomicU64::new(0),
            rx_lease_waker: AtomicWaker::new(),
        }
    }

    /// Increments the total receive frame counter and possibly wakes up a
    /// waiting lease yielder.
    fn rx_complete(&self) {
        let Self { can_watch_rx_leases: _, rx_frame_counter, rx_lease_waker } = self;
        let prev = rx_frame_counter.fetch_add(1, atomic::Ordering::SeqCst);

        // See wait_until for details. We need to hit a waker whenever our add
        // wrapped the u64 back around to 0.
        if prev == u64::MAX {
            rx_lease_waker.wake();
        }
    }
}

/// A trait allowing [`RxLeaseWatcher`] to be agnostic over how to get an
/// [`RxLeaseHandlingState`].
pub(in crate::session) trait RxLeaseHandlingStateContainer {
    fn lease_handling_state(&self) -> &RxLeaseHandlingState;
}

impl<T: Borrow<RxLeaseHandlingState>> RxLeaseHandlingStateContainer for T {
    fn lease_handling_state(&self) -> &RxLeaseHandlingState {
        self.borrow()
    }
}

impl RxLeaseHandlingStateContainer for Arc<Pool> {
    fn lease_handling_state(&self) -> &RxLeaseHandlingState {
        &self.rx_leases
    }
}

/// A type safe-wrapper around a single lease watcher per `Pool`.
pub(in crate::session) struct RxLeaseWatcher<T> {
    state: T,
}

impl<T: RxLeaseHandlingStateContainer> RxLeaseWatcher<T> {
    /// Creates a new lease watcher.
    ///
    /// # Panics
    ///
    /// Panics if an [`RxLeaseWatcher`] has already been created for the given
    /// pool or the pool was not configured for it.
    pub(in crate::session) fn new(state: T) -> Self {
        assert!(
            state.lease_handling_state().can_watch_rx_leases.swap(false, atomic::Ordering::SeqCst),
            "can't watch rx leases"
        );
        Self { state }
    }

    /// Called by sessions to wait until `hold_until_frame` is fulfilled to
    /// yield leases out.
    ///
    /// Blocks until `hold_until_frame`-th rx buffer has been released.
    ///
    /// Note that this method takes `&mut self` because only one
    /// [`RxLeaseWatcher`] may be created by lease handling state, and exclusive
    /// access to it is required to watch lease completion.
    pub(in crate::session) async fn wait_until(&mut self, hold_until_frame: u64) {
        // A note about wrap-arounds.
        //
        // We're assuming the frame counter will never wrap around for
        // correctness here. This should be fine, even assuming a packet
        // rate of 1 million pps it'd take almost 600k years for this counter
        // to wrap around:
        // - 2^64 / 1e6 / 60 / 60 / 24 / 365 ~ 584e3.

        let RxLeaseHandlingState { can_watch_rx_leases: _, rx_frame_counter, rx_lease_waker } =
            self.state.lease_handling_state();

        let prev = rx_frame_counter.fetch_sub(hold_until_frame, atomic::Ordering::SeqCst);
        // After having subtracted the waiting value we *must always restore the
        // value* on return, even if the future is not polled to completion.
        let _guard = scopeguard::guard((), |()| {
            let _: u64 = rx_frame_counter.fetch_add(hold_until_frame, atomic::Ordering::SeqCst);
        });

        // Lease is ready to be fulfilled.
        if prev >= hold_until_frame {
            return;
        }
        // Threshold is a wrapped around subtraction. So now we must wait
        // until the read value from the atomic is LESS THAN the threshold.
        let threshold = prev.wrapping_sub(hold_until_frame);
        futures::future::poll_fn(|cx| {
            let v = rx_frame_counter.load(atomic::Ordering::SeqCst);
            if v < threshold {
                return Poll::Ready(());
            }
            rx_lease_waker.register(cx.waker());
            let v = rx_frame_counter.load(atomic::Ordering::SeqCst);
            if v < threshold {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    use assert_matches::assert_matches;
    use fuchsia_async as fasync;
    use futures::future::FutureExt;
    use test_case::test_case;

    use std::collections::HashSet;
    use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
    use std::pin::pin;
    use std::task::{Poll, Waker};

    const DEFAULT_MIN_TX_BUFFER_HEAD: u16 = 4;
    const DEFAULT_MIN_TX_BUFFER_TAIL: u16 = 8;
    // Safety: These are safe because none of the values are zero.
    const DEFAULT_BUFFER_LENGTH: NonZeroUsize = NonZeroUsize::new(64).unwrap();
    const DEFAULT_TX_BUFFERS: NonZeroU16 = NonZeroU16::new(8).unwrap();
    const DEFAULT_RX_BUFFERS: NonZeroU16 = NonZeroU16::new(8).unwrap();
    const MAX_BUFFER_BYTES: usize = DEFAULT_BUFFER_LENGTH.get()
        * netdev::MAX_DESCRIPTOR_CHAIN as usize
        - DEFAULT_MIN_TX_BUFFER_HEAD as usize
        - DEFAULT_MIN_TX_BUFFER_TAIL as usize;

    const SENTINEL_BYTE: u8 = 0xab;
    const WRITE_BYTE: u8 = 1;
    const PAD_BYTE: u8 = 0;

    const DEFAULT_CONFIG: Config = Config {
        buffer_stride: NonZeroU64::new(DEFAULT_BUFFER_LENGTH.get() as u64).unwrap(),
        num_rx_buffers: DEFAULT_RX_BUFFERS,
        num_tx_buffers: DEFAULT_TX_BUFFERS,
        options: netdev::SessionFlags::empty(),
        buffer_layout: BufferLayout {
            length: DEFAULT_BUFFER_LENGTH.get(),
            min_tx_head: DEFAULT_MIN_TX_BUFFER_HEAD,
            min_tx_tail: DEFAULT_MIN_TX_BUFFER_TAIL,
            min_tx_data: 0,
        },
    };

    impl Pool {
        fn new_test_default() -> Arc<Self> {
            let (pool, _descriptors, _data) =
                Pool::new(DEFAULT_CONFIG).expect("failed to create default pool");
            pool
        }

        async fn alloc_tx_checked(self: &Arc<Self>, n: u8) -> AllocGuard<Tx> {
            self.alloc_tx(ChainLength::try_from(n).expect("failed to convert to chain length"))
                .await
        }

        fn alloc_tx_now_or_never(self: &Arc<Self>, n: u8) -> Option<AllocGuard<Tx>> {
            self.alloc_tx_checked(n).now_or_never()
        }

        fn alloc_tx_all(self: &Arc<Self>, n: u8) -> Vec<AllocGuard<Tx>> {
            std::iter::from_fn(|| self.alloc_tx_now_or_never(n)).collect()
        }

        fn alloc_tx_buffer_now_or_never(self: &Arc<Self>, num_bytes: usize) -> Option<Buffer<Tx>> {
            self.alloc_tx_buffer(num_bytes)
                .now_or_never()
                .transpose()
                .expect("invalid arguments for alloc_tx_buffer")
        }

        fn set_min_tx_buffer_length(self: &mut Arc<Self>, length: usize) {
            Arc::get_mut(self).unwrap().buffer_layout.min_tx_data = length;
        }

        fn fill_sentinel_bytes(&mut self) {
            // Safety: We have mut reference to Pool, so we get to modify the
            // VMO pointed by self.base.
            unsafe { std::ptr::write_bytes(self.base.as_ptr(), SENTINEL_BYTE, self.bytes) };
        }
    }

    impl Buffer<Tx> {
        // Write a byte at offset, the result buffer should be pad_size long, with
        // 0..offset being the SENTINEL_BYTE, offset being the WRITE_BYTE and the
        // rest being PAD_BYTE.
        fn check_write_and_pad(&mut self, offset: usize, pad_size: usize) {
            {
                let mut io = self.io_mut();
                assert_eq!(io.write_at(offset, &[WRITE_BYTE][..]), 1);
            }
            assert_eq!(self.len(), pad_size);
            // An arbitrary value that is not SENTINAL/WRITE/PAD_BYTE so that
            // we can make sure the write really happened.
            const INIT_BYTE: u8 = 42;
            let mut read_buf = vec![INIT_BYTE; pad_size];
            assert_eq!(self.io().read_at(0, &mut read_buf[..]), read_buf.len());
            for (idx, byte) in read_buf.iter().enumerate() {
                if idx < offset {
                    assert_eq!(*byte, SENTINEL_BYTE);
                } else if idx == offset {
                    assert_eq!(*byte, WRITE_BYTE);
                } else {
                    assert_eq!(*byte, PAD_BYTE);
                }
            }
        }
    }

    impl<K, I, T> PartialEq<T> for Chained<DescId<K>>
    where
        K: AllocKind,
        I: ExactSizeIterator<Item = u16>,
        T: Copy + IntoIterator<IntoIter = I>,
    {
        fn eq(&self, other: &T) -> bool {
            let iter = other.into_iter();
            if usize::from(self.len) != iter.len() {
                return false;
            }
            self.iter().zip(iter).all(|(l, r)| l.get() == r)
        }
    }

    impl Debug for TxAllocReq {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let TxAllocReq { sender: _, size } = self;
            f.debug_struct("TxAllocReq").field("size", &size).finish_non_exhaustive()
        }
    }

    #[test]
    fn alloc_tx_distinct() {
        let pool = Pool::new_test_default();
        let allocated = pool.alloc_tx_all(1);
        assert_eq!(allocated.len(), DEFAULT_TX_BUFFERS.get().into());
        let distinct = allocated
            .iter()
            .map(|alloc| {
                assert_eq!(alloc.descs.len(), 1);
                alloc.descs[0].get()
            })
            .collect::<HashSet<u16>>();
        assert_eq!(allocated.len(), distinct.len());
    }

    #[test]
    fn alloc_tx_free_len() {
        let pool = Pool::new_test_default();
        {
            let allocated = pool.alloc_tx_all(2);
            assert_eq!(
                allocated.iter().fold(0, |acc, a| { acc + a.descs.len() }),
                DEFAULT_TX_BUFFERS.get().into()
            );
            assert_eq!(pool.tx_alloc_state.lock().free_list.len, 0);
        }
        assert_eq!(pool.tx_alloc_state.lock().free_list.len, DEFAULT_TX_BUFFERS.get());
    }

    #[test]
    fn alloc_tx_chain() {
        let pool = Pool::new_test_default();
        let allocated = pool.alloc_tx_all(3);
        assert_eq!(allocated.len(), usize::from(DEFAULT_TX_BUFFERS.get()) / 3);
        assert_matches!(pool.alloc_tx_now_or_never(3), None);
        assert_matches!(pool.alloc_tx_now_or_never(2), Some(a) if a.descs.len() == 2);
    }

    #[test]
    fn alloc_tx_many() {
        let pool = Pool::new_test_default();
        let data_len = u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
            - u32::from(DEFAULT_MIN_TX_BUFFER_HEAD)
            - u32::from(DEFAULT_MIN_TX_BUFFER_TAIL);
        let data_len = usize::try_from(data_len).unwrap();
        let mut buffers = pool
            .alloc_tx_buffers(data_len)
            .now_or_never()
            .expect("failed to alloc")
            .unwrap()
            // Collect into a vec so we keep the buffers alive, otherwise they
            // are immediately returned to the pool.
            .collect::<Result<Vec<_>>>()
            .expect("buffer error");
        assert_eq!(buffers.len(), DEFAULT_TX_BUFFERS.get().into());

        // We have all the buffers, which means allocating more should not
        // resolve.
        assert!(pool.alloc_tx_buffers(data_len).now_or_never().is_none());

        // If we release a single buffer we should be able to retrieve it again.
        assert_matches!(buffers.pop(), Some(_));
        let mut more_buffers =
            pool.alloc_tx_buffers(data_len).now_or_never().expect("failed to alloc").unwrap();
        let buffer = assert_matches!(more_buffers.next(), Some(Ok(b)) => b);
        assert_matches!(more_buffers.next(), None);
        // The iterator is fused, so None is yielded even after dropping the
        // buffer.
        drop(buffer);
        assert_matches!(more_buffers.next(), None);
    }

    #[test]
    fn alloc_tx_after_free() {
        let pool = Pool::new_test_default();
        let mut allocated = pool.alloc_tx_all(1);
        assert_matches!(pool.alloc_tx_now_or_never(2), None);
        {
            let _drained = allocated.drain(..2);
        }
        assert_matches!(pool.alloc_tx_now_or_never(2), Some(a) if a.descs.len() == 2);
    }

    #[test]
    fn blocking_alloc_tx() {
        let mut executor = fasync::TestExecutor::new();
        let pool = Pool::new_test_default();
        let mut allocated = pool.alloc_tx_all(1);
        let alloc_fut = pool.alloc_tx_checked(1);
        let mut alloc_fut = pin!(alloc_fut);
        // The allocation should block.
        assert_matches!(executor.run_until_stalled(&mut alloc_fut), Poll::Pending);
        // And the allocation request should be queued.
        assert!(!pool.tx_alloc_state.lock().requests.is_empty());
        let freed = allocated
            .pop()
            .expect("no fulfulled allocations")
            .iter()
            .map(|x| x.get())
            .collect::<Chained<_>>();
        let same_as_freed =
            |descs: &Chained<DescId<Tx>>| descs.iter().map(|x| x.get()).eq(freed.iter().copied());
        // Now the task should be able to continue.
        assert_matches!(
            &executor.run_until_stalled(&mut alloc_fut),
            Poll::Ready(AllocGuard{ descs, pool: _ }) if same_as_freed(descs)
        );
        // And the queued request should now be removed.
        assert!(pool.tx_alloc_state.lock().requests.is_empty());
    }

    #[test]
    fn blocking_alloc_tx_cancel_before_free() {
        let mut executor = fasync::TestExecutor::new();
        let pool = Pool::new_test_default();
        let mut allocated = pool.alloc_tx_all(1);
        {
            let alloc_fut = pool.alloc_tx_checked(1);
            let mut alloc_fut = pin!(alloc_fut);
            assert_matches!(executor.run_until_stalled(&mut alloc_fut), Poll::Pending);
            assert_matches!(
                pool.tx_alloc_state.lock().requests.as_slices(),
                (&[ref req1, ref req2], &[]) if req1.size.get() == 1 && req2.size.get() == 1
            );
        }
        assert_matches!(
            allocated.pop(),
            Some(AllocGuard { ref descs, pool: ref p })
                if descs == &[DEFAULT_TX_BUFFERS.get() - 1] && Arc::ptr_eq(p, &pool)
        );
        let state = pool.tx_alloc_state.lock();
        assert_eq!(state.free_list.len, 1);
        assert!(state.requests.is_empty());
    }

    #[test]
    fn blocking_alloc_tx_cancel_after_free() {
        let mut executor = fasync::TestExecutor::new();
        let pool = Pool::new_test_default();
        let mut allocated = pool.alloc_tx_all(1);
        {
            let alloc_fut = pool.alloc_tx_checked(1);
            let mut alloc_fut = pin!(alloc_fut);
            assert_matches!(executor.run_until_stalled(&mut alloc_fut), Poll::Pending);
            assert_matches!(
                pool.tx_alloc_state.lock().requests.as_slices(),
                (&[ref req1, ref req2], &[]) if req1.size.get() == 1 && req2.size.get() == 1
            );
            assert_matches!(
                allocated.pop(),
                Some(AllocGuard { ref descs, pool: ref p })
                    if descs == &[DEFAULT_TX_BUFFERS.get() - 1] && Arc::ptr_eq(p, &pool)
            );
        }
        let state = pool.tx_alloc_state.lock();
        assert_eq!(state.free_list.len, 1);
        assert!(state.requests.is_empty());
    }

    #[test]
    fn multiple_blocking_alloc_tx_fulfill_order() {
        const TASKS_TOTAL: usize = 3;
        let mut executor = fasync::TestExecutor::new();
        let pool = Pool::new_test_default();
        let mut allocated = pool.alloc_tx_all(1);
        let mut alloc_futs = (1..=TASKS_TOTAL)
            .rev()
            .map(|x| {
                let pool = pool.clone();
                (x, Box::pin(async move { pool.alloc_tx_checked(x.try_into().unwrap()).await }))
            })
            .collect::<Vec<_>>();

        for (idx, (req_size, task)) in alloc_futs.iter_mut().enumerate() {
            assert_matches!(executor.run_until_stalled(task), Poll::Pending);
            // assert that the tasks are sorted decreasing on the requested size.
            assert_eq!(idx + *req_size, TASKS_TOTAL);
        }
        {
            let state = pool.tx_alloc_state.lock();
            // The first pending request was introduced by `alloc_tx_all`.
            assert_eq!(state.requests.len(), TASKS_TOTAL + 1);
            let mut requests = state.requests.iter();
            // It should already be cancelled because the requesting future is
            // already dropped.
            assert!(requests.next().unwrap().sender.is_canceled());
            // The rest of the requests must not be cancelled.
            assert!(requests.all(|req| !req.sender.is_canceled()))
        }

        let mut to_free = Vec::new();
        let mut freed = 0;
        for free_size in (1..=TASKS_TOTAL).rev() {
            let (_req_size, mut task) = alloc_futs.remove(0);
            for _ in 1..free_size {
                freed += 1;
                assert_matches!(
                    allocated.pop(),
                    Some(AllocGuard { ref descs, pool: ref p })
                        if descs == &[DEFAULT_TX_BUFFERS.get() - freed] && Arc::ptr_eq(p, &pool)
                );
                assert_matches!(executor.run_until_stalled(&mut task), Poll::Pending);
            }
            freed += 1;
            assert_matches!(
                allocated.pop(),
                Some(AllocGuard { ref descs, pool: ref p })
                    if descs == &[DEFAULT_TX_BUFFERS.get() - freed] && Arc::ptr_eq(p, &pool)
            );
            match executor.run_until_stalled(&mut task) {
                Poll::Ready(alloc) => {
                    assert_eq!(alloc.len(), free_size);
                    // Don't return the allocation to the pool now.
                    to_free.push(alloc);
                }
                Poll::Pending => panic!("The request should be fulfilled"),
            }
            // The rest of requests can not be fulfilled.
            for (_req_size, task) in alloc_futs.iter_mut() {
                assert_matches!(executor.run_until_stalled(task), Poll::Pending);
            }
        }
        assert!(pool.tx_alloc_state.lock().requests.is_empty());
    }

    #[test]
    fn singleton_tx_layout() {
        let pool = Pool::new_test_default();
        let buffers = std::iter::from_fn(|| {
            let data_len = u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
                - u32::from(DEFAULT_MIN_TX_BUFFER_HEAD)
                - u32::from(DEFAULT_MIN_TX_BUFFER_TAIL);
            pool.alloc_tx_buffer_now_or_never(usize::try_from(data_len).unwrap()).map(|buffer| {
                assert_eq!(buffer.alloc.descriptors().count(), 1);
                let offset = u64::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
                    * u64::from(buffer.alloc[0].get());
                {
                    let descriptor = buffer.alloc.descriptor();
                    assert_matches!(descriptor.chain_length(), Ok(ChainLength::ZERO));
                    assert_eq!(descriptor.head_length(), DEFAULT_MIN_TX_BUFFER_HEAD);
                    assert_eq!(descriptor.tail_length(), DEFAULT_MIN_TX_BUFFER_TAIL);
                    assert_eq!(descriptor.data_length(), data_len);
                    assert_eq!(descriptor.offset(), offset);
                }

                {
                    let mut slices = buffer.parts();
                    let slice = slices.next().expect("should have one slice");
                    assert_matches!(slices.next(), None);
                    assert_eq!(slice.len(), usize::try_from(data_len).unwrap());
                    assert_eq!(
                        slice.as_ptr(),
                        pool.base.as_ptr().wrapping_add(
                            usize::try_from(offset).unwrap()
                                + usize::from(DEFAULT_MIN_TX_BUFFER_HEAD),
                        )
                    );
                }
                buffer
            })
        })
        .collect::<Vec<_>>();
        assert_eq!(buffers.len(), usize::from(DEFAULT_TX_BUFFERS.get()));
    }

    #[test]
    fn chained_tx_layout() {
        let pool = Pool::new_test_default();
        let alloc_len = 4 * DEFAULT_BUFFER_LENGTH.get()
            - usize::from(DEFAULT_MIN_TX_BUFFER_HEAD)
            - usize::from(DEFAULT_MIN_TX_BUFFER_TAIL);
        let buffers = std::iter::from_fn(|| {
            pool.alloc_tx_buffer_now_or_never(alloc_len).map(|buffer| {
                assert_eq!(buffer.parts().count(), 4);
                for (idx, (descriptor, slice)) in
                    buffer.alloc.descriptors().zip(buffer.parts()).enumerate()
                {
                    let chain_length = ChainLength::try_from(buffer.alloc.len() - idx - 1).unwrap();
                    let head_length = if idx == 0 { DEFAULT_MIN_TX_BUFFER_HEAD } else { 0 };
                    let tail_length = if chain_length == ChainLength::ZERO {
                        DEFAULT_MIN_TX_BUFFER_TAIL
                    } else {
                        0
                    };
                    let data_len = u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
                        - u32::from(head_length)
                        - u32::from(tail_length);
                    let offset = u64::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
                        * u64::from(buffer.alloc[idx].get());
                    assert_eq!(descriptor.chain_length().unwrap(), chain_length);
                    assert_eq!(descriptor.head_length(), head_length);
                    assert_eq!(descriptor.tail_length(), tail_length);
                    assert_eq!(descriptor.offset(), offset);
                    assert_eq!(descriptor.data_length(), data_len);
                    if chain_length != ChainLength::ZERO {
                        assert_eq!(descriptor.nxt(), Some(buffer.alloc[idx + 1].get()));
                    }

                    assert_eq!(slice.len(), usize::try_from(data_len).unwrap());
                    assert_eq!(
                        slice.as_ptr(),
                        pool.base.as_ptr().wrapping_add(
                            usize::try_from(offset).unwrap() + usize::from(head_length),
                        )
                    );
                }
                buffer
            })
        })
        .collect::<Vec<_>>();
        assert_eq!(buffers.len(), usize::from(DEFAULT_TX_BUFFERS.get()) / 4);
    }

    #[test]
    fn rx_distinct() {
        let pool = Pool::new_test_default();
        let mut guard = pool.rx_pending.inner.lock();
        let (descs, _): &mut (Vec<_>, Option<Waker>) = &mut *guard;
        assert_eq!(descs.len(), usize::from(DEFAULT_RX_BUFFERS.get()));
        let distinct = descs.iter().map(|desc| desc.get()).collect::<HashSet<u16>>();
        assert_eq!(descs.len(), distinct.len());
    }

    #[test]
    fn alloc_rx_layout() {
        let pool = Pool::new_test_default();
        let mut guard = pool.rx_pending.inner.lock();
        let (descs, _): &mut (Vec<_>, Option<Waker>) = &mut *guard;
        assert_eq!(descs.len(), usize::from(DEFAULT_RX_BUFFERS.get()));
        for desc in descs.iter() {
            let descriptor = pool.descriptors.borrow(desc);
            let offset =
                u64::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap() * u64::from(desc.get());
            assert_matches!(descriptor.chain_length(), Ok(ChainLength::ZERO));
            assert_eq!(descriptor.head_length(), 0);
            assert_eq!(descriptor.tail_length(), 0);
            assert_eq!(descriptor.offset(), offset);
            assert_eq!(
                descriptor.data_length(),
                u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
            );
        }
    }

    #[test]
    fn buffer_read_at_write_at() {
        let pool = Pool::new_test_default();
        let alloc_bytes = DEFAULT_BUFFER_LENGTH.get();
        let mut buffer =
            pool.alloc_tx_buffer_now_or_never(alloc_bytes).expect("failed to allocate");
        // Because we have to accommodate the space for head and tail, there
        // would be 2 parts instead of 1.
        assert_eq!(buffer.parts().count(), 2);
        assert_eq!(buffer.len(), alloc_bytes);
        let write_buf = (0..u8::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()).collect::<Vec<_>>();
        assert_eq!(buffer.io_mut().write_at(0, &write_buf[..]), write_buf.len());
        let mut read_buf = [0xff; DEFAULT_BUFFER_LENGTH.get()];
        assert_eq!(buffer.io().read_at(0, &mut read_buf[..]), read_buf.len());
        for (idx, byte) in read_buf.iter().enumerate() {
            assert_eq!(*byte, write_buf[idx]);
        }
    }

    #[test]
    fn buffer_write_at_short() {
        let pool = Pool::new_test_default();
        let alloc_bytes = DEFAULT_BUFFER_LENGTH.get();
        let mut buffer =
            pool.alloc_tx_buffer_now_or_never(alloc_bytes).expect("failed to allocate");
        assert_eq!(buffer.parts().count(), 2);
        assert_eq!(buffer.len(), alloc_bytes);

        let write_buf = vec![WRITE_BYTE; alloc_bytes + 10];

        // Test short write (writing more than buffer capacity)
        assert_eq!(buffer.io_mut().write_at(0, &write_buf[..]), alloc_bytes);

        // Verify short write
        let mut read_buf = vec![0; alloc_bytes];
        assert_eq!(buffer.io().read_at(0, &mut read_buf[..]), alloc_bytes);
        for byte in read_buf.iter() {
            assert_eq!(*byte, WRITE_BYTE);
        }

        // Test write with offset past end
        assert_eq!(buffer.io_mut().write_at(alloc_bytes + 1, &write_buf[..]), 0);

        // Test write with offset inside buffer but src extending past end
        let offset = alloc_bytes / 2;
        let expected_write = alloc_bytes - offset;
        let write_buf = vec![2; alloc_bytes]; // Different byte to distinguish
        assert_eq!(buffer.io_mut().write_at(offset, &write_buf[..]), expected_write);

        // Verify the write
        let mut read_buf = vec![0; alloc_bytes];
        assert_eq!(buffer.io().read_at(0, &mut read_buf[..]), alloc_bytes);
        for (idx, byte) in read_buf.iter().enumerate() {
            if idx < offset {
                assert_eq!(*byte, WRITE_BYTE);
            } else {
                assert_eq!(*byte, 2);
            }
        }
    }

    #[test]
    fn buffer_read_at_short() {
        let pool = Pool::new_test_default();
        let alloc_bytes = DEFAULT_BUFFER_LENGTH.get();
        let mut buffer =
            pool.alloc_tx_buffer_now_or_never(alloc_bytes).expect("failed to allocate");
        assert_eq!(buffer.parts().count(), 2);
        assert_eq!(buffer.len(), alloc_bytes);

        let write_buf = vec![WRITE_BYTE; alloc_bytes];
        assert_eq!(buffer.io_mut().write_at(0, &write_buf[..]), alloc_bytes);

        // Test short read (reading more than buffer capacity)
        let mut read_buf = vec![0xff; alloc_bytes + 10];
        assert_eq!(buffer.io().read_at(0, &mut read_buf[..]), alloc_bytes);
        for (idx, byte) in read_buf.iter().enumerate() {
            if idx < alloc_bytes {
                assert_eq!(*byte, WRITE_BYTE);
            } else {
                assert_eq!(*byte, 0xff);
            }
        }

        // Test read with offset past end
        assert_eq!(buffer.io().read_at(alloc_bytes + 1, &mut read_buf[..]), 0);

        // Test read with offset inside buffer but dst extending past end
        let offset = alloc_bytes / 2;
        let expected_read = alloc_bytes - offset;
        let mut read_buf = vec![0xff; alloc_bytes];
        assert_eq!(buffer.io().read_at(offset, &mut read_buf[..]), expected_read);
        for (idx, byte) in read_buf.iter().enumerate() {
            if idx < expected_read {
                assert_eq!(*byte, WRITE_BYTE);
            } else {
                assert_eq!(*byte, 0xff);
            }
        }
    }

    #[test]
    fn buffer_read_write_seek() {
        let pool = Pool::new_test_default();
        let alloc_bytes = DEFAULT_BUFFER_LENGTH.get();
        let mut buffer =
            pool.alloc_tx_buffer_now_or_never(alloc_bytes).expect("failed to allocate");
        // Because we have to accommodate the space for head and tail, there
        // would be 2 parts instead of 1.
        assert_eq!(buffer.parts().count(), 2);
        assert_eq!(buffer.len(), alloc_bytes);
        let write_buf = (0..u8::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()).collect::<Vec<_>>();

        let mut io = buffer.io_mut();

        assert_eq!(io.write(&write_buf[..]).expect("failed to write into buffer"), write_buf.len());
        const SEEK_FROM_END: usize = 64;
        const READ_LEN: usize = 12;
        assert_eq!(
            io.seek(SeekFrom::End(-i64::try_from(SEEK_FROM_END).unwrap())).unwrap(),
            u64::try_from(io.len - SEEK_FROM_END).unwrap()
        );
        let mut read_buf = [0xff; READ_LEN];
        assert_eq!(io.read(&mut read_buf[..]).expect("failed to read from buffer"), read_buf.len());
        assert_eq!(&write_buf[..READ_LEN], &read_buf[..]);
    }

    #[test_case(32; "single buffer part")]
    #[test_case(MAX_BUFFER_BYTES; "multiple buffer parts")]
    fn buffer_pad(pad_size: usize) {
        let mut pool = Pool::new_test_default();
        pool.set_min_tx_buffer_length(pad_size);
        for offset in 0..pad_size {
            Arc::get_mut(&mut pool)
                .expect("there are multiple owners of the underlying VMO")
                .fill_sentinel_bytes();
            let mut buffer =
                pool.alloc_tx_buffer_now_or_never(offset + 1).expect("failed to allocate buffer");
            buffer.check_write_and_pad(offset, pad_size);
        }
    }

    #[test]
    fn buffer_pad_grow() {
        const BUFFER_PARTS: u8 = 3;
        let mut pool = Pool::new_test_default();
        let pad_size = u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap()
            * u32::from(BUFFER_PARTS)
            - u32::from(DEFAULT_MIN_TX_BUFFER_HEAD)
            - u32::from(DEFAULT_MIN_TX_BUFFER_TAIL);
        pool.set_min_tx_buffer_length(pad_size.try_into().unwrap());
        for offset in 0..pad_size - u32::try_from(DEFAULT_BUFFER_LENGTH.get()).unwrap() {
            Arc::get_mut(&mut pool)
                .expect("there are multiple owners of the underlying VMO")
                .fill_sentinel_bytes();
            let mut alloc =
                pool.alloc_tx_now_or_never(BUFFER_PARTS).expect("failed to alloc descriptors");
            alloc
                .init(usize::try_from(offset).unwrap() + 1)
                .expect("head/body/tail sizes are representable with u16/u32/u16");
            let mut buffer = Buffer::try_from(alloc).unwrap();
            buffer.check_write_and_pad(offset.try_into().unwrap(), pad_size.try_into().unwrap());
        }
    }

    #[test_case(  0; "writes at the beginning")]
    #[test_case( 15; "writes in the first part")]
    #[test_case( 75; "writes in the second part")]
    #[test_case(135; "writes in the third part")]
    #[test_case(195; "writes in the last part")]
    fn buffer_used(write_offset: usize) {
        let pool = Pool::new_test_default();
        let mut buffer =
            pool.alloc_tx_buffer_now_or_never(MAX_BUFFER_BYTES).expect("failed to allocate buffer");
        let expected_caps = (0..netdev::MAX_DESCRIPTOR_CHAIN).map(|i| {
            if i == 0 {
                DEFAULT_BUFFER_LENGTH.get() - usize::from(DEFAULT_MIN_TX_BUFFER_HEAD)
            } else if i < netdev::MAX_DESCRIPTOR_CHAIN - 1 {
                DEFAULT_BUFFER_LENGTH.get()
            } else {
                DEFAULT_BUFFER_LENGTH.get() - usize::from(DEFAULT_MIN_TX_BUFFER_TAIL)
            }
        });
        assert_eq!(buffer.alloc.len(), netdev::MAX_DESCRIPTOR_CHAIN.into());
        assert_eq!(buffer.io_mut().write_at(write_offset, &[WRITE_BYTE][..]), 1);
        // The accumulator is Some if we haven't found the part where the byte
        // was written, None if we've already found it.
        assert_eq!(
            buffer.parts().zip(expected_caps).fold(
                Some(write_offset),
                |offset, (slice, expected_cap)| {
                    assert_eq!(slice.len(), expected_cap);
                    match offset {
                        Some(offset) => {
                            if offset >= expected_cap {
                                Some(offset - slice.len())
                            } else {
                                assert_eq!(slice[offset], WRITE_BYTE);
                                None
                            }
                        }
                        None => None,
                    }
                }
            ),
            None
        );
    }

    #[test]
    fn allocate_under_device_minimum() {
        const MIN_TX_DATA: usize = 32;
        const ALLOC_SIZE: usize = 16;
        const WRITE_BYTE: u8 = 0xff;
        const WRITE_SENTINAL_BYTE: u8 = 0xee;
        const READ_SENTINAL_BYTE: u8 = 0xdd;
        let mut config = DEFAULT_CONFIG;
        config.buffer_layout.min_tx_data = MIN_TX_DATA;
        let (pool, _descriptors, _vmo) = Pool::new(config).expect("failed to create a new pool");
        for mut buffer in Vec::from_iter(std::iter::from_fn({
            let pool = pool.clone();
            move || pool.alloc_tx_buffer_now_or_never(MIN_TX_DATA)
        })) {
            assert_eq!(
                buffer.io_mut().write_at(0, &[WRITE_SENTINAL_BYTE; MIN_TX_DATA]),
                MIN_TX_DATA
            );
        }
        let mut allocated =
            pool.alloc_tx_buffer_now_or_never(16).expect("failed to allocate buffer");
        assert_eq!(allocated.len(), MIN_TX_DATA);
        const WRITE_BUF_SIZE: usize = MIN_TX_DATA + 1;
        assert_eq!(allocated.io_mut().write_at(0, &[WRITE_BYTE; WRITE_BUF_SIZE]), MIN_TX_DATA);
        assert_eq!(allocated.io_mut().write_at(0, &[WRITE_BYTE; ALLOC_SIZE]), ALLOC_SIZE);
        assert_eq!(allocated.len(), MIN_TX_DATA);
        const READ_BUF_SIZE: usize = MIN_TX_DATA + 1;
        let mut read_buf = [READ_SENTINAL_BYTE; READ_BUF_SIZE];
        assert_eq!(allocated.io().read_at(0, &mut read_buf[..]), MIN_TX_DATA);
        assert_eq!(allocated.io().read_at(0, &mut read_buf[..MIN_TX_DATA]), MIN_TX_DATA);
        assert_eq!(&read_buf[..ALLOC_SIZE], &[WRITE_BYTE; ALLOC_SIZE][..]);
        assert_eq!(&read_buf[ALLOC_SIZE..MIN_TX_DATA], &[WRITE_BYTE; ALLOC_SIZE][..]);
        assert_eq!(&read_buf[MIN_TX_DATA..], &[READ_SENTINAL_BYTE; 1][..]);
    }

    #[test]
    fn invalid_tx_length() {
        let mut config = DEFAULT_CONFIG;
        config.buffer_layout.length = usize::from(u16::MAX) + 2;
        config.buffer_layout.min_tx_head = 0;
        let (pool, _descriptors, _vmo) = Pool::new(config).expect("failed to create pool");
        assert_matches!(pool.alloc_tx_buffer(1).now_or_never(), Some(Err(Error::TxLength)));
    }

    #[test]
    fn rx_leases() {
        let mut executor = fuchsia_async::TestExecutor::new();
        let state = RxLeaseHandlingState::new_with_enabled(true);
        let mut watcher = RxLeaseWatcher { state: &state };

        {
            let mut fut = pin!(watcher.wait_until(0));
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
        }
        {
            state.rx_complete();
            let mut fut = pin!(watcher.wait_until(1));
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
        }
        {
            let mut fut = pin!(watcher.wait_until(0));
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
        }
        {
            let mut fut = pin!(watcher.wait_until(3));
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Pending);
            state.rx_complete();
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Pending);
            state.rx_complete();
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
        }
        // Dropping the wait future without seeing it complete restores the
        // value.
        let counter_before = state.rx_frame_counter.load(atomic::Ordering::SeqCst);
        {
            let mut fut = pin!(watcher.wait_until(10000));
            assert_eq!(executor.run_until_stalled(&mut fut), Poll::Pending);
        }
        let counter_after = state.rx_frame_counter.load(atomic::Ordering::SeqCst);
        assert_eq!(counter_before, counter_after);
    }
}
