// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This is a lockless ring buffer modeled after https://docs.kernel.org/trace/ring-buffer-design.html
//!
//! See `design.md` for the design.
use fuchsia_rcu::RcuDroppable;
use fuchsia_runtime::vmar_root_self;
use fuchsia_trace;
use shared_buffer::SharedBuffer;
use starnix_logging::{log_error, log_info, log_warn};
use starnix_sync::{LockDepMutex, PerfRingBufferStateLock};
use starnix_types::PAGE_SIZE;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error, from_status_like_fdio};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::tail_val::{AtomicTailVal, TailVal};

/// Callback passed to `OutputBuffer::write_each()`. Matches starnix_core's `OutputBufferCallback`.
pub type OutputBufferCallback<'a> = dyn FnMut(&mut [MaybeUninit<u8>]) -> Result<usize, Errno> + 'a;

/// The subset of starnix_core's `OutputBuffer` that `LocklessRingBuffer::read()` uses.
pub trait OutputBuffer {
    /// Returns the number of bytes that can still be written.
    fn available(&self) -> usize;
    /// Calls `callback` with each writable segment, and returns the number of bytes written.
    fn write_each(&mut self, callback: &mut OutputBufferCallback<'_>) -> Result<usize, Errno>;
}

#[derive(RcuDroppable)]
struct Node {
    // page_data is the memory view for this page.
    page_data: SharedBuffer,
    // next and prev are pointers to the next and previous nodes.
    // creating a linked list of pages.
    next: AtomicUsize,
    prev: AtomicUsize,
    // Offset of the next free byte in `page_data`, with `WRITE_OFFSET_SEALED_BIT` set once the page
    // is sealed. Writers claim space with a CAS in `try_reserve_on_page()`, `finalize()` seals the
    // page, and resetting the page for reuse stores `PAGE_HEADER_SIZE`, which also unseals it.
    //
    // Memory ordering: every access is Relaxed. A claim and the seal are both read-modify-writes,
    // and each read-modify-write reads the last value before it in this field's modification
    // order, whatever the memory ordering. A plain load has no such guarantee and can return an
    // older value.
    //
    // Synchronizes with: nothing.
    //
    // Data ordering: no other data is published through this field. Records and `data_size`
    // reach the reader through `active_writers` (see `finalize()`). The plain loads below must
    // still see other threads' writes to this field. Each gets a happens-before edge from another
    // atomic, which is enough: a load cannot return a value older than one written or read by an
    // access that happens before it (coherence).
    //
    // The load in `release_writer()` must see the claims of the writers that released the page
    // before it. For example, Writer A is the last to release a page that Writer B filled:
    //
    //   1. Writer A claims, moving `write_offset` to `PAGE_SIZE - 8`.
    //   2. Writer B claims, moving `write_offset` to `PAGE_SIZE`.
    //   3. Writer B: `active_writers.fetch_sub(1, Release)`, writer count 2 -> 1.
    //   4. Writer A: `active_writers.fetch_sub(1, Release)`, writer count 1 -> 0.
    //   5. Writer A, now the last writer: `fence(Acquire)`.
    //   6. Writer A: `write_offset.load(Relaxed)`, to check whether the page is full.
    //
    // Step 4 reads the value that step 3 wrote, and step 5 is an acquire fence after step 4, so
    // step 3 synchronizes with step 5. Then step 2 happens before step 6, so by coherence step 6
    // cannot return the older `PAGE_SIZE - 8`, and Writer A finalizes the full page. Therefore
    // Relaxed is enough for the claims and the load. The seal in `finalize()` is a
    // read-modify-write, so it needs no such edge.
    //
    // A writer on a new tail page must see the page's reset. If its load in
    // `try_reserve_on_page()` returned the offset from the page's last use, the writer could
    // treat the page as full and finalize it:
    //
    //   1. Thread T, in `advance_to_next_page()`, resets `write_offset` to `PAGE_HEADER_SIZE`,
    //      or sees that another thread already reset it.
    //   2. Thread T moves the tail: `tail_page.compare_exchange(.., AcqRel)`.
    //   3. Writer W, in `reserve()`: `tail_page.load(Acquire)` returns the new tail.
    //   4. Writer W, in `try_reserve_on_page()`: `write_offset.load(Relaxed)`.
    //
    // Step 3 reads the value that step 2 wrote, so step 2 synchronizes with step 3. Then step 1
    // happens before step 4, so by coherence step 4 returns the reset value or a later one.
    // Therefore Relaxed is enough for the resets and the load. `enable()` uses `ref_count` in
    // place of `tail_page`: it resets every page before its Release store of `ref_count`, and
    // `reserve()` increments `ref_count` with Acquire, so a writer that enters after `enable()`
    // synchronizes with that store.
    write_offset: AtomicUsize,
    // Number of active writers currently reserving or writing to this page.
    active_writers: AtomicUsize,
}

const FLAG_MASK: usize = 0b11 << 62;
// Normal link between pages in the circular list.
const FLAG_NORMAL: usize = 0b00 << 62;
// Marks the link pointing to the head page (the oldest page with data).
const FLAG_HEADER: usize = 0b01 << 62;
// Set on the link pointing to the head page when it is being swapped by the reader.
const FLAG_UPDATE: usize = 0b10 << 62;

// The highest bit (bit 63) of `active_writers` is used as a flag indicating if the page is active.
// When the page is finalized, this bit is cleared to 0.
const PAGE_ACTIVE_BIT: usize = 1 << 63;
// The second highest bit (bit 62) of `active_writers` is used to coordinate/claim finalization.
const PAGE_FINALIZED_BIT: usize = 1 << 62;
const FLAGS_MASK: usize = PAGE_ACTIVE_BIT | PAGE_FINALIZED_BIT;
const ACTIVE_WRITERS_MASK: usize = !FLAGS_MASK;

// Bit 63 of `write_offset` marks the page as sealed: it accepts no new reservations.
// `Node::finalize()` sets this bit in the same read-modify-write that reads the page's final
// offset, and `try_reserve_on_page()` rejects a sealed page. Offsets never exceed `PAGE_SIZE`, so
// they never reach this bit.
//
// Ftrace closes the same window differently. The writer that does not fit overshoots `write`
// with `local_add_return()`, and `rb_reset_tail()` pads the rest of the page and leaves `write`
// at the end of the page, so that no later writer can slip in. Claims here are a bounded CAS that
// never overshoots, so the page needs an explicit marker. `Node::unsealed()` plays the role of
// ftrace's `RB_WRITE_MASK`.
const WRITE_OFFSET_SEALED_BIT: usize = 1 << 63;

/// Encodes a `TIME_STAMP` record that sets the absolute time of the record after it to
/// `ts_nanos`. The low 27 bits go in the header's time delta field and the rest in the next u32.
/// This is the layout Linux 4.17+ writes and Perfetto's `CpuReader::ParsePagePayload` reads.
fn encode_time_stamp(ts_nanos: u64) -> [u8; LocklessRingBuffer::TIME_STAMP_RECORD_SIZE] {
    // Saturate, so that a timestamp too large for 59 bits can't wrap to a smaller one.
    let ts_nanos = ts_nanos.min(LocklessRingBuffer::MAX_TIME_STAMP_NANOS);
    let low = (ts_nanos & LocklessRingBuffer::MAX_TIME_DELTA_NANOS) as u32;
    let high = (ts_nanos >> LocklessRingBuffer::TIME_DELTA_BITS) as u32;
    let header = (low << LocklessRingBuffer::TYPE_OR_LENGTH_BITS)
        | LocklessRingBuffer::RINGBUF_TYPE_TIME_STAMP;
    let mut bytes = [0u8; LocklessRingBuffer::TIME_STAMP_RECORD_SIZE];
    bytes[0..4].copy_from_slice(&header.to_le_bytes());
    bytes[4..8].copy_from_slice(&high.to_le_bytes());
    bytes
}

// When yielding under high contention, if we busy-spin we risk priority inversion and CPU starvation
// of preempted writers. Sleeping for 50 microseconds gives the OS scheduler sufficient window (accounting
// for 2-5us context switch overhead and 15-30us thread execution time) to reschedule and execute the
// preempted writer so it can release its reservation.
const SPIN_SLEEP_DURATION: std::time::Duration = std::time::Duration::from_micros(50);

// When progressive sleep is triggered (exceeding 1000 yields), we back off with a 10 milliseconds sleep.
// This is a robust scheduling window to guarantee that Zircon schedules the preempted writer thread
// to complete its commit/release logic.
const PROGRESSIVE_SLEEP_DURATION: std::time::Duration = std::time::Duration::from_millis(10);

impl Node {
    /// Returns the byte offset in a raw `write_offset` value, without `WRITE_OFFSET_SEALED_BIT`.
    fn unsealed(raw_write_offset: usize) -> usize {
        raw_write_offset & !WRITE_OFFSET_SEALED_BIT
    }

    fn get_index(val: usize) -> usize {
        val & !FLAG_MASK
    }
    fn get_flags(val: usize) -> usize {
        val & FLAG_MASK
    }
    fn make_val(index: usize, flags: usize) -> usize {
        index | flags
    }

    /// Finalizes the page by sealing `write_offset` and writing the data size to the page header.
    fn finalize(&self) {
        // Attempt to claim the finalization role for this page.
        // If another thread already claimed it, return early without touching `write_offset` or
        // `page_data`, so a losing finalizer has no side effects. A page reset clears
        // `PAGE_FINALIZED_BIT`. See the TODO in `advance_to_next_page()`.
        //
        // - Acquire: pairs with the Release store of `PAGE_ACTIVE_BIT` in `advance_to_next_page()`
        //   that recycled this page. It is meant to order this thread's `data_size` write below
        //   after the recycle's clear of `data_size`, so that the clear cannot overwrite it.
        //   TODO(https://fxbug.dev/570251032): The recycle clears `data_size` after that store,
        //   so this pairing does not order the clear yet. A recycler preempted between the two
        //   can zero a finalized page's `data_size`, and `swap_reader_page()` then returns `None`
        //   for that page forever.
        //   This thread needs no edge from the writers' Release `fetch_sub`: the seal below is a
        //   read-modify-write (see `Node::write_offset`), and the reader acquires
        //   `active_writers` itself before it reads the records.
        // - Release: orders nothing. Nothing this thread wrote before the `fetch_or` is read
        //   through it, and as a read-modify-write it continues the writers' release sequences
        //   whatever its ordering. The `fetch_and` below publishes `data_size`.
        //   TODO(https://fxbug.dev/570251032): Relax to Acquire.
        let old_val = self.active_writers.fetch_or(PAGE_FINALIZED_BIT, Ordering::AcqRel);
        if old_val & PAGE_FINALIZED_BIT != 0 {
            return;
        }

        // Seal `write_offset` and read the final offset in the same read-modify-write. Every
        // claim in `try_reserve_on_page()` that comes before this RMW in `write_offset`'s
        // modification order is included in the offset, and every claim after it reads the
        // sealed bit and fails. Relaxed: see `Node::write_offset`.
        let write_offset =
            Node::unsealed(self.write_offset.fetch_or(WRITE_OFFSET_SEALED_BIT, Ordering::Relaxed));

        debug_assert!(
            (LocklessRingBuffer::PAGE_HEADER_SIZE..=(*PAGE_SIZE) as usize).contains(&write_offset)
        );
        let data_size = write_offset - LocklessRingBuffer::PAGE_HEADER_SIZE;
        self.page_data.write_at(8, &(data_size as u64).to_le_bytes());

        // Clear `PAGE_ACTIVE_BIT` to publish the completed page.
        // - Release: orders the `data_size` write above before this store. The reader's Acquire
        //   load of `active_writers` in `swap_reader_page()` waits until `PAGE_ACTIVE_BIT` is clear
        //   and the writer count is 0. Every later change to `active_writers` is a read-modify-write
        //   (a writer's `fetch_sub` in `release_writer()`), so the value the reader sees belongs to
        //   the release sequence headed by this `fetch_and`. The reader therefore synchronizes with
        //   this `fetch_and`, and sees `data_size`, even when the last writer releases after it.
        //   Likewise, each writer's Release `fetch_sub` heads a release sequence that the reader's
        //   load reads from, so the reader also sees every record.
        self.active_writers.fetch_and(!PAGE_ACTIVE_BIT, Ordering::Release);
    }

    /// Releases a writer reservation on this node, and finalizes the node if it was the last writer.
    fn release_writer(&self) {
        // Release: orders this writer's claim and record writes before the decrement. It pairs
        // with the `fence(Acquire)` below in whichever writer releases the page last, so that
        // writer's `write_offset` load sees this claim. It also pairs with the Acquire loads of
        // `active_writers` in `swap_reader_page()`, so the reader sees the record, and in
        // `advance_to_next_page()`, so the page is reused only after the record is written.
        let prev_writers = self.active_writers.fetch_sub(1, Ordering::Release);

        // If this was the last writer finalizing an active page (`PAGE_ACTIVE_BIT | 1`), we ensure the page is finalized.
        // Note that if the page was already finalized by the advancing thread, `PAGE_FINALIZED_BIT` will be set,
        // meaning `prev_writers` will have it set and we won't enter this branch, which is correct since it's already finalized.
        if prev_writers == PAGE_ACTIVE_BIT | 1 {
            // Acquire: pairs with the Release `fetch_sub` of every writer that released before us.
            // Until the page is reset, only read-modify-writes change `active_writers`, so our
            // `fetch_sub` reads from their release sequences and their claims happen before the
            // load below. Without the fence, the load can return an older value, such as our own
            // claim, and skip finalizing a page that another writer filled.
            std::sync::atomic::fence(Ordering::Acquire);

            // Relaxed: the fence above makes the other writers' claims happen before this load.
            // See `Node::write_offset`. The load can also return the seal of a `finalize()` that
            // claimed the page after our `fetch_sub`. Then the call below loses the claim and does
            // nothing.
            let write_offset = Node::unsealed(self.write_offset.load(Ordering::Relaxed));
            if write_offset >= (*PAGE_SIZE) as usize {
                // Since this thread holds a valid `Reservation`, `ref_count` remains > 0, which blocks
                // `disable()` from completing and shrinking the VMO. This guarantees that the ring VMO
                // remains enabled and valid for memory access.
                // Without this check and finalization on failure, there is a liveness bug where a full page
                // could never be finalized because the successful writer already skipped finalization.
                self.finalize();
            }
        }
    }
}

/// Points in `try_reserve_on_page()` where a test can run code, to force an interleaving that
/// another thread would otherwise have to hit by chance.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestHook {
    /// After the claim is sized and before its CAS.
    BeforeClaim,
    /// After the claim CAS succeeds and before the record is stamped.
    AfterClaim,
}

