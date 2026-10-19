// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::Deadline;
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::{self, AutoPreemptDisabler, ThreadPtr};
use crate::platform_rs::timer::InstantMono;
use crate::vm::page::VmPagePtr;
use crate::vm::pmm;
use crate::vm::vm_cow_pages::{self, VmCowPages};
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use ksync::guarded;
use pin_init::{PinInit, pin_data, pin_init, stack_pin_init};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use zx_status::Status;

unsafe extern "C" {
    fn cpp_global_prng_draw(buffer: *mut u8, len: usize);
}

crate::counters::define_kcounter!(
    PQ_COMPRESS_DEBUG_RANDOM_COMPRESSION,
    "pq.compress.debug_random_compression",
    Sum
);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Shutdown,
    Running,
    Paused,
}

#[derive(Default)]
struct Entry {
    cow: Option<fbl::RefPtr<VmCowPages>>,
    page: Option<VmPagePtr>,
    offset: u64,
}

// Size of the |list_| that will be allocated.
const ARRAY_SIZE: usize = 128;

/// A debug compressor that can be given references to pages in VMOs and will randomly compress a
/// subset of them. The compression will be performed in a difference Zircon thread, so the pages
/// can be given with arbitrary locks held.
#[guarded]
#[pin_data(PinnedDrop)]
pub struct DebugCompressor {
    #[mutex]
    lock: ksync::KMutex<ksync::RawSpinlock>,

    // Reference to the thread that does compression so we can shut it down later.
    #[guarded_by(lock)]
    thread: Option<ThreadPtr>,

    // The array of entries is used to transfer pages from the synchronous |Add| call to the
    // compression thread. The list is finite and if full pages will be silently dropped, which is
    // equivalent to as if those pages were randomly chosen to not be added.
    #[guarded_by(lock)]
    list: Option<kalloc::Box<fbl::RingBuffer<Entry, ARRAY_SIZE>>>,

    // State is initially shutdown to require |init| to be called.
    #[guarded_by(lock)]
    state: State,

    // Private rng state to avoid further synchronization overhead with the global rand().
    #[guarded_by(lock)]
    rng: SmallRng,

    // Used to signal the compression thread if the list goes from empty->non-empty.
    #[pin]
    event: AutounsignalEvent,

    phantom: PhantomData<PhantomPinned>,
}