#[cfg(test)]
thread_local! {
    /// The hook armed by `set_test_hook()`. It is thread-local, so tests that run in parallel
    /// don't see each other's hooks.
    static TEST_HOOK: std::cell::RefCell<Option<(TestHook, Box<dyn FnOnce(&LocklessRingBuffer)>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Arms `hook` to run once, on this thread, the next time it reaches `point`.
#[cfg(test)]
fn set_test_hook(point: TestHook, hook: impl FnOnce(&LocklessRingBuffer) + 'static) {
    TEST_HOOK.with(|h| *h.borrow_mut() = Some((point, Box::new(hook))));
}

/// Returns whether a hook is armed on this thread. Tests check that it is not after the code
/// under test runs, which shows that the hook fired.
#[cfg(test)]
fn test_hook_is_armed() -> bool {
    TEST_HOOK.with(|h| h.borrow().is_some())
}

/// Runs and disarms the hook armed for `point`, if any. The hook is disarmed before it runs, so
/// it can call back into `buffer`.
#[cfg(test)]
fn run_test_hook(point: TestHook, buffer: &LocklessRingBuffer) {
    let hook = TEST_HOOK.with(|h| {
        let mut h = h.borrow_mut();
        if h.as_ref().is_some_and(|(p, _)| *p == point) { h.take() } else { None }
    });
    if let Some((_, hook)) = hook {
        hook(buffer);
    }
}

// We are only 64 bit today, but make it easy to find this assumption if we go to a smaller arch.
static_assertions::const_assert!(std::mem::size_of::<usize>() == 8);

// Use the high bit to indicate the ring is enabled. This makes the default, 0,
// which is disabled, an easy value to reason about. And the ref count is the lower 63 bits.
const RING_ENABLED_BIT: usize = 1 << 63;

#[derive(RcuDroppable)]
pub struct LocklessRingBuffer {
    vmo: zx::Vmo,
    mapping: SharedBuffer,
    // linked list of pages that represent the ring, backed by the mapping (and the vmo).
    nodes: Vec<Node>,
    // The page where to read the data from the ring.
    head_page: AtomicUsize,
    // The page where writes add to the ring, tagged with a generation. See `TailVal`.
    tail_page: AtomicTailVal,

    // A page used by the reader of the ring.
    reader_page: AtomicUsize,

    // Tracks the global number of active readers and writers of the ring in the lower 63 bits.
    // The highest bit (RING_ENABLED_BIT) is set when the ring is enabled.
    ref_count: AtomicUsize,
    // True indicates drop old pages to write new data when the ring is full. If false, writes fail until
    // the data is read, effectively dropping new data and keeping the old data.
    overwrite: bool,
    // Number of dropped pages when overwrite is true.
    dropped_pages: std::sync::atomic::AtomicU64,
    // Used to calculate the delta from the last event. Since this is written to by multiple threads,
    // we keep an atomic value for this.
    prev_timestamp: std::sync::atomic::AtomicU64,
    // The async trace event ID for write events.
    write_event_async_id: fuchsia_trace::Id,
    // Tracks whether a reader is currently active, used to validate that concurrent reads
    // on a single ring buffer are not supported and do not happen. This can be removed if we
    // are convinced it is not necessary.
    reader_active: std::sync::atomic::AtomicBool,
    // Mutex to serialize enable() and disable() calls to prevent racing. This should
    // never be accessed by other methods to avoid locking during reading and writing.
    // It holds the start time of the current session.
    state_mutex: LockDepMutex<zx::BootInstant, PerfRingBufferStateLock>,
}
impl LocklessRingBuffer {
    // This is an Ftrace page header consisting of a u64 timestamp at offset 0 and a u64 data size at offset 8.
    pub const PAGE_HEADER_SIZE: usize = 16;

    // An ftrace record header is a u32: the low `TYPE_OR_LENGTH_BITS` hold `type_or_length`, and
    // the high `TIME_DELTA_BITS` hold the time delta from the previous record, in nanoseconds.

    /// The number of low bits of an ftrace record header that hold `type_or_length`.
    pub const TYPE_OR_LENGTH_BITS: u32 = 5;
    /// The number of high bits of an ftrace record header that hold the time delta. A `TIME_STAMP`
    /// record puts the low `TIME_DELTA_BITS` of its time there.
    pub const TIME_DELTA_BITS: u32 = 27;
    /// The largest time delta, in nanoseconds, that a record header holds.
    pub const MAX_TIME_DELTA_NANOS: u64 = (1 << Self::TIME_DELTA_BITS) - 1;
    /// The `type_or_length` of a `TIME_STAMP` record (`RINGBUF_TYPE_TIME_STAMP` in Linux
    /// include/linux/ring_buffer.h).
    pub const RINGBUF_TYPE_TIME_STAMP: u32 = 31;
    /// The size of a `TIME_STAMP` record: the header and a u32 with the high bits of the time.
    pub const TIME_STAMP_RECORD_SIZE: usize = 8;
    // The largest time a `TIME_STAMP` record holds: 27 bits in the header and 32 in the next u32.
    const MAX_TIME_STAMP_NANOS: u64 = (1 << (Self::TIME_DELTA_BITS + u32::BITS)) - 1;

    /// Creates a new LocklessRingBuffer.
    /// size_bytes: Size of the ring buffer. Must be at least PAGE_SIZE * 3 and will be rounded up to a multiple of PAGE_SIZE.
    /// overwrite: If true, old pages will be dropped when the ring is full. If false, writes will fail until the data is read.
    /// write_event_async_id: The async trace event ID for write events.
    pub fn new(
        size_bytes: usize,
        overwrite: bool,
        write_event_async_id: fuchsia_trace::Id,
    ) -> Result<Self, Errno> {
        let requested_pages = size_bytes.div_ceil((*PAGE_SIZE) as usize);
        // 3 pages are needed, 1 for the read page, 1 for head, and one to swap.
        let pages = std::cmp::max(3, requested_pages);
        let total_nodes = pages;
        // Every node index must fit in the index field of `tail_page`.
        if total_nodes > TailVal::MAX_NODES {
            return error!(ENOMEM);
        }
        let capacity = total_nodes * (*PAGE_SIZE) as usize;

        // Create VMO
        let vmo = starnix_logging::with_zx_name(
            zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, capacity as u64)
                .map_err(|_| errno!(ENOMEM))?,
            b"starnix:tracefs",
        );
        // Map VMO
        let addr = vmar_root_self()
            .map(0, &vmo, 0, capacity, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .map_err(|e| from_status_like_fdio!(e))?;
        // SAFETY: The address returned by `vmar.map` is valid for `capacity` bytes.
        let mapping = unsafe { SharedBuffer::new(addr as *mut u8, capacity) };

        // Create nodes
        let mut nodes = Vec::with_capacity(total_nodes);
        let base_ptr = addr as *mut u8;
        for i in 0..total_nodes {
            // SAFETY: `base_ptr` points to a valid mapping of size `capacity`.
            // `i * PAGE_SIZE` is within the bounds of this mapping since `i < total_nodes`.
            // The memory range `[page_ptr, page_ptr + PAGE_SIZE)` is valid and mapped.
            let page_ptr = unsafe { base_ptr.add(i * (*PAGE_SIZE) as usize) };

            nodes.push(Node {
                // SAFETY: `page_ptr` is in bounds of the mapped region and valid for `PAGE_SIZE` bytes.
                page_data: unsafe { SharedBuffer::new(page_ptr, (*PAGE_SIZE) as usize) },
                next: AtomicUsize::new(0),
                prev: AtomicUsize::new(0),
                write_offset: AtomicUsize::new(LocklessRingBuffer::PAGE_HEADER_SIZE),
                // Initialize active_writers to PAGE_ACTIVE_BIT, with no writers, since the page is
                // active.
                active_writers: AtomicUsize::new(PAGE_ACTIVE_BIT),
            });
        }
        // Link first `pages - 1` nodes in a circle. The last page is reserved for the reader initialization.
        let circle_size = pages - 1;
        for i in 0..circle_size {
            let next_idx = (i + 1) % circle_size;
            let prev_idx = (i + circle_size - 1) % circle_size;
            nodes[i].next.store(Node::make_val(next_idx, FLAG_NORMAL), Ordering::Relaxed);
            nodes[i].prev.store(Node::make_val(prev_idx, FLAG_NORMAL), Ordering::Relaxed);
        }
        // Initialize the reader page, mark head page (node 0).
        nodes[circle_size - 1].next.store(Node::make_val(0, FLAG_HEADER), Ordering::Relaxed);
        // Initialize reader page pointers to point to the head page and its predecessor
        nodes[pages - 1].next.store(Node::make_val(0, FLAG_NORMAL), Ordering::Relaxed);
        nodes[pages - 1]
            .prev
            .store(Node::make_val(circle_size - 1, FLAG_NORMAL), Ordering::Relaxed);
        // Initialize reader page size to 0
        nodes[pages - 1].page_data.write_at(8, &0u64.to_le_bytes());
        let now = zx::BootInstant::get();
        let buffer = Self {
            vmo,
            mapping,
            nodes,
            head_page: AtomicUsize::new(0),
            tail_page: AtomicTailVal::new(TailVal::INITIAL),
            reader_page: AtomicUsize::new(pages - 1),

            ref_count: AtomicUsize::new(RING_ENABLED_BIT),
            overwrite,
            dropped_pages: std::sync::atomic::AtomicU64::new(0),
            prev_timestamp: std::sync::atomic::AtomicU64::new(now.into_nanos() as u64),
            write_event_async_id,
            reader_active: std::sync::atomic::AtomicBool::new(false),
            state_mutex: LockDepMutex::new(now),
        };
        Ok(buffer)
    }
    #[cfg(test)]
    fn dropped_pages(&self) -> u64 {
        self.dropped_pages.load(Ordering::Relaxed)
    }
}
// Debugging support for detecting live locks. Consider removing once
// the ring has baked for some time.
#[derive(Default, Debug)]
struct YieldTracker {
    /// Number of times we yielded because the next page was being updated by the reader.
    update_flag: u64,
    /// Number of times we yielded because the page to overwrite still had active writers.
    node_match: u64,
    /// Number of times we yielded because we failed to lock the head page for overwrite.
    head_lock: u64,
}
impl YieldTracker {
    fn total(&self) -> u64 {
        self.update_flag + self.node_match + self.head_lock
    }

    /// Yields or sleeps progressively based on the total retry count.
    fn yield_or_sleep(&self) {
        let total = self.total();
        if total > 1000 {
            std::thread::sleep(PROGRESSIVE_SLEEP_DURATION);
        } else if total > 100 {
            std::thread::sleep(SPIN_SLEEP_DURATION);
        } else {
            std::thread::yield_now();
        }
    }
}

/// Represents a reserved region of the ring buffer that is allocated, but not committed yet.
pub struct Reservation<'a> {
    pub offset: usize,
    pub node_idx: usize,
    pub size: usize,
    /// See `header_time_delta()`.
    header_time_delta: u32,
    buffer: &'a LocklessRingBuffer,
    committed: bool,
}

impl<'a> Reservation<'a> {
    /// Returns the value, in nanoseconds, for the time delta field of the record header written
    /// into this reservation. It is at most `MAX_TIME_DELTA_NANOS`. It is 0 when a `TIME_STAMP`
    /// record written before the reservation carries the record's absolute time, and for the
    /// first record on a page, whose time the page header carries.
    pub fn header_time_delta(&self) -> u32 {
        self.header_time_delta
    }

    pub fn write_at(&self, rel_offset: usize, data: &[u8]) {
        assert!(rel_offset + data.len() <= self.size, "Write exceeds reservation size");
        self.buffer.mapping.write_at(self.offset + rel_offset, data);
    }

    fn release(&mut self) {
        if self.committed {
            return;
        }

        let node_idx = self.node_idx;
        let node = &self.buffer.nodes[node_idx];

        node.release_writer();

        self.buffer.ref_count.fetch_sub(1, Ordering::Release);
        self.committed = true;
    }
}

impl<'a> Drop for Reservation<'a> {
    fn drop(&mut self) {
        if !self.committed {
            starnix_logging::log_warn!("LocklessRingBuffer: Reservation dropped without commit");
        }
        self.release();
    }
}

impl<'a> std::fmt::Debug for Reservation<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("offset", &self.offset)
            .field("node_idx", &self.node_idx)
            .field("size", &self.size)
            .field("header_time_delta", &self.header_time_delta)
            .field("committed", &self.committed)
            .finish()
    }
}
#[derive(Debug, PartialEq, Eq)]
enum AdvanceResult {
    /// The tail page has been successfully advanced or transitioned.
    Advanced,
    /// Failed to advance due to transient contention. The caller should yield and retry.
    Yielded,
    /// A hard error occurred (e.g. buffer is full and overwrite is disabled).
    Error(Errno),
}

/// A reservation made by `try_reserve_on_page()`.
struct PageReservation {
    /// Offset in the page of the caller's record, after its `TIME_STAMP` record if it has one.
    offset: usize,
    /// The record's time.
    now: zx::BootInstant,
    /// The delta from the previous record, or 0 for the first record on a page.
    delta: zx::Duration<zx::BootTimeline>,
    /// See `Reservation::header_time_delta()`.
    header_time_delta: u32,
}

impl LocklessRingBuffer {
    /// Attempts to reserve space on the current tail page, stamping the record with the time from
    /// `current_boot_time`.
    ///
    /// Returns `Err(())` if the page is full or sealed.
    fn try_reserve_on_page(
        &self,
        tail_node: &Node,
        size: usize,
        current_boot_time: &impl Fn() -> zx::BootInstant,
    ) -> Result<PageReservation, ()> {
        // Atomically reserve space on the page using a bounded CAS loop.
        // By verifying that `WRITE_OFFSET_SEALED_BIT` is not set and that
        // `current_offset + total_size <= PAGE_SIZE` before executing the CAS, we guarantee that
        // failed reservations never advance `write_offset` and that no reservation can succeed
        // after `Node::finalize()` seals the page. A CAS that loses to the seal returns the sealed
        // value, so the next iteration fails without retrying.
        //
        // A record more than `MAX_TIME_DELTA_NANOS` after the previous one can't store its delta
        // in its header, so a `TIME_STAMP` record goes in front of it, and the claim includes it.
        // Each attempt reads the clock and `prev_timestamp` again, so a writer that lost the CAS
        // after a preemption decides with a current time, not one older than `prev_timestamp`.
        // Relaxed: see `Node::write_offset`.
        let mut current_offset = tail_node.write_offset.load(Ordering::Relaxed);
        let (now_nanos, time_stamp_size) = loop {
            if (current_offset & WRITE_OFFSET_SEALED_BIT) != 0 {
                return Err(());
            }
            let now_nanos = current_boot_time().into_nanos() as u64;
            // Relaxed: only the value is used, to size the claim. See the `debug_assert!` below.
            let prev_estimate = self.prev_timestamp.load(Ordering::Relaxed);
            // The first record on a page takes its time from the page header and never needs a
            // `TIME_STAMP` record, so any `size` that `reserve()` accepts fits on an empty page.
            let time_stamp_size = if current_offset != LocklessRingBuffer::PAGE_HEADER_SIZE
                && now_nanos.saturating_sub(prev_estimate)
                    > LocklessRingBuffer::MAX_TIME_DELTA_NANOS
            {
                LocklessRingBuffer::TIME_STAMP_RECORD_SIZE
            } else {
                0
            };
            let total_size = size + time_stamp_size;
            if current_offset + total_size > (*PAGE_SIZE) as usize {
                return Err(());
            }
            #[cfg(test)]
            run_test_hook(TestHook::BeforeClaim, self);
            match tail_node.write_offset.compare_exchange_weak(
                current_offset,
                current_offset + total_size,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break (now_nanos, time_stamp_size),
                Err(actual) => current_offset = actual,
            }
        };

        #[cfg(test)]
        run_test_hook(TestHook::AfterClaim, self);

        let actual_prev = self.prev_timestamp.fetch_max(now_nanos, Ordering::AcqRel);
        let final_now_nanos = std::cmp::max(now_nanos, actual_prev);
        let now = zx::BootInstant::from_nanos(final_now_nanos as i64);

        // TODO(https://fxbug.dev/566957812): The claim and the `fetch_max` are separate steps, so
        // a writer preempted between them stamps after writers that claimed later. Its time is
        // then their later time, not its own clock read, and `delta_nanos` is measured from the
        // time that the `fetch_max` returns, not from the record before this one on the page. So,
        // by up to how long the writer was preempted, a record without a `TIME_STAMP` record can
        // decode early, a record with one or the first record on a page can decode late, and
        // records can decode out of order along the page.
        // First write on this page. Set timestamp.
        let delta_nanos = if current_offset == LocklessRingBuffer::PAGE_HEADER_SIZE {
            tail_node.page_data.write_at(0, &final_now_nanos.to_le_bytes());
            0
        } else {
            final_now_nanos.saturating_sub(actual_prev)
        };

        let header_time_delta = if time_stamp_size > 0 {
            // The `TIME_STAMP` record carries `final_now_nanos` as the record's absolute time, so
            // the record decodes to that time whatever order concurrent writers finish in. That
            // time can be late, as the TODO above describes. Write the record even if another
            // writer has raised `prev_timestamp` so that the delta would fit: the claimed bytes
            // must hold a valid record.
            tail_node.page_data.write_at(current_offset, &encode_time_stamp(final_now_nanos));
            0
        } else {
            // The `fetch_max` reads a value no older than `prev_estimate` (read-read coherence),
            // and `prev_timestamp` only grows while this thread holds `ref_count`: only `enable()`
            // lowers it, and only when `ref_count` is 0. So `delta_nanos` is at most
            // `now_nanos - prev_estimate`, which needed no `TIME_STAMP` record.
            debug_assert!(
                delta_nanos <= LocklessRingBuffer::MAX_TIME_DELTA_NANOS,
                "delta {delta_nanos} too large"
            );
            delta_nanos.min(LocklessRingBuffer::MAX_TIME_DELTA_NANOS) as u32
        };

        let delta = zx::Duration::from_nanos(delta_nanos as i64);

        Ok(PageReservation {
            offset: current_offset + time_stamp_size,
            now,
            delta,
            header_time_delta,
        })
    }

    /// Advances the tail page to the next page in the ring.
    ///
    /// Handles marking the current page as full, moving the head page if overwrite is enabled,
    /// and updating the global `tail_page` pointer.
    fn advance_to_next_page(
        &self,
        tail_node: &Node,
        tail_val: TailVal,
        yield_tracker: &mut YieldTracker,
    ) -> AdvanceResult {
        // Short-circuit if another thread has already advanced the tail page past tail_val.
        if self.tail_page.load(Ordering::Acquire) != tail_val {
            return AdvanceResult::Advanced;
        }

        // Finalize the tail page: seal it against further reservations and publish its size.
        // Writers that already hold reservations on it may still be writing. The reader waits
        // for them through `active_writers`.
        //
        // TODO(https://fxbug.dev/570251032): A thread preempted between the `tail_page` check
        // above and this call can make the call after `tail_node` has been reset, either as a
        // later tail here or as a returned reader page in `swap_reader_page()`. The reset clears
        // `PAGE_FINALIZED_BIT`, so this call wins the claim and closes the page. If the page is
        // already the tail, it is closed live, possibly with `data_size == 0`. Once that page is
        // the head, `swap_reader_page()` returns `None` forever, so the reader stalls and,
        // without overwrite, writers fail with `ENOSPC`. If the tail has not reached the page
        // yet, the page is sealed while empty, and the reset below reopens it when the tail
        // arrives. Below, the same thread can also find `FLAG_HEADER` on `tail_node.next` and
        // drop the head page, or fail with `ENOSPC`, while the ring still has free pages.
        if self.is_enabled() {
            tail_node.finalize();
        }

        let next_val = tail_node.next.load(Ordering::Acquire);
        let next_idx = Node::get_index(next_val);
        let next_flags = Node::get_flags(next_val);
        if next_flags == FLAG_UPDATE {
            // Reader is swapping this page. Wait.
            starnix_logging::log_warn!(
                "Reservation yielding due to FLAG_UPDATE on node {}",
                next_idx
            );
            yield_tracker.update_flag += 1;
            yield_tracker.yield_or_sleep();
            return AdvanceResult::Yielded;
        } else if next_flags == FLAG_HEADER {
            if self.overwrite {
                // Check if any active writer is on the page we want to overwrite.
                if (self.nodes[next_idx].active_writers.load(Ordering::Acquire)
                    & ACTIVE_WRITERS_MASK)
                    > 0
                {
                    yield_tracker.node_match += 1;
                    yield_tracker.yield_or_sleep();
                    return AdvanceResult::Yielded;
                }

                starnix_logging::log_warn!(
                    "LocklessRingBuffer Overwriting page {} (overwriting={})",
                    next_idx,
                    self.overwrite
                );
                // Try to lock the head page
                let expected_next = Node::make_val(next_idx, FLAG_HEADER);
                let locked_next = Node::make_val(next_idx, FLAG_UPDATE);
                // AcqRel on success:
                // - Acquire: Ensures we see the up-to-date links of the head page we are about to move.
                // - Release: Ensures our transition to FLAG_UPDATE (locking) is visible to others.
                // Relaxed on failure: We fail to acquire the lock and just retry.
                if tail_node
                    .next
                    .compare_exchange_weak(
                        expected_next,
                        locked_next,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // Move the head page.
                    let head_node = &self.nodes[next_idx];
                    let head_next_val = head_node.next.load(Ordering::Acquire);
                    let head_next_idx = Node::get_index(head_next_val);

                    // Set FLAG_HEADER on the new head pointer
                    let new_head_next = Node::make_val(head_next_idx, FLAG_HEADER);
                    head_node.next.store(new_head_next, Ordering::Release);

                    // Update global head_page
                    self.head_page.store(head_next_idx, Ordering::Release);

                    // Unlock the old head pointer (tail_node.next) and make it FLAG_NORMAL
                    let unlocked_next = Node::make_val(next_idx, FLAG_NORMAL);
                    tail_node.next.store(unlocked_next, Ordering::Release);
                    self.dropped_pages.fetch_add(1, Ordering::Relaxed);
                    fuchsia_trace::async_instant!(
                        self.write_event_async_id,
                        "starnix:trace_meta",
                        "page dropped"
                    );
                    return AdvanceResult::Advanced;
                } else {
                    // Failed to lock head_page. Retry.
                    yield_tracker.head_lock += 1;
                    yield_tracker.yield_or_sleep();
                    return AdvanceResult::Yielded;
                }
            } else {
                // Buffer is full and overwrite is false.
                starnix_logging::log_error!("LocklessRingBuffer is full");
                return AdvanceResult::Error(errno!(ENOSPC));
            }
        }

        let next_node = &self.nodes[next_idx];
        // Only the thread whose CAS moves `next_node.write_offset` to `PAGE_HEADER_SIZE` (from a
        // used or sealed offset; the CAS also clears `WRITE_OFFSET_SEALED_BIT`) resets the rest
        // of the page. Any thread that still sees `tail_val` in `tail_page` then helps move the
        // tail. A page sealed while empty holds `PAGE_HEADER_SIZE | WRITE_OFFSET_SEALED_BIT`,
        // not `PAGE_HEADER_SIZE`, so it is reset here too, which reopens it.
        let (should_advance, won_reset) = {
            // Relaxed: see `Node::write_offset`.
            let mut current = next_node.write_offset.load(Ordering::Relaxed);
            loop {
                // Another thread advanced the tail page.
                if self.tail_page.load(Ordering::Acquire) != tail_val {
                    break (false, false);
                }
                // If `current` is exactly `PAGE_HEADER_SIZE` (unsealed), the page was already
                // reset: another thread won the reset race here, or `swap_reader_page()` or
                // `enable()` reset it. Since `self.tail_page` hasn't advanced yet, we must still
                // return `should_advance = true` to cooperate in advancing the global `tail_page`
                // pointer, while returning `won_reset = false` to avoid clobbering page metadata.
                if current == LocklessRingBuffer::PAGE_HEADER_SIZE {
                    break (true, false);
                }
                match next_node.write_offset.compare_exchange_weak(
                    current,
                    LocklessRingBuffer::PAGE_HEADER_SIZE,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break (true, true),
                    Err(actual) => current = actual,
                }
            }
        };

        if should_advance {
            if won_reset {
                // Reset `active_writers` to PAGE_ACTIVE_BIT (1) since the page is active.
                next_node.active_writers.store(PAGE_ACTIVE_BIT, Ordering::Release);
                // Clear the size field in the page header to avoid reader seeing stale size.
                next_node.page_data.write_at(8, &0u64.to_le_bytes());
            }

            // Move the tail to `next_idx` in the next generation. If the CAS fails, another
            // thread has already moved it.
            // - Release: a writer whose `tail_page.load(Acquire)` in `reserve()` reads this value
            //   synchronizes with this CAS. It therefore sees the reset of `write_offset` (see
            //   `Node::write_offset`) and, if this thread won the reset, the `active_writers`
            //   store and the header write above.
            // - Acquire: orders nothing. This function reads nothing after the CAS, and
            //   `reserve()` loads `tail_page` again with Acquire before it uses the new tail.
            //   TODO(https://fxbug.dev/570251032): Relax to Release.
            //
            // TODO(https://fxbug.dev/570251032): A thread that did not win the reset publishes
            // neither the `active_writers` store nor the header write, because it never read
            // them. If the winner was preempted after its `write_offset` CAS, it has not even
            // made them yet. A writer that reads the page's old `active_writers` value, with
            // `PAGE_ACTIVE_BIT` clear, skips the page and moves the tail past it. When the
            // winner's store lands, the page is active again behind the tail, and nothing
            // finalizes it. If the writer instead finalizes the page after that store, it closes
            // the page with `data_size == 0`. Either way, once the page is the head,
            // `swap_reader_page()` returns `None` forever: without overwrite the reader stalls and
            // writers fail with `ENOSPC`; with overwrite the head drop eventually discards the
            // page.
            if let Err(err) = self.tail_page.compare_exchange(
                tail_val,
                tail_val.next(next_idx),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                starnix_logging::log_debug!(
                    "Tail page already advanced by another thread: {err:?}"
                );
            }
        }
        AdvanceResult::Advanced
    }

    /// Reserves `size` bytes for a record.
    ///
    /// Returns the reservation, the record's time, and the delta from the previous record (0 for
    /// the first record on a page). The delta can exceed what a record header holds; write
    /// `Reservation::header_time_delta()` into the header instead.
    pub fn reserve(
        &self,
        size: usize,
    ) -> Result<(Reservation<'_>, zx::BootInstant, zx::Duration<zx::BootTimeline>), Errno> {
        self.reserve_with_clock(size, zx::BootInstant::get)
    }

    /// Like `reserve()`, but reads the current time from `current_boot_time`, so that tests can
    /// control timestamps.
    pub fn reserve_with_clock(
        &self,
        size: usize,
        current_boot_time: impl Fn() -> zx::BootInstant,
    ) -> Result<(Reservation<'_>, zx::BootInstant, zx::Duration<zx::BootTimeline>), Errno> {
        // Check that the reservation is non-zero and fits within a page.
        if size == 0 || size > (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE {
            return error!(EINVAL);
        }
        // Increment ref_count if enabled.
        let mut val = self.ref_count.load(Ordering::Acquire);
        loop {
            if val & RING_ENABLED_BIT == 0 {
                return error!(ENOMEM);
            }
            // Acquire on success:
            // - Acquire: Synchronizes with the Release store in `enable()`. This guarantees that if the writer
            //   observes that the ring is enabled, it will also observe all prior state initialization and memory
            //   setups made by `enable()` before starting any writes.
            // - Release: Synchronizes with `disable()`'s Acquire load on `ref_count`, ensuring that if `disable()`
            //   observes a zero active writer count, it is guaranteed that this writer has either not yet entered
            //   or has fully completed its operations.
            // Note: Only the decrement in `release()` needs to be Release to ensure that the data payload writes
            // are fully visible. Since the writer has not yet written any payload data to the page, there are no
            // memory writes that need to be published to other threads via Release ordering at this point.
            match self.ref_count.compare_exchange_weak(
                val,
                val + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => val = actual,
            }
        }

        // Create a scope guard to decrement ref_count on error, panic, or early exit.
        let ref_count_guard = scopeguard::guard(&self.ref_count, |ref_count| {
            ref_count.fetch_sub(1, Ordering::Release);
        });

        // Lock the reservation.
        let mut yield_tracker = YieldTracker::default();
        let result = loop {
            if !self.is_enabled() {
                log_info!("LocklessRingBuffer: canceling reserve because ring is disabled.");
                break error!(ENOMEM);
            }
            let tail_val = self.tail_page.load(Ordering::Acquire);
            let tail_idx = tail_val.index() as usize;
            let tail_node = &self.nodes[tail_idx];

            // Increment the writer count before trying to reserve space, if it fails decrement it.
            // We use compare_exchange_weak in a loop to ensure we only increment if the PAGE_ACTIVE_BIT is still set.
            // Note: The success and failure orderings are Relaxed because the writer has not yet written any payload
            // data to the page. Therefore, there are no memory writes that need to be published to other threads
            // via Release ordering.
            let mut active_incremented = false;
            let mut old_writers = tail_node.active_writers.load(Ordering::Acquire);
            loop {
                if old_writers & PAGE_ACTIVE_BIT == 0 {
                    break;
                }
                match tail_node.active_writers.compare_exchange_weak(
                    old_writers,
                    old_writers + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        active_incremented = true;
                        break;
                    }
                    Err(actual) => old_writers = actual,
                }
            }

            let reservation_result = if active_incremented {
                self.try_reserve_on_page(tail_node, size, &current_boot_time)
            } else {
                Err(())
            };

            // Try to claim space on the current page.
            match reservation_result {
                Ok(page) => {
                    break Ok((
                        Reservation {
                            offset: tail_idx * (*PAGE_SIZE) as usize + page.offset,
                            node_idx: tail_idx,
                            size,
                            header_time_delta: page.header_time_delta,
                            buffer: self,
                            committed: false,
                        },
                        page.now,
                        page.delta,
                    ));
                }
                Err(()) => {
                    // Handle the race where the last successful writer decrements active_writers but
                    // sees prev_writers > 1 because another concurrent writer (this thread) has already
                    // incremented it. This thread fails reservation because the page is full, decrements
                    // active_writers to 0, and gets prev_writers == 1. We must check and finalize here to
                    // guarantee that full pages are always finalized even if the finalizer thread failed its reserve.
                    if active_incremented {
                        self.nodes[tail_idx].release_writer();
                    }
                    // Page is full. Advance to the next page.
                    match self.advance_to_next_page(tail_node, tail_val, &mut yield_tracker) {
                        AdvanceResult::Advanced | AdvanceResult::Yielded => {}
                        AdvanceResult::Error(e) => {
                            break Err(e);
                        }
                    }
                }
            }
            // Debug logging for high yield counts.
            if yield_tracker.total() % 100_000_000 == 0 && yield_tracker.total() > 0 {
                log_warn!(
                    "LocklessRingBuffer: spinning in reservation loop.  details = {yield_tracker:?}",
                );
            }
        };
        // Cleanup and return
        let total_yields = yield_tracker.total();
        if total_yields > 500_000 {
            starnix_logging::log_info!(
                "Reservation completed with yields: total={}, details={:?}",
                total_yields,
                yield_tracker
            );
        }
        match result {
            Ok((res, now, delta)) => {
                // Success: Disarm the scope guard so ref_count is NOT decremented.
                scopeguard::ScopeGuard::into_inner(ref_count_guard);
                Ok((res, now, delta))
            }
            Err(e) => {
                // Error: The scope guard will automatically decrement ref_count when dropped.
                Err(e)
            }
        }
    }
    pub fn commit(&self, mut reservation: Reservation<'_>) {
        reservation.release();
    }