impl DebugCompressor {
    /// Creates an in-place initializer for `DebugCompressor`.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            lock <- ksync::KSpinlock::init(),
            thread: None.into(),
            list: None.into(),
            state: State::Shutdown.into(),
            rng: SmallRng::seed_from_u64(0).into(),
            event <- AutounsignalEvent::init_unsignaled(),
            phantom: PhantomData,
        })
    }

    /// Initializes the debug compressor. This method may acquire Mutexes, and so must not be called
    /// with any spinlocks held. Other methods should not be called before `init_thread` is called
    /// and returns `Ok(())`.
    pub fn init_thread(&self) -> Result<(), Status> {
        // Allocate objects outside the lock.
        // The compression thread is made as HIGH_PRIORITY since it will be holding refptrs to VMOs,
        // and could be inadvertently keeping those VMOs alive if the list is not processed fast
        // enough.
        let list = kalloc::Box::try_new(fbl::RingBuffer::new()).map_err(|_| Status::NO_MEMORY)?;

        // SAFETY: `self` as raw pointer lives for the duration of the thread.
        let thread = unsafe {
            thread::create_with_priority(
                c"page-queue-debug-compress-thread".as_ptr(),
                Self::compress_thread_entry,
                self as *const Self as *mut c_void,
                thread::HIGH_PRIORITY,
            )
        }?;

        let mut seed = [0u8; 32];
        // SAFETY: cpp_global_prng_draw writes size_of_val(&seed) bytes into seed.
        unsafe {
            cpp_global_prng_draw(seed.as_mut_ptr(), core::mem::size_of_val(&seed));
        }
        let rng = SmallRng::from_seed(seed);

        let _apd = AutoPreemptDisabler::new();
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        assert!(fields.thread.is_none());
        *fields.list = Some(list);
        *fields.thread = Some(thread);
        // SAFETY: thread is newly created and valid to resume.
        unsafe { thread.resume() };
        *fields.state = State::Running;
        *fields.rng = rng;
        Ok(())
    }

    /// Adds the specified `page` at `offset` in `object` to the debug compressor as a candidate for
    /// compression. The `page` and `object` must remain valid until `add` returns. This implies that
    /// `object` lock must be held, however this cannot be stated with an TA_REQ statement since
    /// VmCowPages is not declared yet.
    pub fn add(&self, page: VmPagePtr, object: &VmCowPages, offset: u64) {
        let signal = {
            let _apd = AutoPreemptDisabler::new();
            ksync::lock!(let mut guard = self.lock_lock());
            let fields = guard.as_mut().fields_mut();
            if *fields.state != State::Running {
                return;
            }
            let list = match fields.list.as_mut() {
                Some(l) => l,
                None => return,
            };
            if list.is_full() {
                return;
            }
            // Allow 10% of pages to get added.
            if !fields.rng.random_ratio(1, 10) {
                return;
            }
            // If currently empty then the compression thread will need a kick.
            let signal = list.is_empty();
            // SAFETY: We hold a reference to |object| so it is valid and cannot be destroyed during
            // this method.
            let cow = unsafe { fbl::RefPtr::make_ref_ptr_upgrade_from_raw(object) };
            let entry = Entry { cow, page: Some(page), offset };
            // Callers of |Add| are required to ensure |object| lives till the end of the call, so the
            // upgrade should never fail.
            assert!(entry.cow.is_some());
            list.push(entry);
            signal
        };
        if signal {
            self.event.signal();
        }
    }

    /// Pauses the debug compressor such that all future `add` calls will be ignored. It is an error to
    /// call `pause` twice without calling `resume` in between. Pause might acquire arbitrary VMO and
    /// other locks and should not be called with other locks held.
    pub fn pause(&self) {
        lockdep::assert_no_locks_held();
        {
            ksync::lock!(let mut guard = self.lock_lock());
            let fields = guard.as_mut().fields_mut();
            if *fields.state == State::Shutdown {
                return;
            }
            assert!(*fields.state != State::Paused);
            *fields.state = State::Paused;
        }
        // Drain list so that we aren't holding VMOs alive during an arbitrarily long pause.
        while self.pop().is_some() {}
    }

    /// Resumes from a `pause`, causing calls to `add` to no longer be ignored. It is an error to call
    /// `resume` except after having called `pause`.
    pub fn resume(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        if *fields.state == State::Shutdown {
            return;
        }
        assert!(*fields.state != State::Running);
        // The list should be empty as nothing should have been added while paused.
        assert!(fields.list.as_ref().expect("Should not be null").is_empty());
        *fields.state = State::Running;
    }

    // Helper that Pops an entry from the list_. Will return an entry with a null cow if the list is
    // empty.
    fn pop(&self) -> Option<Entry> {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        let list = fields.list.as_mut()?;
        if list.is_empty() {
            return None;
        }
        let front = list.front_mut();
        let entry = Entry { cow: front.cow.take(), page: front.page.take(), offset: front.offset };
        list.pop();
        Some(entry)
    }

    // Shuts down and cleans up |thread_|.
    fn shutdown(&self) {
        let thread = {
            ksync::lock!(let mut guard = self.lock_lock());
            let fields = guard.as_mut().fields_mut();
            if fields.thread.is_some() {
                assert!(*fields.state != State::Shutdown);
                *fields.state = State::Shutdown;
                self.event.signal();
                fields.thread.take()
            } else {
                None
            }
        };
        if let Some(t) = thread {
            // SAFETY: thread is valid and being joined only once upon shutdown.
            let status = unsafe { t.join(InstantMono::INFINITE) };
            assert!(status.is_ok());
        }
    }

    extern "C" fn compress_thread_entry(arg: *mut c_void) -> i32 {
        // SAFETY: arg points to a live DebugCompressor.
        let compressor = unsafe { &*(arg as *const Self) };
        compressor.compress_thread();
        0
    }

    // Entry point for the |thread_| that performs the actual compression.
    fn compress_thread(&self) {
        let compression = pmm::node().get_page_compression();
        // It is an error to attempt to be using the debug compressor if compression isn't available.
        let compression = compression.expect("debug compressor requires page compression");

        loop {
            // Check for Shutdown prior to waiting to ensure that if a shutdown was triggered while we were
            // processing entries we do not miss it.
            {
                ksync::lock!(let guard = self.lock_lock());
                if *guard.fields().state == State::Shutdown {
                    return;
                }
            }
            let status = self.event.wait(&Deadline::infinite());
            assert!(status.is_ok());

            // Acquire the compression instance, cannot keep this acquired between runs to avoid starving
            // any legitimate compression efforts.
            stack_pin_init!(let instance = compression.acquire_compressor());

            // Work through all items in the list and attempt to compress them.
            while let Some(entry) = self.pop() {
                let status = instance.as_mut().get().arm();
                if status.is_err() {
                    // arm() may fail with NO_MEMORY if it cannot allocate temporary resources.
                    // Since this is for random debug compression, we can safely skip this page and continue. We
                    // still want to process the rest of the list to drop RefPtrs to the VmCowPages which might
                    // be holding them alive.
                    continue;
                }
                if let Some(ref cow) = entry.cow {
                    let page = entry.page.expect("entry with cow must have page");
                    // SAFETY: `page` was validated when added to list, `entry.offset` is the
                    // page's offset in `cow`, and `instance` is an armed compressor guard.
                    let reclaimed = unsafe {
                        cow.reclaim_page(
                            page,
                            entry.offset,
                            vm_cow_pages::EvictionAction::IgnoreHint,
                            Some(instance.as_mut().get()),
                        )
                    };
                    if let Ok(reclaimed) = reclaimed {
                        let count = reclaimed.num_pages;
                        if count > 0 {
                            PQ_COMPRESS_DEBUG_RANDOM_COMPRESSION.add(count as i64);
                        }
                    }
                }
            }
        }
    }
}

#[pin_init::pinned_drop]
impl PinnedDrop for DebugCompressor {
    fn drop(self: Pin<&mut Self>) {
        self.as_ref().get_ref().shutdown();
    }
}