    fn swap_reader_page(&self) -> Option<usize> {
        let mut retries = 0;
        loop {
            let head_val = self.head_page.load(Ordering::Acquire);
            let head_idx = Node::get_index(head_val);
            let tail_val = self.tail_page.load(Ordering::Acquire);
            let tail_idx = tail_val.index() as usize;
            if head_idx == tail_idx {
                // Ring is empty or we are writing to the only page.
                return None;
            }
            let head_node = &self.nodes[head_idx];
            // Perform a single Acquire load on `active_writers` to establish happens-before with the finalizing writer.
            // We check concurrently that the page is finalized (PAGE_ACTIVE_BIT is cleared to 0) and that there are no active writers (count is 0).
            let active_writers = head_node.active_writers.load(Ordering::Acquire);
            // We can swap the page if it is no longer active and has no active writers.
            // We ignore the PAGE_FINALIZED_BIT when determining if the page is ready.
            if (active_writers & (PAGE_ACTIVE_BIT | ACTIVE_WRITERS_MASK)) != 0 {
                return None;
            }
            let mut size_bytes = [0u8; 8];
            head_node.page_data.read_at(8, &mut size_bytes);
            let size = u64::from_le_bytes(size_bytes);
            if size == 0 {
                // This can happen even if `head_page != tail_page` (the ring is not empty).
                // Specifically, if a writer advanced the tail to the next page, the global
                // `head_page` pointer may have advanced, but the active writer on this new
                // `head_page` has not yet called `commit()` or finalized the page.
                // Thus, the data size field at offset 8 in the page header remains 0.
                return None;
            }
            let next_val = head_node.next.load(Ordering::Acquire);
            let next_idx = Node::get_index(next_val);
            let prev_val = head_node.prev.load(Ordering::Acquire);
            let prev_idx = Node::get_index(prev_val);
            let prev_node = &self.nodes[prev_idx];
            // Try to lock the head page by setting UPDATE flag on prev.next
            let expected_next = Node::make_val(head_idx, FLAG_HEADER);
            let locked_next = Node::make_val(head_idx, FLAG_UPDATE);
            if prev_node
                .next
                .compare_exchange_weak(
                    expected_next,
                    locked_next,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // Locked! Now perform the swap.
                let reader_idx = self.reader_page.load(Ordering::Acquire);
                let next_node = &self.nodes[next_idx];
                // Update pointers to insert reader node.
                self.nodes[reader_idx]
                    .prev
                    .store(Node::make_val(prev_idx, FLAG_NORMAL), Ordering::Relaxed);
                self.nodes[reader_idx]
                    .next
                    .store(Node::make_val(next_idx, FLAG_HEADER), Ordering::Relaxed);
                next_node.prev.store(Node::make_val(reader_idx, FLAG_NORMAL), Ordering::Relaxed);
                // Reset write offset of the new head page (old reader page)
                self.nodes[reader_idx]
                    .write_offset
                    .store(LocklessRingBuffer::PAGE_HEADER_SIZE, Ordering::Relaxed);
                // Reset `active_writers` to PAGE_ACTIVE_BIT before recycling.
                self.nodes[reader_idx].active_writers.store(PAGE_ACTIVE_BIT, Ordering::Relaxed);
                // Clear the size field in the page header to avoid reader seeing stale size.
                self.nodes[reader_idx].page_data.write_at(8, &0u64.to_le_bytes());
                // Unlock prev_node and set its next to reader_idx as NORMAL
                let unlocked_val = Node::make_val(reader_idx, FLAG_NORMAL);
                prev_node.next.store(unlocked_val, Ordering::Release);
                // Update global pointers
                self.head_page.store(next_idx, Ordering::Release);
                self.reader_page.store(head_idx, Ordering::Release);
                // Return the old head page index (now reader page)
                return Some(head_idx);
            }
            // Failed to lock, retry.
            starnix_logging::log_warn!("swap_reader_page failed to lock, yielding");
            retries += 1;
            if retries >= 100_000 {
                starnix_logging::log_error!("LocklessRingBuffer: HUNG in swap_reader_page loop");
                return None;
            }
            // Failed to acquire the lock for swapping. Spin and retry.
            // Note: The log message above says "yielding" but we actually spin here
            // to avoid context switch overhead for this short wait.
            std::hint::spin_loop();
        }
    }
    pub fn read(&self, buf: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        // Validate that concurrent reads on a single ring buffer do not happen.
        // By design in both ftrace/tracefs and Starnix VFS, only a single reader is active at a time.
        if self.reader_active.swap(true, Ordering::AcqRel) {
            starnix_logging::log_error!(
                "LocklessRingBuffer: concurrent reads detected! Concurrent reads are not supported by design."
            );
            return error!(EBUSY);
        }

        let _lock_guard = scopeguard::guard(&self.reader_active, |reader_active| {
            reader_active.store(false, Ordering::Release);
        });

        // Increment ref_count if enabled to prevent disable() from shrinking the VMO
        // while we are reading from it. We use a compare_exchange loop instead of
        // fetch_add to avoid modifying the counter if the ring is already disabled,
        // which prevents live-locking disable().
        let mut val = self.ref_count.load(Ordering::Acquire);
        loop {
            if val & RING_ENABLED_BIT == 0 {
                return error!(EAGAIN);
            }
            match self.ref_count.compare_exchange_weak(
                val,
                val + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => val = actual,
            }
        }

        let _guard = scopeguard::guard(&self.ref_count, |ref_count| {
            ref_count.fetch_sub(1, Ordering::Release);
        });

        if buf.available() < (*PAGE_SIZE) as usize {
            starnix_logging::log_error!(
                "Buffer is too small: {} bytes (needs {} bytes)",
                buf.available(),
                (*PAGE_SIZE) as usize
            );
            return error!(EINVAL);
        }
        // TODO(https://fxbug.dev/505532201): Consider handling buffers larger than a page.
        // this currently only works for a single page.
        if let Some(idx) = self.swap_reader_page() {
            let node = &self.nodes[idx];
            let mut offset = 0;
            let page_size = (*PAGE_SIZE) as usize;
            let bytes_written = buf.write_each(&mut move |segment| {
                let available = page_size - offset;
                if available == 0 {
                    return Ok(0);
                }
                let size = std::cmp::min(segment.len(), available);
                // SAFETY: We are writing to this segment, so it's safe to treat it as initialized after we write.
                // We cast it to `&mut [u8]` to pass to `read_at`.
                let segment_mut_u8 = unsafe {
                    std::slice::from_raw_parts_mut(segment.as_mut_ptr() as *mut u8, size)
                };
                node.page_data.read_at(offset, segment_mut_u8);
                offset += size;
                Ok(size)
            })?;
            return Ok(bytes_written);
        }
        error!(EAGAIN)
    }

    pub fn size_bytes(&self) -> usize {
        self.nodes.len() * (*PAGE_SIZE) as usize
    }

    pub fn is_enabled(&self) -> bool {
        self.ref_count.load(Ordering::Acquire) & RING_ENABLED_BIT != 0
    }
    pub fn disable(&self) -> Result<u64, Errno> {
        let _lock = self.state_mutex.lock();
        // Clear the enabled bit.
        self.ref_count.fetch_and(!RING_ENABLED_BIT, Ordering::AcqRel);
        let mut yield_count: u64 = 0;
        loop {
            let active_writers = self.ref_count.load(Ordering::Acquire);
            if active_writers == 0 {
                break;
            }
            yield_count += 1;
            if yield_count % 1_000_000 == 0 {
                log_error!(
                    "LocklessRingBuffer: disable is waiting for {active_writers} active writers",
                );
            }
            // We use yield_now() here as a compromise:
            // - spin_wait would waste too much CPU if the writer is descheduled.
            // - sleep adds too much latency for what should be a very short wait (copying data).
            // Yielding gives other threads a chance to run while keeping latency low.
            std::thread::yield_now();
        }
        if yield_count > 500_000 {
            log_warn!("LocklessRingBuffer disable took {} yields", yield_count);
        }
        if let Err(e) = self.vmo.set_size(0) {
            starnix_logging::log_error!(
                "LocklessRingBuffer disable failed to set_size(0): {:?}",
                e
            );
            return Err(from_status_like_fdio!(e));
        }
        let dropped = self.dropped_pages.load(Ordering::Relaxed);
        Ok(dropped)
    }

    /// If the ring is disabled, resets it to empty, enables it, and returns the time of the reset.
    ///
    /// If the ring is already enabled, does nothing, so in-flight writers and readers, and the
    /// `tail_page` generation, are unaffected. In that case it returns the time of the last reset,
    /// by `new()` or by an earlier `enable()`.
    pub fn enable(&self) -> Result<zx::BootInstant, Errno> {
        let mut last_reset_guard = self.state_mutex.lock();
        // Only `enable()` and `disable()` change `RING_ENABLED_BIT` once the ring is shared, and
        // both hold `state_mutex`, so the ring stays enabled until we return. The mutex, not the
        // Acquire load in `is_enabled()`, is what makes this check reliable.
        if self.is_enabled() {
            return Ok(*last_reset_guard);
        }
        // The ring is disabled, so no reader or writer holds a reference, and none can take one
        // until the Release store of `ref_count` at the end of this function:
        // 1. `disable()` clears `RING_ENABLED_BIT` and drains `ref_count` to 0, holding
        //    `state_mutex`.
        // 2. `reserve()` and `read()` take a reference only with a CAS that fails while
        //    `RING_ENABLED_BIT` is clear, and decrement only a reference they took.
        // So the resets below, including the non-atomic page writes, race with nothing. A thread
        // that takes a reference afterwards synchronizes with that Release store and sees them.
        // Relaxed: `state_mutex` orders this load after the drain in `disable()`.
        debug_assert_eq!(self.ref_count.load(Ordering::Relaxed), 0);
        let initial_pages = self.nodes.len() - 1;
        let capacity = self.nodes.len() * (*PAGE_SIZE) as usize;
        if let Err(e) = self.vmo.set_size(capacity as u64) {
            starnix_logging::log_error!("LocklessRingBuffer enable failed to set_size: {:?}", e);
            return Err(from_status_like_fdio!(e));
        }
        let now = zx::BootInstant::get();
        // Reset state.
        self.head_page.store(0, Ordering::Release);
        // Move the tail to node 0 in the next generation rather than back to generation 0. Today
        // no `tail_page` sample survives into the new ring: every thread that loads `tail_page`
        // holds a `ref_count` reference, and `disable()` drained them. The bump keeps a stale
        // sample from matching the new tail if a later change loads `tail_page` without a
        // reference.
        // Relaxed load: `new()` and earlier `enable()` calls happen before this call, by
        // construction and through `state_mutex`. Every other `tail_page` write was made by a
        // thread that held a reference and released it with a Release decrement. The only plain
        // store to `ref_count` is in `enable()`, before any reference of its epoch is taken, so
        // every later write to it is a read-modify-write. The Acquire load in `disable()` that
        // saw 0 therefore reads from the release sequence of each decrement of that epoch, and
        // `state_mutex` orders `disable()` before this call, so the load returns the latest
        // value.
        // Relaxed store: the Release store of `ref_count` at the end of this function publishes
        // it.
        self.tail_page.store(self.tail_page.load(Ordering::Relaxed).next(0), Ordering::Relaxed);

        self.reader_page.store(initial_pages, Ordering::Release);

        self.dropped_pages.store(0, Ordering::Release);
        self.prev_timestamp.store(now.into_nanos() as u64, Ordering::Release);
        *last_reset_guard = now;

        for i in 0..self.nodes.len() {
            // Relaxed: see `Node::write_offset`.
            self.nodes[i]
                .write_offset
                .store(LocklessRingBuffer::PAGE_HEADER_SIZE, Ordering::Relaxed);
            self.nodes[i].active_writers.store(PAGE_ACTIVE_BIT, Ordering::Release);
        }
        for i in 0..initial_pages {
            let next_idx = (i + 1) % initial_pages;
            let prev_idx = (i + initial_pages - 1) % initial_pages;
            self.nodes[i].next.store(Node::make_val(next_idx, FLAG_NORMAL), Ordering::Relaxed);
            self.nodes[i].prev.store(Node::make_val(prev_idx, FLAG_NORMAL), Ordering::Relaxed);
        }
        self.nodes[initial_pages - 1].next.store(Node::make_val(0, FLAG_HEADER), Ordering::Relaxed);

        // Initialize reader page size to 0
        self.nodes[initial_pages].page_data.write_at(8, &0u64.to_le_bytes());
        // Initialize first page.
        let _ = self.vmo.op_range(zx::VmoOp::ZERO, 0, *PAGE_SIZE);
        let nanos = now.into_nanos() as u64;
        self.nodes[0].page_data.write_at(0, &nanos.to_le_bytes());

        // Set enabled bit LAST to ensure state is fully reset before writes start.
        self.ref_count.store(RING_ENABLED_BIT, Ordering::Release);
        Ok(now)
    }
}
impl Drop for LocklessRingBuffer {
    fn drop(&mut self) {
        let (ptr, len) = self.mapping.as_ptr_len();
        // SAFETY: The mapping was created in `new` and is valid for this lifetime.
        // We are freeing it now because the object is being destroyed.
        unsafe {
            let _ = vmar_root_self().unmap(ptr as usize, len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;

    /// An `OutputBuffer` that collects what is written into a `Vec`, in place of starnix_core's
    /// `VecOutputBuffer`.
    struct VecOutputBuffer {
        data: Vec<u8>,
        capacity: usize,
    }
    impl VecOutputBuffer {
        fn new(capacity: usize) -> Self {
            Self { data: Vec::with_capacity(capacity), capacity }
        }
        fn data(&self) -> &[u8] {
            &self.data
        }
        fn reset(&mut self) {
            self.data.clear();
        }
    }
    impl OutputBuffer for VecOutputBuffer {
        fn available(&self) -> usize {
            self.capacity - self.data.len()
        }
        fn write_each(&mut self, callback: &mut OutputBufferCallback<'_>) -> Result<usize, Errno> {
            let mut segment = vec![MaybeUninit::new(0u8); self.available()];
            let written = callback(&mut segment)?;
            // SAFETY: `segment` was fully initialized above.
            self.data.extend(segment[..written].iter().map(|b| unsafe { b.assume_init() }));
            Ok(written)
        }
    }

    /// Calls `reserve()` with the time fixed at `now`.
    fn reserve_at(
        buffer: &LocklessRingBuffer,
        size: usize,
        now: zx::BootInstant,
    ) -> Result<(Reservation<'_>, zx::BootInstant, zx::Duration<zx::BootTimeline>), Errno> {
        buffer.reserve_with_clock(size, || now)
    }

    /// Calls `reserve()` with the time 1 ns after the ring's last timestamp. Tests that check exact
    /// offsets or page placement use it: with the real clock, a scheduling stall longer than
    /// `MAX_TIME_DELTA_NANOS` between two reservations would add a `TIME_STAMP` record.
    fn reserve_without_gap(
        buffer: &LocklessRingBuffer,
        size: usize,
    ) -> Result<(Reservation<'_>, zx::BootInstant, zx::Duration<zx::BootTimeline>), Errno> {
        // Relaxed: the value only picks the time. If it is stale, `try_reserve_on_page()` raises
        // the time to the latest value with its `fetch_max`, which still leaves no gap.
        let now = buffer.prev_timestamp.load(Ordering::Relaxed) + 1;
        reserve_at(buffer, size, zx::BootInstant::from_nanos(now as i64))
    }

    /// Writes a variable-length ftrace data record (`type_or_length == 0`) into `res`.
    fn write_ftrace_record(res: &Reservation<'_>, payload: &[u8]) {
        assert_eq!(res.size, 8 + payload.len());
        res.write_at(
            0,
            &(res.header_time_delta() << LocklessRingBuffer::TYPE_OR_LENGTH_BITS).to_le_bytes(),
        );
        res.write_at(4, &((4 + payload.len()) as u32).to_le_bytes());
        res.write_at(8, payload);
    }

    /// A data record decoded from an ftrace page.
    #[derive(Debug, Clone, PartialEq)]
    struct DecodedRecord {
        timestamp: u64,
        payload: Vec<u8>,
    }

    /// Returns a time after the ring's initial timestamp, so that no injected time is raised to it.
    fn injected_start(buffer: &LocklessRingBuffer) -> u64 {
        // Relaxed: the tests that call this are single-threaded.
        buffer.prev_timestamp.load(Ordering::Relaxed) + 1_000
    }

    /// Writes an ftrace data record holding `payload` at `now_nanos`. Returns the node it landed
    /// on and the delta that `reserve()` returned.
    fn write_record_at(
        buffer: &LocklessRingBuffer,
        now_nanos: u64,
        payload: &[u8],
    ) -> (usize, u64) {
        let (res, now, delta) =
            reserve_at(buffer, 8 + payload.len(), zx::BootInstant::from_nanos(now_nanos as i64))
                .unwrap();
        assert_eq!(now.into_nanos() as u64, now_nanos);
        let delta = delta.into_nanos() as u64;
        // With one writer, the header holds the delta unless it doesn't fit.
        let expected_header =
            if delta > LocklessRingBuffer::MAX_TIME_DELTA_NANOS { 0 } else { delta };
        assert_eq!(res.header_time_delta() as u64, expected_header);
        write_ftrace_record(&res, payload);
        let node_idx = res.node_idx;
        buffer.commit(res);
        (node_idx, delta)
    }

    /// Reads and decodes every page written so far. Returns each page's records and the indices
    /// of the records that follow a `TIME_STAMP` record.
    fn read_ftrace_pages(buffer: &LocklessRingBuffer) -> Vec<(Vec<DecodedRecord>, Vec<usize>)> {
        let mut pages = vec![];
        let read_all = |pages: &mut Vec<_>| loop {
            let mut dest = VecOutputBuffer::new((*PAGE_SIZE) as usize);
            match buffer.read(&mut dest) {
                Ok(_) => pages.push(decode_ftrace_page(dest.data())),
                Err(e) if e == errno!(EAGAIN) => break,
                Err(e) => panic!("read failed: {e:?}"),
            }
        };
        read_all(&mut pages);
        // A record that fills a page moves the tail off the last page, so it can be read too.
        let max_payload = (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let (res, _, _) = reserve_without_gap(buffer, max_payload).unwrap();
        buffer.commit(res);
        read_all(&mut pages);
        pages
    }

    /// Returns the record timestamps on `page` and how many follow a `TIME_STAMP` record.
    fn timestamps(page: &(Vec<DecodedRecord>, Vec<usize>)) -> (Vec<u64>, usize) {
        (page.0.iter().map(|r| r.timestamp).collect(), page.1.len())
    }

    /// Decodes an ftrace page the way Perfetto's `CpuReader::ParsePagePayload`
    /// (third_party/perfetto/src/traced/probes/ftrace/cpu_reader.cc) does. Returns the data records
    /// with their absolute timestamps, and the indices of the records that follow a `TIME_STAMP`
    /// record.
    fn decode_ftrace_page(page: &[u8]) -> (Vec<DecodedRecord>, Vec<usize>) {
        const TYPE_OR_LENGTH_MASK: u32 = (1 << LocklessRingBuffer::TYPE_OR_LENGTH_BITS) - 1;
        const TYPE_PADDING: u32 = 29;
        const TYPE_TIME_EXTEND: u32 = 30;
        let read_u32 =
            |offset: usize| u32::from_le_bytes(page[offset..offset + 4].try_into().unwrap());
        let mut timestamp = u64::from_le_bytes(page[0..8].try_into().unwrap());
        let end = LocklessRingBuffer::PAGE_HEADER_SIZE + read_data_size(page);
        assert!(end <= page.len(), "data size overflows the page");
        let mut ptr = LocklessRingBuffer::PAGE_HEADER_SIZE;
        let mut records = vec![];
        let mut stamped = vec![];
        let mut after_time_stamp = false;
        while ptr < end {
            assert!(ptr + 4 <= end, "short record header at {ptr}");
            let header = read_u32(ptr);
            ptr += 4;
            let type_or_length = header & TYPE_OR_LENGTH_MASK;
            let time_delta = (header >> LocklessRingBuffer::TYPE_OR_LENGTH_BITS) as u64;
            match type_or_length {
                TYPE_PADDING => {
                    assert!(ptr + 4 <= end, "short PADDING at {ptr}");
                    let length = read_u32(ptr) as usize;
                    assert!(length >= 4 && ptr + length <= end, "bad padding length {length}");
                    timestamp += time_delta;
                    ptr += length;
                }
                TYPE_TIME_EXTEND => {
                    assert!(ptr + 4 <= end, "short TIME_EXTEND at {ptr}");
                    timestamp += time_delta
                        + ((read_u32(ptr) as u64) << LocklessRingBuffer::TIME_DELTA_BITS);
                    ptr += 4;
                }
                LocklessRingBuffer::RINGBUF_TYPE_TIME_STAMP => {
                    assert!(ptr + 4 <= end, "short TIME_STAMP at {ptr}");
                    timestamp = time_delta
                        + ((read_u32(ptr) as u64) << LocklessRingBuffer::TIME_DELTA_BITS);
                    ptr += 4;
                    after_time_stamp = true;
                }
                _ => {
                    timestamp += time_delta;
                    let event_size = if type_or_length == 0 {
                        assert!(ptr + 4 <= end, "short data length at {ptr}");
                        let length = read_u32(ptr) as usize;
                        ptr += 4;
                        assert!(length >= 4, "bad data length {length} at {ptr}");
                        length - 4
                    } else {
                        4 * type_or_length as usize
                    };
                    assert!(ptr + event_size <= end, "record at {ptr} overflows the data");
                    if after_time_stamp {
                        stamped.push(records.len());
                        after_time_stamp = false;
                    }
                    records.push(DecodedRecord {
                        timestamp,
                        payload: page[ptr..ptr + event_size].to_vec(),
                    });
                    ptr += event_size;
                }
            }
        }
        assert_eq!(ptr, end, "records do not end exactly at the data size");
        (records, stamped)
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestMessage {
        thread_index: u32,
        timestamp_nanos: u64,
        delta: u64,
        data: [u8; 12],
    }
    /// Returns the `data_size` field of the page header at the start of `page`.
    fn read_data_size(page: &[u8]) -> usize {
        const DATA_SIZE_OFFSET: usize = 8;
        assert!(page.len() >= LocklessRingBuffer::PAGE_HEADER_SIZE, "page shorter than header");
        let mut size_bytes = [0u8; 8];
        size_bytes.copy_from_slice(&page[DATA_SIZE_OFFSET..LocklessRingBuffer::PAGE_HEADER_SIZE]);
        u64::from_le_bytes(size_bytes) as usize
    }

    impl TestMessage {
        pub const SIZE: usize = 32;
        fn to_bytes(&self) -> [u8; TestMessage::SIZE] {
            let mut bytes = [0u8; TestMessage::SIZE];
            bytes[0..4].copy_from_slice(&self.thread_index.to_le_bytes());
            bytes[4..12].copy_from_slice(&self.timestamp_nanos.to_le_bytes());
            bytes[12..20].copy_from_slice(&self.delta.to_le_bytes());
            bytes[20..32].copy_from_slice(&self.data);
            bytes
        }
        fn from_bytes(bytes: &[u8]) -> Self {
            let mut thread_index = [0u8; 4];
            thread_index.copy_from_slice(&bytes[0..4]);
            let mut timestamp_nanos = [0u8; 8];
            timestamp_nanos.copy_from_slice(&bytes[4..12]);
            let mut delta = [0u8; 8];
            delta.copy_from_slice(&bytes[12..20]);
            let mut data = [0u8; 12];
            data.copy_from_slice(&bytes[20..32]);
            Self {
                thread_index: u32::from_le_bytes(thread_index),
                timestamp_nanos: u64::from_le_bytes(timestamp_nanos),
                delta: u64::from_le_bytes(delta),
                data,
            }
        }
    }
    #[test]
    fn test_init() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        assert_eq!(buffer.size_bytes(), 3 * (*PAGE_SIZE) as usize);
    }
    #[test]
    fn test_reserve() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let res = buffer.reserve(100).unwrap();
        assert_eq!(res.0.size, 100);
        assert_eq!(res.0.offset, LocklessRingBuffer::PAGE_HEADER_SIZE);
        assert_eq!(res.0.node_idx, 0);
    }
    #[test]
    fn test_commit() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let res = buffer.reserve(100).unwrap();
        buffer.commit(res.0);
        assert_eq!(buffer.nodes[0].active_writers.load(Ordering::Relaxed), PAGE_ACTIVE_BIT);
    }
    #[test]
    fn test_swap_reader_page_empty() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        assert_eq!(buffer.swap_reader_page(), None);
    }
    #[test]
    fn test_swap_reader_page_success() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        // Fill node 0
        let res1 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        buffer.commit(res1.0);
        // Next reserve should go to node 1
        let res2 = buffer.reserve(100).unwrap();
        assert_eq!(res2.0.node_idx, 1);
        buffer.commit(res2.0);
        // Page 0 has size > 0 and tail advanced to Page 1.
        // So swap_reader_page should succeed!
        let old_head = buffer.swap_reader_page();
        assert_eq!(old_head, Some(0));
        // And head_page should now have advanced to index 1.
        assert_eq!(buffer.head_page.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn test_concurrent_reserve() {
        use std::sync::Arc;
        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let mut handles = vec![];
        for _ in 0..5 {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                for _ in 0..20 {
                    if let Ok(res) = buffer_clone.reserve(10) {
                        buffer_clone.commit(res.0);
                    }
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            handle.join().unwrap();
        }
        for i in 0..buffer.nodes.len() {
            assert_eq!(buffer.nodes[i].active_writers.load(Ordering::Relaxed), PAGE_ACTIVE_BIT);
        }
    }
    #[test]
    fn test_reserve_moves_to_next_page() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        // Reserve most of the page, leaving only 50 bytes
        let res1 = buffer
            .reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE - 50)
            .unwrap();
        buffer.commit(res1.0);
        // Try to reserve 100 bytes. It shouldn't fit on node 0.
        let res2 = buffer.reserve(100).unwrap();
        // It should be on node 1.
        assert_eq!(res2.0.node_idx, 1);
        assert_eq!(res2.0.offset, (*PAGE_SIZE) as usize + LocklessRingBuffer::PAGE_HEADER_SIZE);
    }
    #[test]
    fn test_reserve_overwrite() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        // Fill node 0 and commit
        let res1 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        buffer.commit(res1.0);
        // Fill node 1 but do NOT commit
        let _res2 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        // Now tail is at node 1. Next reserve wants to move to node 0.
        // But commit is still at node 0.
        // So it should trigger overwrite.
        let res3 = buffer.reserve(100).unwrap();
        // It should succeed and be on node 0.
        assert_eq!(res3.0.node_idx, 0);
        assert_eq!(buffer.dropped_pages(), 1);
        // And head_page should have advanced to 1.
        assert_eq!(buffer.head_page.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn test_read() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let res1 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        let data = vec![1u8; (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE];
        res1.0.write_at(0, &data);
        buffer.commit(res1.0);
        // Advance commit page to node 1
        let res2 = buffer.reserve(100).unwrap();
        buffer.commit(res2.0);
        let mut dest = VecOutputBuffer::new((*PAGE_SIZE) as usize);
        let result = buffer.read(&mut dest);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), (*PAGE_SIZE) as usize);
        assert_eq!(&dest.data()[LocklessRingBuffer::PAGE_HEADER_SIZE..], &data[..]);
    }
    #[test]
    fn test_enable_disable() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        assert!(buffer.disable().is_ok());
        assert_eq!(buffer.ref_count.load(Ordering::Relaxed) & RING_ENABLED_BIT, 0);
        let res = buffer.reserve(100);
        assert_eq!(res.unwrap_err(), starnix_uapi::errno!(ENOMEM));
        assert!(buffer.enable().is_ok());
        assert_eq!(buffer.size_bytes(), 3 * (*PAGE_SIZE) as usize);
        let res = buffer.reserve(100);
        assert!(res.is_ok());
    }

    // Mirrors how starnix_core's TraceEventQueue uses the ring: create it in overwrite mode with a
    // page-multiple size larger than the minimum, disable it, then enable it.
    #[test]
    fn test_enable_after_disable_restores_requested_size() {
        let size_bytes = 512 * (*PAGE_SIZE) as usize;
        let buffer = LocklessRingBuffer::new(size_bytes, true, fuchsia_trace::Id::new()).unwrap();
        assert_eq!(buffer.size_bytes(), size_bytes);

        buffer.disable().unwrap();
        assert_eq!(buffer.vmo.get_size().unwrap(), 0);

        buffer.enable().unwrap();
        assert_eq!(buffer.size_bytes(), size_bytes);
        assert_eq!(buffer.vmo.get_size().unwrap(), size_bytes as u64);
    }
    // Spawns a reader thread that periodically drains the lockless ring buffer.
    fn start_reader_thread(
        buffer_reader: Arc<LocklessRingBuffer>,
        writers_done_reader: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<Vec<TestMessage>> {
        start_gated_reader_thread(buffer_reader, writers_done_reader, false)
    }

    // Spawns a reader thread that optionally yields until at least one page has been
    // dropped (overwritten) by writers before draining.
    fn start_gated_reader_thread(
        buffer_reader: Arc<LocklessRingBuffer>,
        writers_done_reader: Arc<std::sync::atomic::AtomicBool>,
        wait_for_dropped_pages: bool,
    ) -> std::thread::JoinHandle<Vec<TestMessage>> {
        std::thread::spawn(move || {
            if wait_for_dropped_pages {
                while buffer_reader.dropped_pages() == 0
                    && !writers_done_reader.load(Ordering::Acquire)
                {
                    std::thread::yield_now();
                }
            }
            let mut all_messages = Vec::new();
            let mut dest = VecOutputBuffer::new((*PAGE_SIZE) as usize);
            let mut consecutive_eagain = 0;
            loop {
                dest.reset();
                match buffer_reader.read(&mut dest) {
                    Ok(bytes_read) => {
                        consecutive_eagain = 0;
                        assert_eq!(bytes_read, (*PAGE_SIZE) as usize);
                        let mut header_ts_bytes = [0u8; 8];
                        header_ts_bytes.copy_from_slice(&dest.data()[0..8]);
                        let header_timestamp = u64::from_le_bytes(header_ts_bytes);
                        let data_size = read_data_size(dest.data());
                        let max_offset = LocklessRingBuffer::PAGE_HEADER_SIZE + data_size;
                        let mut offset = LocklessRingBuffer::PAGE_HEADER_SIZE;
                        let mut first_msg = true;
                        while offset + TestMessage::SIZE <= max_offset {
                            // Skip the `TIME_STAMP` record that `reserve()` puts before a message
                            // more than `MAX_TIME_DELTA_NANOS` after the previous one. A message
                            // starts with `thread_index`, which is never `RINGBUF_TYPE_TIME_STAMP`.
                            let header = u32::from_le_bytes(
                                dest.data()[offset..offset + 4].try_into().unwrap(),
                            );
                            if header & ((1 << LocklessRingBuffer::TYPE_OR_LENGTH_BITS) - 1)
                                == LocklessRingBuffer::RINGBUF_TYPE_TIME_STAMP
                            {
                                offset += LocklessRingBuffer::TIME_STAMP_RECORD_SIZE;
                                continue;
                            }
                            let msg_bytes = &dest.data()[offset..offset + TestMessage::SIZE];
                            let msg = TestMessage::from_bytes(msg_bytes);
                            if first_msg {
                                if header_timestamp != msg.timestamp_nanos {
                                    println!(
                                        "HEADER TIMESTAMP MISMATCH: header={}, msg={}",
                                        header_timestamp, msg.timestamp_nanos
                                    );
                                }
                                first_msg = false;
                            }
                            all_messages.push(msg);
                            offset += TestMessage::SIZE;
                        }
                    }
                    Err(e) if e == starnix_uapi::errno!(EAGAIN) => {
                        if writers_done_reader.load(Ordering::Acquire) {
                            let head_val = buffer_reader.head_page.load(Ordering::Relaxed);
                            let tail_val = buffer_reader.tail_page.load(Ordering::Relaxed);
                            // If the writers are done and the reader has caught up to the tail page
                            // index, all readable pages have been drained, so we break the loop.
                            if Node::get_index(head_val) == tail_val.index() as usize {
                                break;
                            }
                            consecutive_eagain += 1;
                            assert!(
                                consecutive_eagain < 500,
                                "LocklessRingBuffer reader stuck: consecutive EAGAIN limit exceeded (5s) after writers finished. head_page={}, tail_page={}, reader_page={}",
                                Node::get_index(head_val),
                                tail_val.index(),
                                buffer_reader.reader_page.load(Ordering::Relaxed)
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("Unexpected error from read: {:?}", e),
                }
            }
            all_messages
        })
    }
    fn check_all_message_data(all_messages: &[TestMessage], num_threads: u32) {
        let mut prev_timestamp = 0;
        let mut thread_counts = vec![0; num_threads as usize];
        let mut out_of_order = 0;
        let mut corrupted = 0;
        for msg in all_messages {
            if msg.timestamp_nanos < prev_timestamp {
                println!("OUT OF ORDER: prev_timestamp={}, current={:?}", prev_timestamp, msg);
                out_of_order += 1;
            }
            prev_timestamp = msg.timestamp_nanos;
            if &msg.data != b"Event data\0\0" || msg.thread_index >= num_threads {
                println!("CORRUPTED: msg={:?}", msg);
                corrupted += 1;
            } else {
                thread_counts[msg.thread_index as usize] += 1;
            }
        }
        println!(
            "TEST_RESULT: Read {} messages, {} out of order, {} corrupted. Thread counts: {:?}",
            all_messages.len(),
            out_of_order,
            corrupted,
            thread_counts
        );
        // We do not assert out_of_order is 0 because wait-free reservation can cause slight reordering of timestamps.
        assert_eq!(corrupted, 0, "Found corrupted messages");
    }
    #[test]
    fn test_concurrent_read_write_4() {
        let num_threads = 4;
        let msgs_per_thread = 64;
        // 4 threads * 64 msgs * 32 bytes = 8192 bytes.
        // Each page can hold (4096 - 16) / 32 = 127 msgs (with 16 bytes unused at the end).
        // 256 msgs will take 2 full pages (254 msgs) + 2 msgs on a 3rd page.
        let buffer = Arc::new(
            LocklessRingBuffer::new(4 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = vec![];
        // Spawn reader
        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        let reader_handle = start_reader_thread(buffer_reader, writers_done_reader);
        // Spawn writers
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10 + thread_index as u64));
                for _ in 0..msgs_per_thread {
                    // Reserve exactly TestMessage::SIZE bytes
                    let (res, now, delta) = buffer_clone.reserve(TestMessage::SIZE).unwrap();
                    let msg = TestMessage {
                        thread_index,
                        timestamp_nanos: now.into_nanos() as u64,
                        delta: delta.into_nanos() as u64,
                        data: *b"Event data\0\0",
                    };
                    res.write_at(0, &msg.to_bytes());
                    buffer_clone.commit(res);
                    std::thread::sleep(std::time::Duration::from_nanos(thread_index as u64));
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            handle.join().unwrap();
        }
        writers_done.store(true, Ordering::Release);
        let all_messages = reader_handle.join().unwrap();
        // Check the messages
        check_all_message_data(&all_messages, num_threads);
        // Trailing messages might remain on the final unfinalized tail page. We expect between 250 and 256 messages to be successfully read.
        assert!(
            all_messages.len() >= 250 && all_messages.len() <= 256,
            "Expected between 250 and 256 messages, got {}",
            all_messages.len()
        );
    }
    #[test]
    fn test_concurrent_read_write_1_thread() {
        let num_threads = 1;
        let msgs_per_thread = 256;
        // 1 thread * 256 msgs = 256 msgs.
        // Each page holds 127 msgs. 256 msgs requires 3 data pages.
        // We use 5 pages total to provide enough capacity.
        let buffer = Arc::new(
            LocklessRingBuffer::new(5 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = vec![];
        // Spawn reader
        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        let reader_handle = start_reader_thread(buffer_reader, writers_done_reader);
        // Spawn writers
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10 + thread_index as u64));
                for _ in 0..msgs_per_thread {
                    let (res, now, delta) =
                        reserve_without_gap(&buffer_clone, TestMessage::SIZE).unwrap();
                    let msg = TestMessage {
                        thread_index,
                        timestamp_nanos: now.into_nanos() as u64,
                        delta: delta.into_nanos() as u64,
                        data: *b"Event data\0\0",
                    };
                    res.write_at(0, &msg.to_bytes());
                    buffer_clone.commit(res);
                    std::thread::sleep(std::time::Duration::from_nanos(thread_index as u64));
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            handle.join().unwrap();
        }
        writers_done.store(true, Ordering::Release);
        let all_messages = reader_handle.join().unwrap();
        check_all_message_data(&all_messages, num_threads);
        assert_eq!(
            all_messages.len(),
            254,
            "Expected exactly 254 messages, got {}",
            all_messages.len()
        );
    }
    #[test]
    fn test_concurrent_read_write_8_threads() {
        let num_threads = 8;
        let msgs_per_thread = 128;
        // 4 threads * 64 msgs * 32 bytes = 8192 bytes.
        // 8 threads * 128 msgs = 1024 msgs.
        // Each page holds 127 msgs. 1024 msgs requires 9 data pages.
        // We use 12 pages total to provide enough capacity so no pages are overwritten.
        let buffer = Arc::new(
            LocklessRingBuffer::new(12 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _writers_done_guard = scopeguard::guard(Arc::clone(&writers_done), |done| {
            done.store(true, Ordering::Release);
        });
        let mut handles = vec![];
        // Spawn reader
        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        let reader_handle = start_reader_thread(buffer_reader, writers_done_reader);
        // Spawn writers
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10 + thread_index as u64));
                for _ in 0..msgs_per_thread {
                    // Reserve exactly TestMessage::SIZE bytes
                    let mut retries = 0;
                    let (res, now, delta) = loop {
                        match buffer_clone.reserve(TestMessage::SIZE) {
                            Ok(r) => break r,
                            Err(e) if e == starnix_uapi::errno!(ENOSPC) => {
                                retries += 1;
                                assert!(
                                    retries < 100,
                                    "LocklessRingBuffer: ENOSPC transient limit exceeded (100ms). Reader may have hung."
                                );
                                // Under high contention, writers can transiently catch up to the head page.
                                // This is expected and will clear up once the reader thread gets scheduled
                                // and swaps a page.
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(e) => panic!("Unexpected error: {:?}", e),
                        }
                    };
                    let msg = TestMessage {
                        thread_index,
                        timestamp_nanos: now.into_nanos() as u64,
                        delta: delta.into_nanos() as u64,
                        data: *b"Event data\0\0",
                    };
                    res.write_at(0, &msg.to_bytes());
                    buffer_clone.commit(res);
                    std::thread::sleep(std::time::Duration::from_nanos(thread_index as u64));
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            handle.join().unwrap();
        }

        writers_done.store(true, Ordering::Release);
        let all_messages = reader_handle.join().unwrap();
        // Check the messages
        check_all_message_data(&all_messages, num_threads);
        // The last page in the circle (current tail page) cannot be swapped by the reader
        // (to prevent reading uncommitted/partial data). The number of messages left sitting on
        // the tail page when the writers finish depends entirely on concurrent thread scheduling
        // variance under high contention. We expect between 1008 and 1024 messages to be read
        // (leaving 0 to 16 messages unread on the final tail page).
        assert!(
            all_messages.len() >= 1008 && all_messages.len() <= 1024,
            "Expected between 1008 and 1024 messages, got {}",
            all_messages.len()
        );
    }
    #[test]
    fn test_disable_waits_for_ref_count() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        // Thread 1: Reserves space, keeping ref_count = 1 (plus enabled bit).
        let res = buffer.reserve(100).unwrap();
        let disable_finished = Arc::new(AtomicBool::new(false));
        let disable_finished_clone = Arc::clone(&disable_finished);
        let buffer_clone = Arc::clone(&buffer);
        // Thread 2: Calls disable. This should block because ref_count has active writers.
        let handle = std::thread::spawn(move || {
            buffer_clone.disable().unwrap();
            disable_finished_clone.store(true, Ordering::Relaxed);
        });
        // Wait for Thread 2 to start and set size_bytes to 0
        while buffer.ref_count.load(Ordering::Acquire) & RING_ENABLED_BIT != 0 {
            std::hint::spin_loop();
        }
        // Verify that disable has NOT finished
        assert!(!disable_finished.load(Ordering::Relaxed));
        // Verify that new reserves fail with ENOMEM because disable() cleared RING_ENABLED_BIT
        let res2 = buffer.reserve(100);
        assert_eq!(res2.unwrap_err(), starnix_uapi::errno!(ENOMEM));
        // Now write data and commit the first reservation.
        // If the VMO had been shrunk, write_data would panic/fault here.
        let data = vec![42u8; 100];
        res.0.write_at(0, &data);
        buffer.commit(res.0);
        // Wait for Thread 2 to finish
        handle.join().unwrap();
        // Verify disable finished
        assert!(disable_finished.load(Ordering::Relaxed));
    }
    #[test]
    fn test_reservation_drop_cancels_writer_count() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        // 1. Reserve space. ref_count becomes 1 (plus enabled bit).
        let res = buffer.reserve(100).unwrap();
        assert_eq!(buffer.ref_count.load(Ordering::Relaxed), RING_ENABLED_BIT | 1);
        // 2. Drop the reservation without commit.
        std::mem::drop(res.0);
        // 3. Verify ref_count becomes 0 (plus enabled bit).
        assert_eq!(buffer.ref_count.load(Ordering::Relaxed), RING_ENABLED_BIT);
        // 4. Verify that a new reservation can be committed (proving the dropped one didn't block).
        let res2 = buffer.reserve(100).unwrap();
        buffer.commit(res2.0);
    }
    #[test]
    fn test_overwrite_and_read_4_pages() {
        let num_threads = 1;
        // Use 6 pages total (5 data pages) to allow exactly 4 readable pages
        // (1 page is the active commit page).
        let buffer = Arc::new(
            LocklessRingBuffer::new(6 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 5 data pages capacity + 2 overwritten pages = 7 pages to write.
        // Each page holds exactly 127 messages.
        let msgs_per_thread = 7 * 127;
        let buffer_clone = Arc::clone(&buffer);
        let thread_index = 0;
        let writer_handle = std::thread::spawn(move || {
            for _ in 0..msgs_per_thread {
                let (res, now, delta) =
                    reserve_without_gap(&buffer_clone, TestMessage::SIZE).unwrap();
                let msg = TestMessage {
                    thread_index,
                    timestamp_nanos: now.into_nanos() as u64,
                    delta: delta.into_nanos() as u64,
                    data: *b"Event data\0\0",
                };
                res.write_at(0, &msg.to_bytes());
                buffer_clone.commit(res);
            }
        });
        writer_handle.join().unwrap();
        assert_eq!(buffer.dropped_pages(), 2, "Expected 2 pages to be dropped");
        writers_done.store(true, Ordering::Release);
        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        let reader_handle = start_reader_thread(buffer_reader, writers_done_reader);
        let all_messages = reader_handle.join().unwrap();
        check_all_message_data(&all_messages, num_threads);
        assert_eq!(all_messages.len(), 4 * 127, "Expected exactly 4 pages of messages");
    }
    #[test]
    fn test_reserve_full_producer_consumer() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        // Fill node 0
        let res1 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        buffer.commit(res1.0);
        // Fill node 1
        let res2 =
            buffer.reserve((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE).unwrap();
        buffer.commit(res2.0);
        // Buffer is full (2 data pages used, head at 0, commit at 1 but next is head).
        // Next reserve should fail with ENOSPC because overwrite is false.
        let res3 = buffer.reserve(100);
        assert_eq!(res3.unwrap_err(), starnix_uapi::errno!(ENOSPC));
    }

    #[test]
    fn test_reserve_zero_size() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let res = buffer.reserve(0);
        assert_eq!(res.unwrap_err(), starnix_uapi::errno!(EINVAL));
    }
    #[test]
    fn test_concurrent_overwrite_stability() {
        let num_threads = 4;
        let msgs_per_thread = 256;
        // Small buffer to force overwrites
        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        // Synchronously fill the active circular buffer (2 data pages) to capacity.
        // The reader thread is gated below so it does not drain the buffer until
        // the writer threads wrap around to Node 0 and trigger a dropped page.
        let max_payload = (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let num_msgs = max_payload / TestMessage::SIZE;
        for _ in 0..(2 * num_msgs) {
            let (res, now, delta) = reserve_without_gap(&buffer, TestMessage::SIZE).unwrap();
            let msg = TestMessage {
                thread_index: 0,
                timestamp_nanos: now.into_nanos() as u64,
                delta: delta.into_nanos() as u64,
                data: *b"Event data\0\0",
            };
            res.write_at(0, &msg.to_bytes());
            buffer.commit(res);
        }

        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = vec![];

        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        // Gate reader until writers wrap around and trigger an overwrite (dropped page),
        // guaranteeing overwrite stability is exercised without relying on timing or sleeps.
        let reader_handle = start_gated_reader_thread(buffer_reader, writers_done_reader, true);

        // Spawn writers
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                for _ in 0..msgs_per_thread {
                    if let Ok((res, now, delta)) = buffer_clone.reserve(TestMessage::SIZE) {
                        let msg = TestMessage {
                            thread_index,
                            timestamp_nanos: now.into_nanos() as u64,
                            delta: delta.into_nanos() as u64,
                            data: *b"Event data\0\0",
                        };
                        res.write_at(0, &msg.to_bytes());
                        buffer_clone.commit(res);
                    }
                    // Yield to allow other threads to run and cause contention
                    std::thread::yield_now();
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            handle.join().unwrap();
        }
        writers_done.store(true, Ordering::Release);
        let all_messages = reader_handle.join().unwrap();
        // Check that data is not corrupted
        check_all_message_data(&all_messages, num_threads);
        // Verify that some pages were dropped (overwritten)
        assert!(buffer.dropped_pages() > 0, "Expected at least some dropped pages");
    }

    #[test]
    fn test_out_of_order_completion_same_page() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();

        let header_size = LocklessRingBuffer::PAGE_HEADER_SIZE;
        let page_size = (*PAGE_SIZE) as usize;
        let available_space = page_size - header_size;

        let size1 = available_space / 2;
        let size2 = available_space - size1;

        // Reserve first chunk
        let (res1, _, _) = reserve_without_gap(&buffer, size1).unwrap();
        // Reserve second chunk (fills the page)
        let (res2, _, _) = reserve_without_gap(&buffer, size2).unwrap();

        // Commit second chunk first (out of order)
        buffer.commit(res2);

        // Page should not be finalized yet because res1 is still active
        assert!(buffer.swap_reader_page().is_none());

        // Commit first chunk
        buffer.commit(res1);

        // Force advance tail by making a reservation that won't fit on Page 0.
        let _ = reserve_without_gap(&buffer, 10).unwrap();

        // Now page should be finalized
        let swapped = buffer.swap_reader_page();
        assert!(swapped.is_some());
        assert_eq!(swapped.unwrap(), 0); // First data page is node 0
    }

    #[test]
    fn test_concurrent_readers_rejection() {
        #[derive(Debug)]
        struct SleepingOutputBuffer {
            barrier_in_available: Arc<std::sync::Barrier>,
            barrier_test_done: Arc<std::sync::Barrier>,
        }
        impl OutputBuffer for SleepingOutputBuffer {
            fn write_each(
                &mut self,
                _callback: &mut OutputBufferCallback<'_>,
            ) -> Result<usize, Errno> {
                Ok(0)
            }
            fn available(&self) -> usize {
                // Signal the main thread that we are active and holding the read lock, and block
                // until the main thread reaches its wait point.
                self.barrier_in_available.wait();

                // Block here until the main thread has completed its concurrent read assertion.
                self.barrier_test_done.wait();

                (*PAGE_SIZE) as usize
            }
        }

        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap(),
        );

        let barrier_in_available = Arc::new(std::sync::Barrier::new(2));
        let barrier_test_done = Arc::new(std::sync::Barrier::new(2));

        let buffer_clone = Arc::clone(&buffer);
        let barrier_in_available_clone = Arc::clone(&barrier_in_available);
        let barrier_test_done_clone = Arc::clone(&barrier_test_done);
        let handle = std::thread::spawn(move || {
            let mut dest = SleepingOutputBuffer {
                barrier_in_available: barrier_in_available_clone,
                barrier_test_done: barrier_test_done_clone,
            };
            let _ = buffer_clone.read(&mut dest);
        });

        // Wait until the background thread enters `available()` and is holding the read lock.
        barrier_in_available.wait();

        // Perform concurrent read while background thread holds `reader_active` lock.
        let mut dest = VecOutputBuffer::new((*PAGE_SIZE) as usize);
        let res = buffer.read(&mut dest);
        assert_eq!(res.unwrap_err(), starnix_uapi::errno!(EBUSY));

        // Signal the background thread that the assertion is done and it can proceed.
        barrier_test_done.wait();

        handle.join().unwrap();
    }

    #[test]
    fn test_stale_offset_livelock() {
        let num_threads = 8;
        let msgs_per_thread = 200;
        // Small 3-page buffer to force wraparounds and high contention
        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let mut handles = vec![];

        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                for _ in 0..msgs_per_thread {
                    if let Ok((res, now, delta)) = buffer_clone.reserve(TestMessage::SIZE) {
                        let msg = TestMessage {
                            thread_index,
                            timestamp_nanos: now.into_nanos() as u64,
                            delta: delta.into_nanos() as u64,
                            data: *b"Event data\0\0",
                        };
                        res.write_at(0, &msg.to_bytes());
                        buffer_clone.commit(res);
                    }
                    std::thread::yield_now();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let dropped = buffer.dropped_pages();
        println!("High contention completed. Total dropped pages = {}", dropped);
        // Dropped pages should be reasonable and not spike to infinity or cause deadlock
        assert!(dropped > 0);
    }

    #[test]
    fn test_writer_preemption_and_overwrite_prevention() {
        let buffer = Arc::new(
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );

        // 1. Thread 1 reserves space on Node 0, but delays committing (preempted)
        let res1 = reserve_without_gap(&buffer, 100).unwrap();
        assert_eq!(res1.0.node_idx, 0);

        // 2. Thread 2 fills the rest of Node 0, Node 1, and wraps around to Node 0.
        // Thread 2 should block or spin since Node 0 has active writers (active_writers > 0)
        let buffer_clone = Arc::clone(&buffer);
        let writer_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_finished_clone = Arc::clone(&writer_finished);

        let handle = std::thread::spawn(move || {
            // Fill Node 0
            let res2 = reserve_without_gap(
                &buffer_clone,
                (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE - 100,
            )
            .unwrap();
            buffer_clone.commit(res2.0);

            // Fill Node 1
            let res3 = reserve_without_gap(
                &buffer_clone,
                (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE,
            )
            .unwrap();
            buffer_clone.commit(res3.0);

            // Try to reserve again - wraps around to Node 0 (which still has res1 active)
            // This should block/spin until we commit res1.
            let res4 = reserve_without_gap(&buffer_clone, 100).unwrap();
            buffer_clone.commit(res4.0);

            writer_finished_clone.store(true, Ordering::Release);
        });

        // Give the thread some time to reach the block point
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!writer_finished.load(Ordering::Acquire));

        // Now commit res1, which unblocks the wraparound writer
        buffer.commit(res1.0);

        handle.join().unwrap();
        assert!(writer_finished.load(Ordering::Acquire));
    }

    #[test]
    fn test_extreme_disable_enable_stress() {
        let num_threads = 8;
        let msgs_per_thread = 100;
        let buffer = Arc::new(
            LocklessRingBuffer::new(5 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let mut handles = vec![];

        // 1. Spawn 8 writer threads writing continuously until they successfully write msgs_per_thread.
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                let mut count = 0;
                while count < msgs_per_thread {
                    match buffer_clone.reserve(TestMessage::SIZE) {
                        Ok((res, now, delta)) => {
                            let msg = TestMessage {
                                thread_index,
                                timestamp_nanos: now.into_nanos() as u64,
                                delta: delta.into_nanos() as u64,
                                data: *b"Event data\0\0",
                            };
                            res.write_at(0, &msg.to_bytes());
                            buffer_clone.commit(res);
                            count += 1;
                        }
                        Err(e) if e == starnix_uapi::errno!(ENOMEM) => {
                            // Expected when disabled. Yield and retry.
                            std::thread::yield_now();
                        }
                        Err(e) => panic!("Unexpected error during reserve: {:?}", e),
                    }
                }
                count
            });
            handles.push(handle);
        }

        // 2. Coordinator thread repeatedly disables and enables the ring buffer.
        let buffer_clone = Arc::clone(&buffer);
        let coordinator = std::thread::spawn(move || {
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                // Disable the ring buffer. This must successfully wait for all active writers.
                let _dropped = buffer_clone.disable().unwrap();

                // Ensure subsequent reservations fail while disabled.
                let res = buffer_clone.reserve(TestMessage::SIZE);
                assert_eq!(res.unwrap_err(), starnix_uapi::errno!(ENOMEM));

                std::thread::sleep(std::time::Duration::from_millis(2));
                // Re-enable the ring buffer.
                let _now = buffer_clone.enable().unwrap();
            }
        });

        coordinator.join().unwrap();
        let mut total_writes = 0;
        for handle in handles {
            total_writes += handle.join().unwrap();
        }
        println!("Disable/enable stress completed. Total writes = {}", total_writes);
        assert_eq!(total_writes, num_threads * msgs_per_thread);
    }

    #[test]
    fn test_reader_loop_swapping_high_contention() {
        let num_threads = 8usize;
        let msgs_per_thread = 100;
        // 8 threads * 100 msgs * 32 bytes = 25600 bytes.
        // Each page holds 127 messages.
        // Let's make a 10-page buffer to have ample space.
        let buffer = Arc::new(
            LocklessRingBuffer::new(10 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap(),
        );
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = vec![];

        // Spawn unified reader thread that loops read() draining all pages.
        let buffer_reader = Arc::clone(&buffer);
        let writers_done_reader = Arc::clone(&writers_done);
        let reader_handle = start_reader_thread(buffer_reader, writers_done_reader);

        // Spawn 8 writer threads.
        for thread_index in 0..num_threads {
            let buffer_clone = Arc::clone(&buffer);
            let handle = std::thread::spawn(move || {
                for _ in 0..msgs_per_thread {
                    let (res, now, delta) = buffer_clone.reserve(TestMessage::SIZE).unwrap();
                    let msg = TestMessage {
                        thread_index: thread_index as u32,
                        timestamp_nanos: now.into_nanos() as u64,
                        delta: delta.into_nanos() as u64,
                        data: *b"Event data\0\0",
                    };
                    res.write_at(0, &msg.to_bytes());
                    buffer_clone.commit(res);
                    // Yield to cause maximum interleaving
                    std::thread::yield_now();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
        writers_done.store(true, Ordering::Release);
        let all_messages = reader_handle.join().unwrap();
        let messages_read = all_messages.len();
        check_all_message_data(&all_messages, num_threads as u32);
        let dropped = buffer.dropped_pages();
        println!(
            "Reader high contention completed. Messages read = {}, dropped pages = {}",
            messages_read, dropped
        );
        // Verify that the total number of read messages + dropped pages' messages covers all writes.
        let messages_per_page =
            ((*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE) / TestMessage::SIZE;
        let total_written = num_threads * msgs_per_thread;
        let total_accounted = messages_read + (dropped as usize * messages_per_page);
        assert!(total_accounted >= total_written - messages_per_page);
    }

    #[test]
    fn test_failed_reservation_offset_boundary() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        let page_size = (*PAGE_SIZE) as usize;
        let max_payload = page_size - LocklessRingBuffer::PAGE_HEADER_SIZE;

        // Reserve enough space to almost fill Node 0
        let msg_size = 100;
        let num_msgs = max_payload / msg_size;
        for _ in 0..num_msgs {
            let (res, _, _) = reserve_without_gap(&buffer, msg_size).unwrap();
            buffer.commit(res);
        }

        // Check current write_offset of Node 0
        let expected_offset = LocklessRingBuffer::PAGE_HEADER_SIZE + num_msgs * msg_size;
        assert_eq!(buffer.nodes[0].write_offset.load(Ordering::Relaxed), expected_offset);

        // Now try a reservation that exceeds remaining space on Node 0.
        let (res, _, _) = reserve_without_gap(&buffer, msg_size).unwrap();
        assert_eq!(res.node_idx, 1);
        buffer.commit(res);

        // The failed reservation must not advance Node 0's write_offset, and finalizing Node 0
        // sealed it at that offset.
        assert_eq!(
            buffer.nodes[0].write_offset.load(Ordering::Relaxed),
            expected_offset | WRITE_OFFSET_SEALED_BIT
        );
    }

    #[test]
    fn test_sealed_write_offset_prevents_late_small_reservation() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let page_size = (*PAGE_SIZE) as usize;
        let header_size = LocklessRingBuffer::PAGE_HEADER_SIZE;

        // Leave 80 bytes remaining on Node 0.
        let initial_size = page_size - header_size - 80;
        let (res0, _, _) = buffer.reserve(initial_size).unwrap();
        res0.write_at(0, &vec![0xAA; initial_size]);
        buffer.commit(res0);

        // A late writer registers on Node 0 (increments `active_writers` while `PAGE_ACTIVE_BIT`
        // is still set) but has not claimed space yet when another writer finalizes Node 0.
        // Relaxed: this test is single-threaded. `PAGE_ACTIVE_BIT` is set, so `reserve()`'s
        // registration CAS would succeed here too.
        buffer.nodes[0].active_writers.fetch_add(1, Ordering::Relaxed);
        buffer.nodes[0].finalize();

        // The late writer now tries a 32-byte claim. 32 bytes fit in the 80 bytes left, but
        // Node 0 is sealed, so the claim must fail. Otherwise the record would sit past Node 0's
        // published `data_size` and readers would never see it.
        assert!(buffer.try_reserve_on_page(&buffer.nodes[0], 32, &zx::BootInstant::get).is_err());
        // The rejected claim left the sealed offset unchanged.
        assert_eq!(
            buffer.nodes[0].write_offset.load(Ordering::Relaxed),
            (header_size + initial_size) | WRITE_OFFSET_SEALED_BIT
        );
        buffer.nodes[0].release_writer();

        // The writer retries through `reserve()` and lands on Node 1.
        let (res_small, _, _) = buffer.reserve(32).unwrap();
        assert_eq!(res_small.node_idx, 1);
        buffer.commit(res_small);

        let mut dest = VecOutputBuffer::new(page_size);
        let bytes_read = buffer.read(&mut dest).unwrap();
        assert_eq!(bytes_read, page_size);
        assert_eq!(read_data_size(dest.data()), initial_size);
    }

    #[test]
    fn test_advance_reopens_page_sealed_while_empty() {
        // 4 nodes: ring 0 -> 1 -> 2 -> 0, and reader page 3.
        let buffer =
            LocklessRingBuffer::new(4 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let page_size = (*PAGE_SIZE) as usize;
        let header_size = LocklessRingBuffer::PAGE_HEADER_SIZE;
        let record_size = page_size - header_size;

        // A stale thread finalizes Node 1 before the tail reaches it. This is the state a stale
        // `finalize()` leaves on a page that `swap_reader_page()` re-inserted: sealed at
        // `PAGE_HEADER_SIZE`, `PAGE_ACTIVE_BIT` clear, `PAGE_FINALIZED_BIT` set, `data_size == 0`.
        buffer.nodes[1].finalize();
        assert_eq!(
            buffer.nodes[1].write_offset.load(Ordering::Relaxed),
            header_size | WRITE_OFFSET_SEALED_BIT
        );
        assert_eq!(
            buffer.nodes[1].active_writers.load(Ordering::Relaxed) & FLAGS_MASK,
            PAGE_FINALIZED_BIT
        );

        // Fill Node 0, so that the next reservation advances the tail to Node 1.
        let (res0, _, _) = buffer.reserve(record_size).unwrap();
        assert_eq!(res0.node_idx, 0);
        res0.write_at(0, &vec![0xAA; record_size]);
        buffer.commit(res0);

        // The advance resets Node 1, because its offset is sealed rather than exactly
        // `PAGE_HEADER_SIZE`, so the reservation lands on Node 1 instead of skipping it.
        let (res1, _, _) = buffer.reserve(record_size).unwrap();
        assert_eq!(res1.node_idx, 1);
        assert_eq!(buffer.tail_page.load(Ordering::Relaxed).index(), 1);
        assert_eq!(
            buffer.nodes[1].active_writers.load(Ordering::Relaxed),
            PAGE_ACTIVE_BIT | 1,
            "Node 1 must be active again, with the one writer registered"
        );
        res1.write_at(0, &vec![0xBB; record_size]);
        buffer.commit(res1);

        // Move the tail to Node 2, so that Node 1 is finalized with its record.
        let (res2, _, _) = buffer.reserve(32).unwrap();
        assert_eq!(res2.node_idx, 2);
        buffer.commit(res2);

        // The reader gets Node 0 and then Node 1, each with a full record.
        for fill in [0xAA, 0xBB] {
            let mut dest = VecOutputBuffer::new(page_size);
            assert_eq!(buffer.read(&mut dest).unwrap(), page_size);
            assert_eq!(read_data_size(dest.data()), record_size);
            assert!(dest.data()[header_size..].iter().all(|&b| b == fill));
        }
    }

    #[test]
    fn test_stale_tail_advance_across_wraparound() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        let max_payload = (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;

        // Capture the initial tail_val on Node 0 (generation 0, index 0).
        let stale_tail_val = buffer.tail_page.load(Ordering::Acquire);
        assert_eq!(stale_tail_val, TailVal::INITIAL);

        // Fill Node 0 and advance to Node 1.
        let (res0, _, _) = buffer.reserve(max_payload).unwrap();
        buffer.commit(res0);
        let (res1, _, _) = buffer.reserve(max_payload).unwrap();
        assert_eq!(res1.node_idx, 1);
        buffer.commit(res1);

        // Overflow Node 1, which forces an overwrite back onto Node 0 and moves head_page to
        // Node 1.
        let (res2, _, _) = buffer.reserve(100).unwrap();
        assert_eq!(res2.node_idx, 0);
        buffer.commit(res2);

        // Now tail_page points to Node 0 again (with an incremented generation counter),
        // head_page is Node 1, and nodes[0].next has FLAG_HEADER pointing to Node 1.
        let current_tail_val = buffer.tail_page.load(Ordering::Acquire);
        assert_eq!(current_tail_val.index(), 0);
        assert_eq!(current_tail_val.generation(), 2);
        assert_eq!(buffer.head_page.load(Ordering::Acquire), 1);
        assert_eq!(buffer.dropped_pages(), 1);

        // Simulate a delayed thread from generation 0 calling advance_to_next_page with
        // `stale_tail_val`. It must short-circuit at the entry check and NOT finalize Node 0, NOT
        // overwrite Node 1, and NOT advance `tail_page` or `head_page`. This covers only the entry
        // check. The re-check in the reset loop and the `tail_page` CAS would need the thread to
        // be paused inside `advance_to_next_page()`.
        let mut yield_tracker = YieldTracker::default();
        let res = buffer.advance_to_next_page(&buffer.nodes[0], stale_tail_val, &mut yield_tracker);
        assert_eq!(res, AdvanceResult::Advanced);
        assert_eq!(
            buffer.tail_page.load(Ordering::Acquire),
            current_tail_val,
            "Stale advance_to_next_page must not advance tail_page on a later generation"
        );
        assert_eq!(buffer.head_page.load(Ordering::Acquire), 1);
        assert_eq!(buffer.dropped_pages(), 1);
        assert_ne!(
            buffer.nodes[0].active_writers.load(Ordering::Acquire) & PAGE_ACTIVE_BIT,
            0,
            "Stale advance_to_next_page must not clear PAGE_ACTIVE_BIT on the active tail page"
        );

        // Subsequent reservation on Node 0 should still succeed on Node 0.
        let (res3, _, _) = buffer.reserve(100).unwrap();
        assert_eq!(res3.node_idx, 0);
        buffer.commit(res3);
    }

    #[test]
    fn test_new_rejects_more_nodes_than_tail_val_can_index() {
        // The check runs before the VMO is created, so this allocates nothing.
        let size_bytes = (TailVal::MAX_NODES + 1) * (*PAGE_SIZE) as usize;
        assert_eq!(
            LocklessRingBuffer::new(size_bytes, true, fuchsia_trace::Id::new()).err(),
            Some(errno!(ENOMEM))
        );
    }

    #[test]
    fn test_enable_on_new_ring_returns_creation_time() {
        let before_new = zx::BootInstant::get();
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        let after_new = zx::BootInstant::get();

        // `new()` creates the ring enabled, so enable() is a no-op that returns the time of the
        // reset done by `new()`.
        let created_at = buffer.enable().unwrap();
        assert!(before_new <= created_at && created_at <= after_new);
    }

    // Holds a reservation on the same thread in place of a writer racing on another thread, which
    // is enough because the reservation holds its references until commit.
    #[test]
    fn test_enable_with_reservation_in_flight_is_noop() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();

        // Start from a reset, as production does, so the reset time is known.
        buffer.disable().unwrap();
        let enabled_at = buffer.enable().unwrap();

        // Hold an in-flight reservation on node 0 while calling enable() on the enabled ring.
        let (res, _, _) = buffer.reserve(100).unwrap();
        assert_eq!(res.node_idx, 0);
        let ref_before = buffer.ref_count.load(Ordering::Acquire);

        // The no-op enable() returns the time of the last reset and leaves node 0 alone.
        assert_eq!(buffer.enable(), Ok(enabled_at));
        assert_eq!(buffer.ref_count.load(Ordering::Acquire), ref_before);
        assert_eq!(
            buffer.nodes[0].write_offset.load(Ordering::Relaxed),
            LocklessRingBuffer::PAGE_HEADER_SIZE + 100
        );

        // Committing the in-flight reservation must not underflow active_writers or ref_count.
        buffer.commit(res);
        assert_eq!(buffer.ref_count.load(Ordering::Acquire), RING_ENABLED_BIT);
        assert_eq!(buffer.nodes[0].active_writers.load(Ordering::Acquire) & ACTIVE_WRITERS_MASK, 0);
    }

    // `read()` takes its reference before it calls `available()`, so calling enable() from
    // `available()` runs it, on the same thread, while the reader holds that reference.
    #[test]
    fn test_enable_with_read_in_flight_is_noop() {
        struct EnablingOutputBuffer<'a> {
            buffer: &'a LocklessRingBuffer,
            enabled_at: zx::BootInstant,
        }
        impl std::fmt::Debug for EnablingOutputBuffer<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("EnablingOutputBuffer").finish_non_exhaustive()
            }
        }
        impl OutputBuffer for EnablingOutputBuffer<'_> {
            fn write_each(
                &mut self,
                _callback: &mut OutputBufferCallback<'_>,
            ) -> Result<usize, Errno> {
                Ok(0)
            }
            fn available(&self) -> usize {
                // The reader holds one reference. The no-op enable() must keep it.
                assert_eq!(self.buffer.ref_count.load(Ordering::Acquire), RING_ENABLED_BIT | 1);
                assert_eq!(self.buffer.enable(), Ok(self.enabled_at));
                assert_eq!(self.buffer.ref_count.load(Ordering::Acquire), RING_ENABLED_BIT | 1);
                (*PAGE_SIZE) as usize
            }
        }

        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        buffer.disable().unwrap();
        let enabled_at = buffer.enable().unwrap();

        // The ring is empty, so read() returns EAGAIN after `available()` runs.
        let mut dest = EnablingOutputBuffer { buffer: &buffer, enabled_at };
        assert_eq!(buffer.read(&mut dest), error!(EAGAIN));

        // Dropping the reader's reference must not underflow ref_count.
        assert_eq!(buffer.ref_count.load(Ordering::Acquire), RING_ENABLED_BIT);
    }

    #[test]
    fn test_reenable_resets_ring() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        let max_payload = (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;

        // Move the tail to node 1, so that the reset below is observable.
        let (res, _, _) = buffer.reserve(100).unwrap();
        assert_eq!(res.node_idx, 0);
        buffer.commit(res);
        let (res, _, _) = buffer.reserve(max_payload).unwrap();
        assert_eq!(res.node_idx, 1);
        buffer.commit(res);
        let tail_before = buffer.tail_page.load(Ordering::Acquire);
        assert_eq!(tail_before.index(), 1);

        // Disabling and re-enabling resets the ring to node 0 in the next generation, and later
        // no-op enables return the new time. Two clock reads can be equal, so the tail and offset
        // checks, not the time, show that the ring was reset.
        buffer.disable().unwrap();
        let reenabled_at = buffer.enable().unwrap();
        assert_eq!(buffer.tail_page.load(Ordering::Acquire), tail_before.next(0));
        assert_eq!(
            buffer.nodes[0].write_offset.load(Ordering::Relaxed),
            LocklessRingBuffer::PAGE_HEADER_SIZE
        );
        assert_eq!(buffer.enable(), Ok(reenabled_at));
    }

    #[test]
    fn test_decode_ftrace_page_reads_reserved_records() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        // Start after the ring's initial timestamp, so that no time is raised to it.
        let t0 = buffer.prev_timestamp.load(Ordering::Relaxed) + 1_000;
        let times = [t0, t0 + 5, t0 + 5 + LocklessRingBuffer::MAX_TIME_DELTA_NANOS];
        for (i, &t) in times.iter().enumerate() {
            let payload = format!("record {i}").into_bytes();
            let (res, now, delta) =
                reserve_at(&buffer, 8 + payload.len(), zx::BootInstant::from_nanos(t as i64))
                    .unwrap();
            assert_eq!(now.into_nanos() as u64, t);
            assert_eq!(res.header_time_delta() as i64, delta.into_nanos());
            write_ftrace_record(&res, &payload);
            buffer.commit(res);
        }
        // A record that fills a page moves the tail off Node 0, which finalizes it.
        let max_payload = (*PAGE_SIZE) as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let (res, _, _) = reserve_without_gap(&buffer, max_payload).unwrap();
        assert_eq!(res.node_idx, 1);
        buffer.commit(res);

        let mut dest = VecOutputBuffer::new((*PAGE_SIZE) as usize);
        buffer.read(&mut dest).unwrap();
        let (records, stamped) = decode_ftrace_page(dest.data());
        assert_eq!(records.iter().map(|r| r.timestamp).collect::<Vec<_>>(), times);
        assert_eq!(records[2].payload, b"record 2");
        assert!(stamped.is_empty());
    }

    #[test]
    fn test_encode_time_stamp() {
        let max = LocklessRingBuffer::MAX_TIME_DELTA_NANOS;
        for ts in [
            0,
            1,
            max,
            max + 1,
            10_000_000_000,
            1_000_000_000_000_000,
            LocklessRingBuffer::MAX_TIME_STAMP_NANOS,
            LocklessRingBuffer::MAX_TIME_STAMP_NANOS + 1,
            u64::MAX,
        ] {
            // A page that holds a `TIME_STAMP` record and a data record with a delta of 0.
            let mut page = vec![0u8; LocklessRingBuffer::PAGE_HEADER_SIZE];
            page[8..16].copy_from_slice(
                &(LocklessRingBuffer::TIME_STAMP_RECORD_SIZE as u64 + 12).to_le_bytes(),
            );
            page.extend_from_slice(&encode_time_stamp(ts));
            page.extend_from_slice(&0u32.to_le_bytes());
            page.extend_from_slice(&8u32.to_le_bytes());
            page.extend_from_slice(b"data");

            let (records, stamped) = decode_ftrace_page(&page);
            assert_eq!(stamped, vec![0]);
            // A time that doesn't fit in 59 bits saturates instead of wrapping.
            assert_eq!(
                records[0].timestamp,
                ts.min(LocklessRingBuffer::MAX_TIME_STAMP_NANOS),
                "ts {ts}"
            );
        }
    }

    #[test]
    fn test_time_stamp_record_only_for_deltas_over_27_bits() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let payload = b"launchingActivity#1";
        let record_size = 8 + payload.len();
        let t0 = injected_start(&buffer);
        let t1 = t0 + LocklessRingBuffer::MAX_TIME_DELTA_NANOS;
        let t2 = t1 + LocklessRingBuffer::MAX_TIME_DELTA_NANOS + 1;
        let t3 = t2 + 200_000_000;
        let t4 = t3 + 10_000_000_000;

        assert_eq!(write_record_at(&buffer, t0, payload), (0, 0));
        let mut offset = buffer.nodes[0].write_offset.load(Ordering::Relaxed);
        // A delta of exactly `MAX_TIME_DELTA_NANOS` fits in the header.
        assert_eq!(
            write_record_at(&buffer, t1, payload),
            (0, LocklessRingBuffer::MAX_TIME_DELTA_NANOS)
        );
        offset += record_size;
        assert_eq!(buffer.nodes[0].write_offset.load(Ordering::Relaxed), offset);
        // One more nanosecond, 200 ms, and 10 s each need a `TIME_STAMP` record.
        for (prev, next) in [(t1, t2), (t2, t3), (t3, t4)] {
            assert_eq!(write_record_at(&buffer, next, payload), (0, next - prev));
            offset += LocklessRingBuffer::TIME_STAMP_RECORD_SIZE + record_size;
            assert_eq!(buffer.nodes[0].write_offset.load(Ordering::Relaxed), offset);
        }

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0, t1, t2, t3, t4], 3));
    }

    #[test]
    fn test_time_stamp_records_decode_exactly_across_pages() {
        let buffer =
            LocklessRingBuffer::new(8 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let max = LocklessRingBuffer::MAX_TIME_DELTA_NANOS;
        let gaps = [1, 1_000, max, max + 1, 200_000_000, 7, 10_000_000_000, (3 << 27) + 5, 0];
        let mut now = injected_start(&buffer);
        let mut expected: Vec<Vec<DecodedRecord>> = vec![vec![]];
        // Fill 4 pages with records of varying sizes and gaps.
        for i in 0.. {
            now += gaps[i % gaps.len()];
            let mut payload = format!("event {i}").into_bytes();
            payload.resize(payload.len() + i % 13, b'.');
            let (node_idx, _) = write_record_at(&buffer, now, &payload);
            if node_idx == 4 {
                break;
            }
            if node_idx == expected.len() {
                expected.push(vec![]);
            }
            expected[node_idx].push(DecodedRecord { timestamp: now, payload });
        }

        let pages = read_ftrace_pages(&buffer);
        for (node_idx, records) in expected.iter().enumerate() {
            assert_eq!(&pages[node_idx].0, records, "page {node_idx}");
            // A record has a `TIME_STAMP` record exactly when it comes more than
            // `MAX_TIME_DELTA_NANOS` after the record before it on its page. The first record on
            // a page never has one.
            let expected_stamped: Vec<usize> = (1..records.len())
                .filter(|&j| records[j].timestamp - records[j - 1].timestamp > max)
                .collect();
            assert_eq!(pages[node_idx].1, expected_stamped, "stamped records on page {node_idx}");
        }
        assert!(pages.iter().take(4).map(|page| page.1.len()).sum::<usize>() > 0);
    }

    #[test]
    fn test_first_record_on_page_has_no_time_stamp_record() {
        let buffer =
            LocklessRingBuffer::new(4 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let page_size = (*PAGE_SIZE) as usize;
        let max_payload = page_size - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let t0 = injected_start(&buffer);
        // Leave 10 bytes free on Node 0, so the next record moves to Node 1.
        assert_eq!(write_record_at(&buffer, t0, &vec![0xaa; max_payload - 8 - 10]).0, 0);

        // After a long gap, the record is the first on Node 1. The page header carries its time,
        // so it needs no `TIME_STAMP` record and its delta is 0.
        let t1 = t0 + 1_000_000_000;
        assert_eq!(write_record_at(&buffer, t1, b"first on page"), (1, 0));
        assert_eq!(
            buffer.nodes[1].write_offset.load(Ordering::Relaxed),
            LocklessRingBuffer::PAGE_HEADER_SIZE + 8 + 13
        );

        // A record of the largest size `reserve()` accepts has no room for a `TIME_STAMP`
        // record, but still succeeds on the next empty page.
        let t2 = t1 + 1_000_000_000;
        assert_eq!(write_record_at(&buffer, t2, &vec![0xbb; max_payload - 8]), (2, 0));

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0], 0));
        assert_eq!(timestamps(&pages[1]), (vec![t1], 0));
        assert_eq!(timestamps(&pages[2]), (vec![t2], 0));
    }

    #[test]
    fn test_record_moves_to_next_page_when_time_stamp_record_does_not_fit() {
        let buffer =
            LocklessRingBuffer::new(4 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let page_size = (*PAGE_SIZE) as usize;
        let max_payload = page_size - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let payload = b"rollover";
        let record_size = 8 + payload.len();
        let gap = 500_000_000;
        let t0 = injected_start(&buffer);

        // Node 0: leave room for the record but not for the record and a `TIME_STAMP` record.
        let free = record_size + LocklessRingBuffer::TIME_STAMP_RECORD_SIZE - 1;
        assert_eq!(write_record_at(&buffer, t0, &vec![0xbb; max_payload - 8 - free]).0, 0);
        // The record moves to Node 1 instead of going without its `TIME_STAMP` record.
        let t1 = t0 + gap;
        assert_eq!(write_record_at(&buffer, t1, payload), (1, 0));
        assert_eq!(
            buffer.nodes[0].write_offset.load(Ordering::Relaxed),
            (page_size - free) | WRITE_OFFSET_SEALED_BIT
        );

        // Node 1: leave room for exactly the record and a `TIME_STAMP` record, which then fill it.
        let t2 = t1 + 1;
        let free = record_size + LocklessRingBuffer::TIME_STAMP_RECORD_SIZE;
        let filler = vec![0xcc; max_payload - record_size - 8 - free];
        assert_eq!(write_record_at(&buffer, t2, &filler).0, 1);
        let t3 = t2 + gap;
        assert_eq!(write_record_at(&buffer, t3, payload), (1, gap));
        // The last writer on a full page finalizes it, so Node 1 may already be sealed.
        assert_eq!(Node::unsealed(buffer.nodes[1].write_offset.load(Ordering::Relaxed)), page_size);

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0], 0));
        assert_eq!(timestamps(&pages[1]), (vec![t1, t2, t3], 1));
    }

    #[test]
    fn test_time_stamp_records_decode_exactly_when_stamped_out_of_order() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let t0 = injected_start(&buffer);
        write_record_at(&buffer, t0, b"first");
        // After an idle gap, writer A claims space for a record and a `TIME_STAMP` record. Before
        // A stamps them, writer B claims and stamps its own, at a later time.
        let t_a = t0 + 1_000_000_000;
        let t_b = t_a + 10;
        set_test_hook(TestHook::AfterClaim, move |buffer| {
            assert_eq!(write_record_at(buffer, t_b, b"writer B"), (0, t_b - t0));
        });
        let (res, now, delta) =
            reserve_at(&buffer, 16, zx::BootInstant::from_nanos(t_a as i64)).unwrap();
        assert!(!test_hook_is_armed());
        // A's `fetch_max` returns B's later time, so A gets that time and a delta of 0. A
        // relative delta would decode from the record before A on the page, at `t0`.
        assert_eq!(now.into_nanos() as u64, t_b);
        assert_eq!(delta.into_nanos(), 0);
        assert_eq!(res.header_time_delta(), 0);
        write_ftrace_record(&res, b"writer A");
        buffer.commit(res);
        let t_c = t_b + 5;
        assert_eq!(write_record_at(&buffer, t_c, b"third"), (0, 5));

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0, t_b, t_b, t_c], 2));
    }

    #[test]
    fn test_time_stamp_record_written_after_page_finalized() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let t0 = injected_start(&buffer);
        write_record_at(&buffer, t0, b"first");
        // After an idle gap, writer W claims space for a record and a `TIME_STAMP` record. Before
        // W stamps them, another writer finalizes Node 0 and moves the tail to Node 1. W's pin
        // keeps Node 0 from being read or reset in the meantime. The hook's `finalize()` call
        // borrows W's pin, which `finalize()` requires.
        set_test_hook(TestHook::AfterClaim, |buffer| {
            buffer.nodes[0].finalize();
            let tail_val = buffer.tail_page.load(Ordering::Relaxed);
            let mut yield_tracker = YieldTracker::default();
            assert_eq!(
                buffer.advance_to_next_page(&buffer.nodes[0], tail_val, &mut yield_tracker),
                AdvanceResult::Advanced
            );
        });
        let t1 = t0 + 1_000_000_000;
        assert_eq!(write_record_at(&buffer, t1, b"late"), (0, t1 - t0));
        assert!(!test_hook_is_armed());

        // The seal counted W's claim, so the reader gets both records, with exact times.
        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0, t1], 1));
    }

    #[test]
    fn test_claim_retry_rechecks_time_stamp_need() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let t0 = injected_start(&buffer);
        write_record_at(&buffer, t0, b"first");
        // After an idle gap, writer W sizes its claim with a `TIME_STAMP` record. Before W's CAS,
        // writer X claims and stamps a record, with its own `TIME_STAMP` record.
        let t1 = t0 + 1_000_000_000;
        set_test_hook(TestHook::BeforeClaim, move |buffer| {
            assert_eq!(write_record_at(buffer, t1, b"writer X"), (0, t1 - t0));
        });
        // W's CAS fails. Its retry reads X's time from `prev_timestamp`, so W needs no
        // `TIME_STAMP` record and its delta fits in the header.
        assert_eq!(write_record_at(&buffer, t1 + 1, b"writer W"), (0, 1));
        assert!(!test_hook_is_armed());

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0, t1, t1 + 1], 1));
    }

    #[test]
    fn test_claim_retry_reads_clock_again() {
        let buffer =
            LocklessRingBuffer::new(3 * (*PAGE_SIZE) as usize, false, fuchsia_trace::Id::new())
                .unwrap();
        let t0 = injected_start(&buffer);
        write_record_at(&buffer, t0, b"first");
        // Writer W reads the time `t1` and sizes its claim without a `TIME_STAMP` record. Before
        // W's CAS, writer X writes a record 1 ms later, and then the system idles for 200 ms.
        let t1 = t0 + 1_000;
        let t_x = t1 + 1_000_000;
        let t2 = t_x + 200_000_000;
        set_test_hook(TestHook::BeforeClaim, move |buffer| {
            assert_eq!(write_record_at(buffer, t_x, b"writer X"), (0, t_x - t0));
        });
        let clock_reads = Cell::new(0);
        let current_boot_time = || {
            clock_reads.set(clock_reads.get() + 1);
            zx::BootInstant::from_nanos(if clock_reads.get() == 1 { t1 } else { t2 } as i64)
        };
        let (res, now, delta) = buffer.reserve_with_clock(16, current_boot_time).unwrap();
        assert!(!test_hook_is_armed());
        // W's CAS fails and its retry reads the clock again, so W gets the time it lands at. With
        // `t1`, W would get X's time and a delta of 0, 200 ms before it wrote.
        assert_eq!(now.into_nanos() as u64, t2);
        assert_eq!(delta.into_nanos() as u64, t2 - t_x);
        assert_eq!(res.header_time_delta(), 0);
        write_ftrace_record(&res, b"writer W");
        buffer.commit(res);

        let pages = read_ftrace_pages(&buffer);
        assert_eq!(timestamps(&pages[0]), (vec![t0, t_x, t2], 1));
    }

    #[test]
    fn test_time_stamp_records_survive_overwrite() {
        let buffer =
            LocklessRingBuffer::new(4 * (*PAGE_SIZE) as usize, true, fuchsia_trace::Id::new())
                .unwrap();
        let gaps =
            [1_000, LocklessRingBuffer::MAX_TIME_DELTA_NANOS + 1, 200_000_000, 7, 10_000_000_000];
        let mut now = injected_start(&buffer);
        let mut expected = std::collections::HashMap::new();
        // Write until the ring has wrapped and dropped pages.
        for i in 0.. {
            if buffer.dropped_pages() >= 3 {
                break;
            }
            assert!(i < 100_000, "the ring never dropped pages");
            now += gaps[i % gaps.len()];
            let mut payload = format!("overwrite {i}").into_bytes();
            payload.resize(payload.len() + i % 11, b'.');
            write_record_at(&buffer, now, &payload);
            expected.insert(payload, now);
        }

        // Every page that is left decodes, with exact times.
        let pages = read_ftrace_pages(&buffer);
        assert!(!pages.is_empty());
        for (records, _) in &pages {
            for record in records {
                assert_eq!(record.timestamp, expected[&record.payload]);
            }
        }
        assert!(pages.iter().map(|page| page.1.len()).sum::<usize>() > 0);
    }
}
