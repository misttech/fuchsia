// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// OwnedWaitQueue topology tests
#[cfg(ktest)]
#[unittest::suite(name = "owq")]
mod owq_topology_tests {
    use core::ffi::c_void;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering};
    use kprint::kprintln;
    use lazy_init::LazyInit;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_false, assert_lt, assert_nonnull, assert_null, assert_true, expect_eq, expect_ok,
    };
    use zr::AtomicConstPtr;
    use zx_types::zx_instant_mono_t;

    use crate::arch_rs::max_num_cpus;
    use crate::kconcurrent::{
        ChainLockTransaction, ChainLockTransactionToken, ChainLocked, IRQ_SAVE_OPTION, OwnerGuard,
        PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, TransactionResult, acquire_both_or_backoff, clt_tag,
        single_chain_lock_guard_owned,
    };
    use crate::kernel::auto_preempt_disabler::AnnotatedAutoPreemptDisabler;
    use crate::kernel::deadline::Deadline;
    use crate::kernel::event::Event;
    use crate::kernel::mp::get_online_mask;
    use crate::kernel::owned_wait_queue::{DefaultWakeHooks, OwnedWaitQueue, WakeOption};
    use crate::kernel::scheduler_state::{SchedulerStateBaseProfile, ThreadStateKind};
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, PreemptDisabledToken, Thread, ThreadPtr};
    use crate::kernel::wait_queue::{
        ForceInheritance, Interruptible, ResourceOwnership, WaitQueue,
    };
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};

    macro_rules! begin_test {
        ($all_ok:ident) => {
            #[allow(unused_mut)]
            let mut $all_ok = true;
            #[allow(unused_macros)]
            macro_rules! record_failure {
                () => {
                    #[allow(unused_assignments)]
                    {
                        $all_ok = false;
                    }
                };
            }
        };
    }

    const INVALID: u32 = u32::MAX;

    static FINAL_EXIT_QUEUE: LazyInit<WaitQueue> = LazyInit::uninit();

    fn final_exit_queue_init(_level: init::LkInitLevel) {
        // SAFETY: Single-threaded initialization during early boot before any test runs.
        let _ = unsafe { Pin::static_ref(&FINAL_EXIT_QUEUE).init_pin(WaitQueue::init()) };
    }

    init::lk_init_hook!(
        owq_topology_final_exit_queue_init,
        final_exit_queue_init,
        init::LK_INIT_LEVEL_EARLIEST
    );

    struct OwnedWaitQueueTopologyTests;

    // All fields use atomics for interior mutability because `TestThread` is shared via
    // `&TestThread` between the test setup thread and the spawned `thread_entry` worker (`main`).
    // `exit_now`, `do_baao`, and `thread` use `Ordering::SeqCst` to synchronize lifecycle and
    // publish the `Ordering::Relaxed` setup fields (`has_start_time_gate`, `start_time_gate`,
    // `owner_locked`, `target_queue`, and `target_owner`) before `do_baao` is set to `true`.
    struct TestThread {
        exit_now: AtomicBool,
        do_baao: AtomicBool,
        has_start_time_gate: AtomicBool,
        start_time_gate: AtomicI64,
        owner_locked: AtomicBool,
        target_queue: AtomicConstPtr<OwnedWaitQueue>,
        target_owner: AtomicPtr<Thread>,
        thread: AtomicPtr<Thread>,
    }

    impl Default for TestThread {
        fn default() -> Self {
            Self {
                exit_now: AtomicBool::new(false),
                do_baao: AtomicBool::new(false),
                has_start_time_gate: AtomicBool::new(false),
                start_time_gate: AtomicI64::new(0),
                owner_locked: AtomicBool::new(false),
                target_queue: AtomicConstPtr::new(core::ptr::null()),
                target_owner: AtomicPtr::new(core::ptr::null_mut()),
                thread: AtomicPtr::new(core::ptr::null_mut()),
            }
        }
    }

    impl Drop for TestThread {
        fn drop(&mut self) {
            debug_assert!(
                self.thread.load(Ordering::SeqCst).is_null(),
                "TestThread must be shut down before Drop forms `&mut self`"
            );
            self.shutdown();
        }
    }

    impl TestThread {
        fn deadline_for_index(index: usize) -> DurationMono {
            const K_BASE_DURATION: DurationMono = DurationMono::from_millis(10);
            const K_DURATION_INC: DurationMono = DurationMono::from_millis(1);
            DurationMono(
                K_BASE_DURATION.into_nanos() + (index as i64 * K_DURATION_INC.into_nanos()),
            )
        }

        fn init(&self, base_profile: Option<&SchedulerStateBaseProfile>) -> bool {
            begin_test!(all_ok);

            assert_null!(self.thread.load(Ordering::SeqCst));
            extern "C" fn thread_entry(arg: *mut c_void) -> i32 {
                // SAFETY: `arg` points to a valid `TestThread` that outlives this thread.
                unsafe { (*arg.cast::<TestThread>()).main() }
            }
            // SAFETY: `c"owq_test_thread"` is a valid C string, `thread_entry` is safe to run with
            // `self` as its argument, and the thread is joined in `shutdown` before `self` is
            // dropped.
            let created = unsafe {
                thread::create_with_priority(
                    c"owq_test_thread".as_ptr(),
                    thread_entry,
                    core::ptr::from_ref(self).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            };
            assert_true!(created.is_ok());
            let thread_ptr = created.unwrap();
            self.thread.store(thread_ptr.as_raw(), Ordering::SeqCst);
            if let Some(base_profile) = base_profile {
                // SAFETY: `thread_ptr` is a valid, live thread and we are in normal thread context.
                unsafe { thread_ptr.set_base_profile(base_profile) };
            }
            // SAFETY: `thread_ptr` was just created and has not been resumed or joined yet.
            unsafe { thread_ptr.resume() };

            all_ok
        }

        fn shutdown(&self) {
            self.exit_now.store(true, Ordering::SeqCst);
            let raw = self.thread.swap(core::ptr::null_mut(), Ordering::SeqCst);
            // SAFETY: Any non-null pointer stored in `self.thread` is a resumed, unjoined `Thread`,
            // and `swap` ensures it is joined at most once.
            if let Some(thread_ptr) = unsafe { ThreadPtr::from_raw(raw) } {
                // SAFETY: `thread_ptr` is valid and unjoined.
                unsafe {
                    thread_ptr.kill();
                    let _unused_retcode = thread_ptr.join(InstantMono::INFINITE);
                }
            }
        }

        fn set_start_time_gate(&self, time: zx_instant_mono_t) {
            // `Relaxed` is sufficient: synchronized with `main` by the `SeqCst` store/load on
            // `do_baao` in `do_block` and `main`.
            self.start_time_gate.store(time, Ordering::Relaxed);
            self.has_start_time_gate.store(true, Ordering::Relaxed);
        }

        fn set_owner_locked(&self, value: bool) {
            // `Relaxed` is sufficient: synchronized with `main` by the `SeqCst` store/load on
            // `do_baao` in `do_block` and `main`.
            self.owner_locked.store(value, Ordering::Relaxed);
        }

        fn thread(&self) -> *mut Thread {
            self.thread.load(Ordering::SeqCst)
        }

        fn target_queue(&self) -> *const OwnedWaitQueue {
            // `Relaxed` is sufficient: `target_queue` is only written on the setup thread in
            // `do_block` before setting `do_baao`.
            self.target_queue.load(Ordering::Relaxed)
        }

        fn blocking_queue(&self) -> *const OwnedWaitQueue {
            let thread_ptr = self.thread();
            assert!(!thread_ptr.is_null());
            // SAFETY: `thread_ptr` is non-null and valid until joined in `shutdown`.
            let thread_ref = unsafe { &*thread_ptr };
            single_chain_lock_guard_owned!(
                guard,
                thread_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("OwnedWaitQueueTopologyTests::TestThread::blocking_queue")
            );
            let proof = guard.proof();
            proof
                .wait_queue_state()
                .blocking_wait_queue(proof.token())
                // SAFETY: `wq` is the thread's live `blocking_wait_queue` read under the thread
                // lock.
                .and_then(|wq| unsafe { OwnedWaitQueue::downcast_ptr(wq) })
                .map_or(core::ptr::null(), |owq| owq.as_ptr().cast_const())
        }

        fn start_time(&self) -> zx_instant_mono_t {
            let thread_ptr = self.thread();
            assert!(!thread_ptr.is_null());
            // SAFETY: `thread_ptr` is non-null and valid until joined in `shutdown`.
            let thread_ref = unsafe { &*thread_ptr };
            single_chain_lock_guard_owned!(
                guard,
                thread_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("OwnedWaitQueueTopologyTests::TestThread::start_time")
            );
            guard.proof().dynamic_parameters().0.raw_value()
        }

        fn finish_time(&self) -> zx_instant_mono_t {
            let thread_ptr = self.thread();
            assert!(!thread_ptr.is_null());
            // SAFETY: `thread_ptr` is non-null and valid until joined in `shutdown`.
            let thread_ref = unsafe { &*thread_ptr };
            single_chain_lock_guard_owned!(
                guard,
                thread_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("OwnedWaitQueueTopologyTests::TestThread::finish_time")
            );
            guard.proof().dynamic_parameters().1.raw_value()
        }
    }

    #[derive(Default)]
    struct TestQueue {
        wait_queue: Option<Pin<kalloc::Box<OwnedWaitQueue>>>,
    }

    impl TestQueue {
        fn init(&mut self) -> bool {
            begin_test!(all_ok);

            assert_null!(self.wait_queue());
            let wq = kalloc::Box::try_pin_init(OwnedWaitQueue::init()).ok();
            assert_true!(wq.is_some());
            self.wait_queue = wq;

            all_ok
        }

        fn wait_queue(&self) -> *const OwnedWaitQueue {
            self.wait_queue.as_ref().map_or(core::ptr::null(), |wq| {
                core::ptr::from_ref::<OwnedWaitQueue>(wq.as_ref().get_ref())
            })
        }

        fn wait_queue_ref(&self) -> &OwnedWaitQueue {
            self.wait_queue.as_deref().expect("TestQueue not initialized")
        }

        fn owner(&self) -> *mut Thread {
            let wait_queue = self.wait_queue_ref();
            single_chain_lock_guard_owned!(
                guard,
                wait_queue,
                IRQ_SAVE_OPTION,
                clt_tag!("OwnedWaitQueueTopologyTests::TestQueue::owner")
            );
            guard.proof().queue_owner().map_or(core::ptr::null_mut(), ThreadPtr::as_raw)
        }
    }

    struct Action {
        /// Index of the blocking thread.
        thread_index: u32,
        /// Index of the queue which the thread blocks behind.
        queue_index: u32,
        /// Index of the thread declared to be the owner.
        owning_thread_index: u32,
        /// True if we expect the owner to be accepted, false if
        /// we expect the owner to be rejected (and end up with
        /// no owner after the action.
        owner_expected: bool,
        /// True if we expect the owner to be locked, prior to
        /// being assigned as the owner.
        owner_locked: bool,
    }

    struct RequeueAction {
        wake_count: u32,
        requeue_count: u32,
        requeue_owner_index: u32,
        assign_woken: bool,
    }

    impl OwnedWaitQueueTopologyTests {
        fn shutdown_threads<const THREAD_COUNT: usize>(threads: &[TestThread; THREAD_COUNT]) {
            for t in threads {
                t.shutdown();
            }
        }

        fn run_baao_test<
            const THREAD_COUNT: usize,
            const QUEUE_COUNT: usize,
            const ACTION_COUNT: usize,
        >(
            threads: &[TestThread; THREAD_COUNT],
            queues: &mut [TestQueue; QUEUE_COUNT],
            actions: &[Action; ACTION_COUNT],
            setup_for_requeue_test: bool,
        ) -> bool {
            begin_test!(all_ok);
            let mut shutdown_on_exit = zr::defer(|| Self::shutdown_threads(threads));

            // Initialize all of the threads and queues.  We cannot proceed if any of this
            // fails.
            for (i, thread) in threads.iter().enumerate() {
                if !setup_for_requeue_test {
                    assert_true!(thread.init(None));
                } else {
                    let deadline = TestThread::deadline_for_index(i);
                    let capacity = DurationMono(deadline.into_nanos() / 2);
                    let profile = SchedulerStateBaseProfile::deadline(capacity, deadline);
                    assert_true!(thread.init(Some(&profile)));
                }
            }

            for queue in queues.iter_mut() {
                assert_true!(queue.init());
            }

            // Perform each of the actions, blocking a given thread behind a given wait
            // queue and optionally declaring an owner as we do.  Afterwards, verify that
            // each of the threads is blocked behind the queue we expect, and that the
            // owner is who we expect.
            let mut previous_thread: Option<&TestThread> = None;
            for action in actions {
                // Make sure the action indexes are valid.
                assert_lt!(action.thread_index as usize, threads.len());
                assert_lt!(action.queue_index as usize, queues.len());

                let blocking_thread = &threads[action.thread_index as usize];
                let target_queue = &queues[action.queue_index as usize];
                let mut new_owner: Option<&TestThread> = None;
                if action.owning_thread_index != INVALID {
                    assert_lt!(action.owning_thread_index as usize, threads.len());
                    new_owner = Some(&threads[action.owning_thread_index as usize]);
                }

                // Make certain that when we are setting up for a re-queue test that we
                // carefully control the order that the threads will be selected from the
                // wait queue when it is time to perform the actual wake-and-requeue
                // operation.
                //
                // Note; this is not normally something that tests can depend on.  In
                // theory, the priority order of a wait queue is officially "not your
                // business" and not formally specified.  When it comes to the specific
                // case of in-kernel unit tests and keeping them deterministic, it is
                // probably OK to be aware of the specifics of how things are ordered,
                // even though this means that when the sorting invariant for the wait
                // queue changes (not a common thing at all), this test will need to be
                // updated as well.
                //
                // At any rate, currently the wake order of threads in a wait queue is
                // controlled completely by their finish time.  Threads with earlier
                // finish times get woken before threads with later finish times.  The
                // finish time of a deadline thread is its start time + its period.  We
                // want the threads to wake in the order that they were setup (T0 wakes
                // first, then T1, then T2 and so on).
                //
                // We have already made certain that the periods of these threads are
                // strictly monotonically increasing, so now we just need to ensure that
                // the start times are monotonically increasing and we should be good to
                // go.
                //
                // To accomplish this, each thread has an optional "start time gate" which
                // can be assigned.  The first thread can start whenever it wants to, but
                // the thread T(x) needs to make sure that its start time is >= the start
                // time of T(x-1).
                //
                // So, we set T(x).start_time_gate = T(x - 1).start_time for x > 0.  These
                // threads, when released from their initial spin phase of the test, will
                // make certain that their start time is >= their start time gate (when
                // they have one) by sleeping until their finish time until they hit the
                // point that the condition is satisfied.
                //
                if setup_for_requeue_test && let Some(prev) = previous_thread {
                    blocking_thread.set_start_time_gate(prev.start_time());
                }
                previous_thread = Some(blocking_thread);
                blocking_thread.set_owner_locked(action.owner_locked);

                // Then perform the block operation.
                assert_true!(blocking_thread.do_block(target_queue, new_owner));

                // Now validate everything.
                let expected_owner = new_owner
                    .filter(|_| action.owner_expected)
                    .map_or(core::ptr::null_mut(), TestThread::thread);
                expect_eq!(expected_owner, target_queue.owner());
                for t in threads {
                    expect_eq!(t.target_queue(), t.blocking_queue());
                }
            }

            if setup_for_requeue_test && all_ok {
                shutdown_on_exit.cancel();
            }

            all_ok
        }

        fn run_requeue_test<
            const THREAD_COUNT: usize,
            const QUEUE_COUNT: usize,
            const ACTION_COUNT: usize,
        >(
            threads: &[TestThread; THREAD_COUNT],
            queues: &mut [TestQueue; QUEUE_COUNT],
            setup_actions: &[Action; ACTION_COUNT],
            requeue_action: &RequeueAction,
            expected_blocking_queues: &[u32; THREAD_COUNT],
            expected_owners: &[u32; QUEUE_COUNT],
        ) -> bool {
            begin_test!(all_ok);
            let _shutdown_on_exit = zr::defer(|| Self::shutdown_threads(threads));

            let get_thread = |index: u32| -> *mut Thread {
                if index == INVALID {
                    return core::ptr::null_mut();
                }
                debug_assert!((index as usize) < threads.len());
                threads[index as usize].thread()
            };

            let get_queue =
                |queues: &[TestQueue; QUEUE_COUNT], index: u32| -> *const OwnedWaitQueue {
                    if index == INVALID {
                        return core::ptr::null();
                    }
                    debug_assert!((index as usize) < queues.len());
                    queues[index as usize].wait_queue()
                };

            const {
                assert!(QUEUE_COUNT >= 2, "Requeue tests must always involve at least two queues");
            }

            // Setup the pre-requeue state.
            assert_true!(Self::run_baao_test(threads, queues, setup_actions, true));

            // Now perform the requeue action
            let new_requeue_owner = get_thread(requeue_action.requeue_owner_index);
            let mut wake_hooks = OwnedWaitQueue::default_wake_hooks();
            let mut requeue_hooks = OwnedWaitQueue::default_wake_hooks();
            let src_queue = queues[0].wait_queue_ref();
            let dst_queue = queues[1].wait_queue_ref();
            // SAFETY: `new_requeue_owner` is either null or a live `Thread` in `threads`.
            let new_requeue_owner_ref = unsafe { new_requeue_owner.as_ref() };
            src_queue.wake_and_requeue(
                dst_queue,
                new_requeue_owner_ref,
                requeue_action.wake_count,
                requeue_action.requeue_count,
                &mut wake_hooks,
                &mut requeue_hooks,
                if requeue_action.assign_woken {
                    WakeOption::AssignOwner
                } else {
                    WakeOption::None
                },
            );

            // Now verify that all of the threads are blocked (or not) in the proper
            // queues, and that all queues are owned by the expected threads (or no
            // thread).
            for (thread_index, &queue_index) in expected_blocking_queues.iter().enumerate() {
                let expected_queue = get_queue(queues, queue_index);
                expect_eq!(expected_queue, threads[thread_index].blocking_queue());
            }

            for (queue_index, &thread_index) in expected_owners.iter().enumerate() {
                let expected = get_thread(thread_index);
                expect_eq!(expected, queues[queue_index].owner());
            }

            all_ok
        }
    }

    impl TestThread {
        fn main(&self) -> i32 {
            while !self.do_baao.load(Ordering::SeqCst) {
                let _ = thread::sleep_relative(DurationMono::from_micros(100));
                if self.exit_now.load(Ordering::SeqCst) {
                    return -1;
                }
            }

            // If we have a "start time gate", then we need make sure that our start time
            // >= to our gate time in order to ensure a deterministic wake order when
            // running requeue tests.  This is a pretty simple operation; while our start
            // time is not large enough, we can just sleep until our finish time (getting
            // a new start time in the process).
            //
            // Note that we know that it is safe to observe the start_time_gate value
            // (from a memory order perspective) as it was stored before the CST store to
            // do_baao performed by the test setup thread.
            if self.has_start_time_gate.load(Ordering::Relaxed) {
                let gate = self.start_time_gate.load(Ordering::Relaxed);
                while self.start_time() < gate {
                    let _ = thread::sleep(InstantMono(self.finish_time()));
                }
            }

            // `Relaxed` loads below are synchronized by the `SeqCst` store/load on `do_baao`
            // above; `do_block` and the test setup thread write `owner_locked`, `target_queue`,
            // and `target_owner` before storing `true` to `do_baao`.
            if !self.owner_locked.load(Ordering::Relaxed) {
                let aapd = AnnotatedAutoPreemptDisabler::new();
                let target_queue_ptr = self.target_queue.load(Ordering::Relaxed);
                assert!(!target_queue_ptr.is_null());
                // SAFETY: `target_queue_ptr` is non-null and remains alive for the duration of the
                // test.
                let target_queue = unsafe { &*target_queue_ptr };
                // SAFETY: `target_owner` is either null or a valid `Thread` that remains alive.
                let target_owner = unsafe { self.target_owner.load(Ordering::Relaxed).as_ref() };
                // SAFETY: No transaction is active, the current thread is in a valid state to
                // block, and `target_queue` and `target_owner` remain alive while blocked.
                let _ = unsafe {
                    target_queue.block_and_assign_owner(
                        &const { Deadline::infinite() },
                        target_owner,
                        ResourceOwnership::Normal,
                        Interruptible::Yes,
                        aapd.token(),
                    )
                };
            } else {
                let target_queue_ptr = self.target_queue.load(Ordering::Relaxed);
                let target_owner_ptr = self.target_owner.load(Ordering::Relaxed);
                assert!(!target_queue_ptr.is_null());
                assert!(!target_owner_ptr.is_null());
                // SAFETY: `target_queue_ptr` is non-null and remains valid and alive for the
                // duration of the test.
                let target_queue = unsafe { &*target_queue_ptr };
                // SAFETY: `target_owner_ptr` is non-null and remains valid and alive for the
                // duration of the test.
                let target_owner = unsafe { &*target_owner_ptr };
                let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
                    // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for
                    // the duration of this transaction.
                    let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
                    {
                        // Validate we can change owner while the owner is locked.
                        let mut guard = OwnerGuard::new(target_owner, token);
                        let mut proof = guard.proof();
                        assert!(proof.state() != ThreadStateKind::Initial);
                        assert!(
                            proof.wait_queue_state().blocking_wait_queue(proof.token()).is_none()
                        );
                        // SAFETY: `target_queue`'s lock is not held, `target_owner` was just
                        // verified to be started with an empty downstream PI chain, and the
                        // transaction is not finalized.
                        if unsafe {
                            target_queue.assign_owner_locked(
                                &mut proof,
                                token,
                                &preempt_disabled_token,
                            )
                        }
                        .is_err()
                        {
                            return TransactionResult::BACKOFF;
                        }
                    }
                    let Some(mut queue_guard) = OwnerGuard::acquire_or_backoff(target_queue, token)
                    else {
                        return TransactionResult::BACKOFF;
                    };
                    // SAFETY: Called from a running kernel thread.
                    let current_thread = unsafe { &*thread::current_get() };
                    // SAFETY: Transaction is not finalized, `current_thread` is the running
                    // thread, and no downstream PI locks are held.
                    if let Some((details, mut current_thread_guard)) = unsafe {
                        queue_guard.proof().try_lock_for_baao_operation_locked(
                            current_thread,
                            Some(target_owner),
                            token,
                        )
                    } {
                        ChainLockTransaction::finalize(token);
                        // SAFETY: Transaction is finalized with the locks acquired by
                        // `try_lock_for_baao_operation_locked`, and `target_queue` remains
                        // alive.
                        let _ = unsafe {
                            OwnedWaitQueue::block_and_assign_owner_locked(
                                queue_guard,
                                &mut current_thread_guard.proof(),
                                &const { Deadline::infinite() },
                                &details,
                                ResourceOwnership::Normal,
                                Interruptible::Yes,
                                ForceInheritance::No,
                                token,
                                &preempt_disabled_token,
                            )
                        };
                        current_thread_guard.release();
                        return TransactionResult::DONE;
                    }
                    queue_guard.release();
                    TransactionResult::BACKOFF
                };
                ChainLockTransaction::until_done(
                    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
                    clt_tag!("OwnedWaitQueueTopologyTests::TestThread::Main (assign owner locked)"),
                    do_transaction,
                );
            }

            if self.exit_now.load(Ordering::SeqCst) {
                return -1;
            }

            let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
                // SAFETY: Called from a running kernel thread.
                let current_thread = unsafe { &*thread::current_get() };

                if !acquire_both_or_backoff(
                    // SAFETY: No `OwnerGuard` or `ChainLockProof` for `FINAL_EXIT_QUEUE` is
                    // live on this CPU at the start of the transaction.
                    unsafe { FINAL_EXIT_QUEUE.lock().as_chain_lock() },
                    // SAFETY: No `OwnerGuard` or `ChainLockProof` for `current_thread` is live
                    // on this CPU at the start of the transaction.
                    unsafe { current_thread.lock().as_chain_lock() },
                    token,
                ) {
                    return TransactionResult::BACKOFF;
                }

                // SAFETY: Both locks were just acquired by `acquire_both_or_backoff`.
                let (wq_guard, mut thread_guard) = unsafe {
                    (
                        OwnerGuard::adopt_unchecked(&*FINAL_EXIT_QUEUE, token),
                        OwnerGuard::adopt_unchecked(current_thread, token),
                    )
                };

                ChainLockTransaction::finalize(token);
                // SAFETY: Transaction is finalized with `FINAL_EXIT_QUEUE`'s and
                // `current_thread`'s locks held, and `FINAL_EXIT_QUEUE` is static.
                let _ = unsafe {
                    WaitQueue::block(
                        wq_guard,
                        &mut thread_guard.proof(),
                        &const { Deadline::infinite() },
                        Interruptible::Yes,
                        token,
                    )
                };
                thread_guard.release();
                TransactionResult::DONE
            };
            ChainLockTransaction::until_done(
                IRQ_SAVE_OPTION,
                clt_tag!("OwnedWaitQueueTopologyTests::TestThread::Main (final exit block)"),
                do_transaction,
            );

            0
        }

        fn do_block(&self, queue: &TestQueue, new_owner: Option<&TestThread>) -> bool {
            begin_test!(all_ok);

            assert_false!(self.do_baao.load(Ordering::SeqCst));
            // `Relaxed` is sufficient for `target_queue` and `target_owner`: all accesses here run
            // on the setup thread before the `SeqCst` store to `do_baao` below publishes them to
            // `main`.
            assert_null!(self.target_queue.load(Ordering::Relaxed));
            assert_null!(self.target_owner.load(Ordering::Relaxed));
            assert_nonnull!(queue.wait_queue());
            if let Some(new_owner) = new_owner {
                assert_nonnull!(new_owner.thread());
                self.target_owner.store(new_owner.thread(), Ordering::Relaxed);
            }
            self.target_queue.store(queue.wait_queue(), Ordering::Relaxed);
            self.do_baao.store(true, Ordering::SeqCst);

            loop {
                {
                    let thread_ptr = self.thread();
                    assert!(!thread_ptr.is_null());
                    // SAFETY: `thread_ptr` is non-null and remains valid until `shutdown`.
                    let thread_ref = unsafe { &*thread_ptr };
                    single_chain_lock_guard_owned!(
                        guard,
                        thread_ref,
                        IRQ_SAVE_OPTION,
                        clt_tag!("OwnedWaitQueueTopologyTests::TestThread::DoBlock")
                    );
                    if guard.proof().state() == ThreadStateKind::Blocked {
                        break;
                    }
                }
                let _ = thread::sleep_relative(DurationMono::from_micros(100));
            }

            all_ok
        }
    }

    /// simple
    #[test]
    fn owq_topology_test_simple() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // A basic smoke test.  Simply generate
        //
        // +----+     +----+
        // | T0 | --> | Q0 |
        // +----+     +----+
        //

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 1] = Default::default();
        const ACTIONS: [Action; 1] = [Action {
            thread_index: 0,
            queue_index: 0,
            owning_thread_index: INVALID,
            owner_expected: true,
            owner_locked: false,
        }];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// simple owner change
    #[test]
    fn owq_topology_test_simple_owner_change() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Change owners, moving from this
        //
        // +----+     +----+     +----+
        // | T0 | --> | Q0 | --> | T2 |
        // +----+     +----+     +----+
        //
        // to this:
        //
        // +----+     +----+     +----+
        // | T0 | --> | Q0 | --> | T3 |
        // +----+  ^  +----+     +----+
        //         |
        // +----+  |  +----+
        // | T1 | -+  | Q0 |
        // +----+     +----+

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const ACTIONS: [Action; 2] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// deny self own
    #[test]
    fn owq_topology_test_deny_self_own() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to do this
        //
        //  +----------------------+
        //  |                      |
        //  |  +----+     +----+   |
        //  +->| T0 | --> | Q0 | --+
        //     +----+     +----+
        //
        // Which should be denied, ending up with this.
        //
        //     +----+     +----+
        //     | T0 | --> | Q0 |
        //     +----+     +----+
        //

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 1] = Default::default();
        const ACTIONS: [Action; 1] = [Action {
            thread_index: 0,
            queue_index: 0,
            owning_thread_index: 0,
            owner_expected: false,
            owner_locked: false,
        }];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// owner becomes blocked; no new owner
    #[test]
    fn owq_topology_test_owner_becomes_blocked_no_new_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 |
        //     +----+     +----+     +----+
        //
        // To this, by having T1 block, and declare no new owner.
        //
        //     +----+     +----+
        //     | T0 | --> | Q0 |
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 2] = Default::default();
        const ACTIONS: [Action; 2] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// owner becomes blocked; yes new owner
    #[test]
    fn owq_topology_test_owner_becomes_blocked_yes_new_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 |
        //     +----+     +----+     +----+
        //
        // To this, by having T1 block, and declare T2 as the new owner.
        //
        //     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T2 |
        //     +----+  ^  +----+     +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //
        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const ACTIONS: [Action; 2] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// owner becomes blocked deny blocked new owner
    #[test]
    fn owq_topology_test_owner_becomes_blocked_deny_blocked_new_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 |
        //     +----+     +----+     +----+
        //
        // To this, by having T1 block, and declare T0 as the owner.
        //
        //  +----------------------+
        //  |                      |
        //  |  +----+     +----+   |
        //  +->| T0 | --> | Q0 | --+
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //
        // Because of the cycle, this should be denied, resulting in this:
        //
        //     +----+     +----+
        //     | T0 | --> | Q0 |
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 2] = Default::default();
        const ACTIONS: [Action; 2] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 0,
                owner_expected: false,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// blocked owners
    #[test]
    fn owq_topology_test_blocked_owners() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 | --> | Q1 |
        //     +----+     +----+     +----+     +----+
        //
        //                           +----+     +----+
        //                           | T2 | --> | Q2 |
        //                           +----+     +----+
        //
        // To this, by having T3 block, and declare T2 as the owner.
        //
        //                           +----+     +----+
        //                           | T1 | --> | Q1 |
        //                           +----+     +----+
        //
        //     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T2 | --> | Q2 |
        //     +----+  ^  +----+     +----+     +----+
        //             |
        //     +----+  |
        //     | T3 | -+
        //     +----+
        //

        let mut queues: [TestQueue; 3] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 1,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 2,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 0,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// blocking thread downstream of owner; no owner change
    #[test]
    fn owq_topology_test_blocking_thread_downstream_of_owner_no_owner_change() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 | --> | Q1 | --> | T2 |
        //     +----+     +----+     +----+     +----+     +----+
        //
        //
        // To this, by having T2 block in Q0, declaring T1 to still be the owner.
        //
        //     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 | --> | Q1 |
        //     +----+  ^  +----+     +----+     +----+
        //             |                          |
        //     +----+  |                          |
        //     | T2 | -+                          |
        //     +----+                             |
        //       ^                                |
        //       |                                |
        //       +--------------------------------+
        //
        // This should fail because of the cycle, and result in this instead.
        //
        //                          +----+     +----+
        //                          | T0 | --> | Q0 |
        //                          +----+  ^  +----+
        //                                  |
        //    +----+     +----+     +----+  |
        //    | T1 | --> | Q1 | --> | T2 | -+
        //    +----+     +----+     +----+
        //

        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 1,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: false,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// blocking thread downstream of owner; yes owner change
    #[test]
    fn owq_topology_test_blocking_thread_downstream_of_owner_yes_owner_change() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q1 | --> | T3 |
        //     +----+     +----+     +----+
        //
        // To this, by having T3 block in Q0, declaring T2 to be the new owner.
        //
        //     +----+
        //     | T1 |
        //     +----+
        //
        //     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T2 | --> | Q1 |
        //     +----+  ^  +----+     +----+     +----+
        //             |                          |
        //     +----+  |                          |
        //     | T3 | -+                          |
        //     +----+                             |
        //       ^                                |
        //       |                                |
        //       +--------------------------------+
        //
        // This should fail because of the cycle, and result in this instead.
        //     +----+
        //     | T1 |
        //     +----+
        //
        //     +----+     +----+
        //     | T0 | --> | Q0 |
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T3 | -+
        //     +----+
        //
        //     +----+     +----+
        //     | T2 | --> | Q1 |
        //     +----+     +----+
        //

        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 1,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 0,
                owning_thread_index: 2,
                owner_expected: false,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// intersecting owner chains
    #[test]
    fn owq_topology_test_intersecting_owner_chains() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+     +----+     +----+     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 | --> | Q1 | --> | T2 | --> | Q2 | --> | T3 |
        //     +----+     +----+     +----+     +----+     +----+  ^  +----+     +----+
        //                                                         |
        //                                                 +----+  |
        //                                                 | T4 | -+
        //                                                 +----+
        //
        // To this, by having T5 block in Q0, declaring T4 to be the new owner.
        //
        //     +----+     +----+     +----+
        //     | T1 | --> | Q1 | --> | T2 | -+
        //     +----+     +----+     +----+  |
        //                                   |
        //     +----+     +----+     +----+  V  +----+     +----+
        //     | T0 | --> | Q0 | --> | T4 | --> | Q2 | --> | T3 |
        //     +----+  ^  +----+     +----+     +----+     +----+
        //             |
        //     +----+  |
        //     | T5 | -+
        //     +----+
        //
        let mut queues: [TestQueue; 3] = Default::default();
        let threads: [TestThread; 6] = Default::default();
        const ACTIONS: [Action; 5] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 1,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 2,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 4,
                queue_index: 2,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 5,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// intersecting owner chains; old owner is target of new owner chain
    #[test]
    fn owq_topology_test_intersecting_owner_chains_old_owner_target_of_new_owner_chain() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //                           +----+     +----+     +----+
        //                           | T0 | --> | Q0 | --> | T3 |
        //                           +----+     +----+  ^  +----+
        //                                              |
        //     +----+     +----+     +----+     +----+  |
        //     | T1 | --> | Q1 | --> | T2 | --> | Q2 | -+
        //     +----+     +----+     +----+     +----+
        //
        // To this, by having T4 block in Q0, declaring T1 to be the new owner.
        //
        //     +----+
        //     | T4 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+     +----+     +----+     +----+     +----+
        //     | T0 | --> | Q0 | --> | T1 | --> | Q1 | --> | T2 | --> | Q2 | --> | T3 |
        //     +----+     +----+     +----+     +----+     +----+     +----+     +----+
        //
        //

        let mut queues: [TestQueue; 3] = Default::default();
        let threads: [TestThread; 5] = Default::default();
        const ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 1,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 2,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 4,
                queue_index: 0,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// basic requeue
    #[test]
    fn owq_topology_test_basic_requeue() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1
        //
        //     +----+
        //     | T0 |
        //     +----+
        //
        //     +----+     +----+
        //     | T2 | --> | Q0 |
        //     +----+     +----+
        //
        //     +----+     +----+
        //     | T1 | --> | Q1 |
        //     +----+     +----+
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: false,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 3] = [INVALID, 1, 0];
        const EXPECTED_OWNERS: [u32; 2] = [INVALID, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue assign woken owner
    #[test]
    fn owq_topology_test_requeue_assign_woken_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner in the process.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+
        //     | T1 | --> | Q1 |
        //     +----+     +----+
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 3] = [INVALID, 1, 0];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue assign both owners
    #[test]
    fn owq_topology_test_requeue_assign_both_owners() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |                  +----+
        //     | T2 | -+                  | T3 |
        //     +----+                     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and T3 as the Q1 owner in the process.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+
        //     | T1 | --> | Q1 | --> | T3 |
        //     +----+     +----+     +----+
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 3,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, INVALID];
        const EXPECTED_OWNERS: [u32; 2] = [0, 3];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue owner in wake queue
    #[test]
    fn owq_topology_test_requeue_owner_in_wake_queue() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and T2 as the new requeue owner.
        //
        //     +----+     +----+     +----+     +----+     +----+
        //     | T1 | --> | Q1 | --> | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+     +----+     +----+
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 2,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 3] = [INVALID, 1, 0];
        const EXPECTED_OWNERS: [u32; 2] = [0, 2];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue owner is new wake owner
    #[test]
    fn owq_topology_test_requeue_owner_is_new_wake_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and T0 as the new requeue owner.
        //
        //     +----+     +----+
        //     | T1 | --> | Q1 | -+
        //     +----+     +----+  |
        //                        |
        //     +----+     +----+  V  +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 0,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 3] = [INVALID, 1, 0];
        const EXPECTED_OWNERS: [u32; 2] = [0, 0];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue owner is in requeue target
    #[test]
    fn owq_topology_test_requeue_owner_is_in_requeue_target() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+          +----+
        //     | T1 | --> | Q0 |          | Q1 |
        //     +----+  ^  +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and T1 as the new requeue owner.
        //
        //          +----+     +----+
        //      +-> | T1 | --> | Q1 | -+
        //      |   +----+     +----+  |
        //      |                      |
        //      +----------------------+
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        // This will fail because of the cycle, and should result in:
        //
        //     +----+     +----+
        //     | T1 | --> | Q1 |
        //     +----+     +----+
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 3] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 1,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 3] = [INVALID, 1, 0];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake owner becomes requeue owner
    #[test]
    fn owq_topology_test_wake_owner_becomes_requeue_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+          +----+
        //     | T1 | --> | Q0 | --> | T3 |          | Q1 |
        //     +----+  ^  +----+     +----+          +----+
        //             |
        //     +----+  |
        //     | T2 | -+
        //     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and T3 as the new requeue owner.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+
        //     | T1 | --> | Q1 | --> | T3 |
        //     +----+     +----+     +----+
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 3] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 3,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, INVALID];
        const EXPECTED_OWNERS: [u32; 2] = [0, 3];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake and requeue have same owner keep rq owner
    #[test]
    fn owq_topology_test_wake_and_requeue_have_same_owner_keep_rq_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+
        //     | T1 | --> | Q0 | --> | T4 |
        //     +----+  ^  +----+  ^  +----+
        //             |          |
        //     +----+  |          |
        //     | T2 | -+          |
        //     +----+             |
        //                        |
        //     +----+     +----+  |
        //     | T3 | --> | Q1 | -+
        //     +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and keep T4 as the requeue owner.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+
        //     | T1 | --> | Q1 | --> | T4 |
        //     +----+  ^  +----+     +----+
        //             |
        //     +----+  |
        //     | T3 | -+
        //     +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 5] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 4,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 5] = [INVALID, 1, 0, 1, INVALID];
        const EXPECTED_OWNERS: [u32; 2] = [0, 4];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake and requeue have same owner lose rq owner
    #[test]
    fn owq_topology_test_wake_and_requeue_have_same_owner_lose_rq_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+
        //     | T1 | --> | Q0 | --> | T4 |
        //     +----+  ^  +----+  ^  +----+
        //             |          |
        //     +----+  |          |
        //     | T2 | -+          |
        //     +----+             |
        //                        |
        //     +----+     +----+  |
        //     | T3 | --> | Q1 | -+
        //     +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and no owner for Q1.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+
        //     | T1 | --> | Q1 |
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T3 | -+
        //     +----+
        //
        //     +----+
        //     | T4 |
        //     +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 5] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 5] = [INVALID, 1, 0, 1, INVALID];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue op intersecting owner chains no rq owner change
    #[test]
    fn owq_topology_test_requeue_op_intersecting_owner_chains_no_rq_owner_change() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+     +----+     +----+     +----+
        //     | T1 | --> | Q0 | --> | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+  ^  +----+     +----+     +----+  ^  +----+     +----+
        //             |                                |
        //     +----+  |                                |
        //     | T2 | -+                                |
        //     +----+                                   |
        //                                              |
        //                           +----+     +----+  |
        //                           | T3 | --> | Q1 | -+
        //                           +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and keeping T5 as the owner for Q1.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+     +----+
        //     | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+     +----+  ^  +----+     +----+
        //                        |
        //     +----+     +----+  |
        //     | T3 | --> | Q1 | -+
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //
        //
        let mut queues: [TestQueue; 4] = Default::default();
        let threads: [TestThread; 6] = Default::default();
        const SETUP_ACTIONS: [Action; 6] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 4,
                queue_index: 2,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 5,
                queue_index: 3,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 5,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 6] = [INVALID, 1, 0, 1, 2, 3];
        const EXPECTED_OWNERS: [u32; 4] = [0, 5, 5, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue op intersecting owner chains change rqo to none
    #[test]
    fn owq_topology_test_requeue_op_intersecting_owner_chains_change_rqo_to_none() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+     +----+     +----+     +----+
        //     | T1 | --> | Q0 | --> | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+  ^  +----+     +----+     +----+  ^  +----+     +----+
        //             |                                |
        //     +----+  |                                |
        //     | T2 | -+                                |
        //     +----+                                   |
        //                                              |
        //                           +----+     +----+  |
        //                           | T3 | --> | Q1 | -+
        //                           +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing no-owner for Q1.
        //
        //     +----+     +----+     +----+
        //     | T2 | --> | Q0 | --> | T0 |
        //     +----+     +----+     +----+
        //
        //     +----+     +----+     +----+     +----+
        //     | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+     +----+     +----+     +----+
        //
        //     +----+     +----+
        //     | T3 | --> | Q1 |
        //     +----+  ^  +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //
        //
        let mut queues: [TestQueue; 4] = Default::default();
        let threads: [TestThread; 6] = Default::default();
        const SETUP_ACTIONS: [Action; 6] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 4,
                queue_index: 2,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 5,
                queue_index: 3,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 6] = [INVALID, 1, 0, 1, 2, 3];
        const EXPECTED_OWNERS: [u32; 4] = [0, INVALID, 5, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue op intersecting owner chains change rqo to wq thread
    #[test]
    fn owq_topology_test_requeue_op_intersecting_owner_chains_change_rqo_to_wq_thread() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //     +----+
        //     | T0 | -+
        //     +----+  |
        //             |
        //     +----+  V  +----+     +----+     +----+     +----+     +----+
        //     | T1 | --> | Q0 | --> | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+  ^  +----+     +----+     +----+  ^  +----+     +----+
        //             |                                |
        //     +----+  |                                |
        //     | T2 | -+                                |
        //     +----+                                   |
        //                                              |
        //                           +----+     +----+  |
        //                           | T3 | --> | Q1 | -+
        //                           +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing T2 as the new owner of Q1.
        //
        //     +----+     +----+     +----+     +----+     +----+
        //     | T3 | --> | Q1 | --> | T2 | --> | Q0 | --> | T0 |
        //     +----+  ^  +----+     +----+     +----+     +----+
        //             |
        //     +----+  |
        //     | T1 | -+
        //     +----+
        //
        //     +----+     +----+     +----+     +----+
        //     | T4 | --> | Q2 | --> | T5 | --> | Q3 |
        //     +----+     +----+     +----+     +----+
        //
        let mut queues: [TestQueue; 4] = Default::default();
        let threads: [TestThread; 6] = Default::default();
        const SETUP_ACTIONS: [Action; 6] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 4,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 4,
                queue_index: 2,
                owning_thread_index: 5,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 5,
                queue_index: 3,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 2,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 6] = [INVALID, 1, 0, 1, 2, 3];
        const EXPECTED_OWNERS: [u32; 4] = [0, 2, 5, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue target upstream from woken thread
    #[test]
    fn owq_topology_test_requeue_target_upstream_from_woken_thread() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //    +----+     +----+     +----+
        //    | T3 | --> | Q1 | --> | T0 | -+
        //    +----+     +----+     +----+  |
        //                                  |
        //                          +----+  V  +----+
        //                          | T1 | --> | Q0 |
        //                          +----+  ^  +----+
        //                                  |
        //                          +----+  |
        //                          | T2 | -+
        //                          +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing no owner for Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //    +----+     +----+
        //    | T1 | --> | Q1 |
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 0,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue target upstream from requeue thread
    #[test]
    fn owq_topology_test_requeue_target_upstream_from_requeue_thread() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //                          +----+
        //                          | T0 | -+
        //                          +----+  |
        //                                  |
        //    +----+     +----+     +----+  V  +----+
        //    | T3 | --> | Q1 | --> | T1 | --> | Q0 |
        //    +----+     +----+     +----+  ^  +----+
        //                                  |
        //                          +----+  |
        //                          | T2 | -+
        //                          +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing no owner for Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //    +----+     +----+
        //    | T1 | --> | Q1 |
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue target upstream from still blocked thread
    #[test]
    fn owq_topology_test_requeue_target_upstream_from_still_blocked_thread() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //                          +----+
        //                          | T0 | -+
        //                          +----+  |
        //                                  |
        //                          +----+  V  +----+
        //                          | T1 | --> | Q0 |
        //                          +----+  ^  +----+
        //                                  |
        //    +----+     +----+     +----+  |
        //    | T3 | --> | Q1 | --> | T2 | -+
        //    +----+     +----+     +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing no owner for Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //    +----+     +----+
        //    | T1 | --> | Q1 |
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// requeue target upstream from requeue thread stays upstream
    #[test]
    fn owq_topology_test_requeue_target_upstream_from_requeue_thread_stays_upstream() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //                          +----+
        //                          | T0 | -+
        //                          +----+  |
        //                                  |
        //    +----+     +----+     +----+  V  +----+
        //    | T3 | --> | Q1 | --> | T1 | --> | Q0 |
        //    +----+     +----+     +----+  ^  +----+
        //                                  |
        //                          +----+  |
        //                          | T2 | -+
        //                          +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing T2 as the new owner for Q1.
        //
        //    +----+     +----+     +----+     +----+     +----+
        //    | T1 | --> | Q1 | --> | T2 | --> | Q0 | --> | T0 |
        //    +----+  ^  +----+     +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: 1,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 2,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, 2];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake queue upstream from requeue target
    #[test]
    fn owq_topology_test_wake_queue_upstream_from_requeue_target() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //    +----+
        //    | T0 | -+
        //    +----+  |
        //            |
        //    +----+  V  +----+     +----+     +----+
        //    | T1 | --> | Q0 | --> | T3 | --> | Q1 |
        //    +----+  ^  +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T2 | -+
        //    +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing no owner for Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //    +----+     +----+
        //    | T1 | --> | Q1 |
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: INVALID,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake queue upstream from requeue target swaps position
    #[test]
    fn owq_topology_test_wake_queue_upstream_from_requeue_target_swaps_position() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //    +----+
        //    | T0 | -+
        //    +----+  |
        //            |
        //    +----+  V  +----+     +----+     +----+
        //    | T1 | --> | Q0 | --> | T3 | --> | Q1 |
        //    +----+  ^  +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T2 | -+
        //    +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing T2 for the new owner of Q1.
        //
        //    +----+     +----+     +----+     +----+     +----+
        //    | T1 | --> | Q1 | --> | T2 | --> | Q0 | --> | T0 |
        //    +----+  ^  +----+     +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 2,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, 2];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake queue upstream from requeue target shares new wq owner
    #[test]
    fn owq_topology_test_wake_queue_upstream_from_requeue_target_shares_new_wq_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //    +----+
        //    | T0 | -+
        //    +----+  |
        //            |
        //    +----+  V  +----+     +----+     +----+
        //    | T1 | --> | Q0 | --> | T3 | --> | Q1 |
        //    +----+  ^  +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T2 | -+
        //    +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing T0 for the new owner of Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+  ^  +----+
        //                       |
        //    +----+     +----+  |
        //    | T1 | --> | Q1 | -+
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 0,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, 0];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// wake queue upstream from requeue target rejects new rq thread
    #[test]
    fn owq_topology_test_wake_queue_upstream_from_requeue_target_rejects_new_rq_thread() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // Try to go from this:
        //
        //    +----+
        //    | T0 | -+
        //    +----+  |
        //            |
        //    +----+  V  +----+     +----+     +----+
        //    | T1 | --> | Q0 | --> | T3 | --> | Q1 |
        //    +----+  ^  +----+     +----+     +----+
        //            |
        //    +----+  |
        //    | T2 | -+
        //    +----+
        //
        // To this, by doing a wake-1, requeue-1 from Q0 to Q1, assigning the woken
        // thread as the Q0 owner, and choosing T1 for the new owner of Q1.
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //     +----------------------+
        //     |                      |
        //     V   +----+     +----+  |
        //     +-> | T1 | --> | Q1 |--+
        //         +----+  ^  +----+
        //                 |
        //         +----+  |
        //         | T3 | -+
        //         +----+
        //
        // This cycle should be rejected, resulting in the following:
        //
        //    +----+     +----+     +----+
        //    | T2 | --> | Q0 | --> | T0 |
        //    +----+     +----+     +----+
        //
        //    +----+     +----+
        //    | T1 | --> | Q1 |
        //    +----+  ^  +----+
        //            |
        //    +----+  |
        //    | T3 | -+
        //    +----+
        //
        //
        let mut queues: [TestQueue; 2] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const SETUP_ACTIONS: [Action; 4] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 2,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 3,
                queue_index: 1,
                owning_thread_index: INVALID,
                owner_expected: true,
                owner_locked: false,
            },
        ];
        const REQUEUE_ACTION: RequeueAction = RequeueAction {
            wake_count: 1,
            requeue_count: 1,
            requeue_owner_index: 1,
            assign_woken: true,
        };
        const EXPECTED_BLOCKING_QUEUES: [u32; 4] = [INVALID, 1, 0, 1];
        const EXPECTED_OWNERS: [u32; 2] = [0, INVALID];

        assert_true!(OwnedWaitQueueTopologyTests::run_requeue_test(
            &threads,
            &mut queues,
            &SETUP_ACTIONS,
            &REQUEUE_ACTION,
            &EXPECTED_BLOCKING_QUEUES,
            &EXPECTED_OWNERS,
        ));
    }

    /// new owner is already locked
    #[test]
    fn owq_topology_test_locked_owner() {
        // TODO(https://fxbug.dev/378976169): Investigate hang on single-cpu builder.
        if max_num_cpus() == 1 {
            kprintln!("Skipping test that requires more than one cpu.");
            return true;
        }

        // A basic test that a owner that is already locked can take ownership of a queue
        //
        // +----+     +----+     +----+
        // | T0 | --> | Q0 | --> | T2 |
        // +----+     +----+     +----+
        //
        // to this:
        //
        // +----+     +----+     +----+
        // | T0 | --> | Q0 | --> | T3 |
        // +----+  ^  +----+     +----+
        //         |
        // +----+  |  +----+
        // | T1 | -+  | Q0 |
        // +----+     +----+

        let mut queues: [TestQueue; 1] = Default::default();
        let threads: [TestThread; 4] = Default::default();
        const ACTIONS: [Action; 2] = [
            Action {
                thread_index: 0,
                queue_index: 0,
                owning_thread_index: 2,
                owner_expected: true,
                owner_locked: false,
            },
            Action {
                thread_index: 1,
                queue_index: 0,
                owning_thread_index: 3,
                owner_expected: true,
                owner_locked: true,
            },
        ];

        assert_true!(OwnedWaitQueueTopologyTests::run_baao_test(
            &threads,
            &mut queues,
            &ACTIONS,
            false
        ));
    }

    /// bug 502179440 regression test
    #[test]
    fn owq_topology_bug_502179440_regression_test() {
        // We need CPUs 0 and 1 online to pin the main and contesting threads to
        // distinct CPUs below.  Skip the test if either CPU is offline.
        if (get_online_mask() & 0x3) != 0x3 {
            kprintln!("Need CPUs 0 and 1 online to run test. Skipping.");
            return true;
        }

        // Force the test thread to run on CPU 0 for the duration of the test.  Be
        // sure to restore the affinity after the test.
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` is the valid running thread.
        let old_affinity = unsafe { current.get_cpu_affinity() };
        // SAFETY: `current` is the valid running thread.
        unsafe { current.set_cpu_affinity(0x1) };
        let _restore_affinity = zr::defer(|| {
            // SAFETY: `current` is the valid running thread.
            unsafe { current.set_cpu_affinity(old_affinity) };
        });

        stack_pin_init!(let q0 = OwnedWaitQueue::init());
        stack_pin_init!(let q1 = OwnedWaitQueue::init());
        stack_pin_init!(let quit_event = Event::init_unsignaled());

        const QUEUE_COUNT: usize = 2;
        const THREAD_COUNT: usize = 2;

        // `threads` and `contesting_thread` use `AtomicPtr` with `Ordering::SeqCst` so `Args` can
        // be shared via `&Args` across the setup thread, `contesting_entry` (which reads
        // `threads[0]`), and `cleanup_threads` (which atomically swaps and joins spawned threads).
        struct Args<'a> {
            queues: [&'a OwnedWaitQueue; QUEUE_COUNT],
            threads: [AtomicPtr<Thread>; THREAD_COUNT],
            contesting_thread: AtomicPtr<Thread>,
            ready_threads: AtomicUsize,
            quit_event: &'a Event,
        }

        let args = Args {
            queues: [&q0, &q1],
            threads: [AtomicPtr::new(core::ptr::null_mut()), AtomicPtr::new(core::ptr::null_mut())],
            contesting_thread: AtomicPtr::new(core::ptr::null_mut()),
            ready_threads: AtomicUsize::new(0),
            quit_event: &quit_event,
        };

        let cleanup_threads = zr::defer(|| {
            args.quit_event.signal();
            for t in &args.threads {
                let raw = t.swap(core::ptr::null_mut(), Ordering::SeqCst);
                // SAFETY: Any non-null pointer stored in `args.threads` is a resumed, unjoined
                // thread; `swap` ensures it is joined at most once.
                if let Some(thread_ptr) = unsafe { ThreadPtr::from_raw(raw) } {
                    // SAFETY: `thread_ptr` is valid and unjoined.
                    let _unused_retcode = unsafe { thread_ptr.join(InstantMono::INFINITE) };
                }
            }
            let raw = args.contesting_thread.swap(core::ptr::null_mut(), Ordering::SeqCst);
            // SAFETY: Any non-null pointer stored in `args.contesting_thread` is a resumed,
            // unjoined thread; `swap` ensures it is joined at most once.
            if let Some(thread_ptr) = unsafe { ThreadPtr::from_raw(raw) } {
                // SAFETY: `thread_ptr` is valid and unjoined.
                let _unused_retcode = unsafe { thread_ptr.join(InstantMono::INFINITE) };
            }
        });

        extern "C" fn worker_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a valid `Args` that outlives this thread.
            let args = unsafe { &*arg.cast::<Args<'_>>() };
            args.ready_threads.fetch_add(1, Ordering::SeqCst);
            let _ = args.quit_event.wait_infinite();
            0
        }

        // Create two threads and wait for them to start.
        for t in &args.threads {
            // SAFETY: `worker_entry` and `&args` are valid, and `cleanup_threads` joins all
            // spawned threads before `args` goes out of scope.
            let created = unsafe {
                thread::create_with_priority(
                    c"owq_test_thread".as_ptr(),
                    worker_entry,
                    core::ptr::from_ref(&args).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            };
            assert_true!(created.is_ok());
            let thread_ptr = created.unwrap();
            t.store(thread_ptr.as_raw(), Ordering::SeqCst);
            // SAFETY: `thread_ptr` was just created and has not been resumed or joined yet.
            unsafe { thread_ptr.resume() };
        }

        while args.ready_threads.load(Ordering::SeqCst) != args.threads.len() {
            let _ = thread::sleep_relative(DurationMono::from_millis(1));
        }

        // Assign thread N as the owner of queue N, forall N.
        const {
            assert!(QUEUE_COUNT == THREAD_COUNT);
        }
        for i in 0..args.threads.len() {
            let thread_ptr = args.threads[i].load(Ordering::SeqCst);
            assert!(!thread_ptr.is_null());
            // SAFETY: `thread_ptr` is non-null and alive until joined by `cleanup_threads`.
            let thread_ref = unsafe { &*thread_ptr };
            expect_ok!(args.queues[i].assign_owner(Some(thread_ref)));
        }

        extern "C" fn contesting_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a valid `Args` that outlives this thread.
            let args = unsafe { &*arg.cast::<Args<'_>>() };
            {
                let t0_ptr = args.threads[0].load(Ordering::SeqCst);
                assert!(!t0_ptr.is_null());
                // SAFETY: `t0_ptr` is non-null and alive until joined by `cleanup_threads`.
                let t0 = unsafe { &*t0_ptr };
                single_chain_lock_guard_owned!(
                    _guard,
                    t0,
                    IRQ_SAVE_OPTION,
                    clt_tag!("owq_topology_bug_502179440_regression_test")
                );
                args.ready_threads.fetch_add(1, Ordering::SeqCst);

                let deadline = current_mono_time() + DurationMono::from_millis(200);
                while current_mono_time() < deadline {
                    libarch::r#yield();
                }
            }
            let _ = args.quit_event.wait_infinite();
            0
        }

        // Start another thread and have it hold T0's lock.  We need to be told to
        // Backoff on locking when we attempt to obtain the wake_futex's OWQ's current
        // owner lock to set up the original conditions of the bug.
        //
        // Sadly, it is very difficult to make this fully deterministic.  Once the
        // requeue operation starts, it will continue to try until it finally
        // completes.  We need to have T0's lock contested, but we also need to drop
        // the lock after a certain period of time, or we will be deadlocked.
        //
        // Right now, we are hardcoding a timeout of 200mSec for holding the lock.
        // There is a small chance of a false negative if the lock is dropped before
        // the requeue operation starts, but automated tests should not be able to
        // miss a regression forever.
        //
        // Also, use hard affinity make sure that the main test thread is not running
        // on the same CPU as the contesting thread.  If the contesting thread makes
        // it into the lock, disabling interrupts in the process, and the primary test
        // thread ends up being scheduled on the same CPU as the contesting thread, it
        // is possible that it will end up stuck there if there is so little activity
        // in the system that the main thread does not get stolen by another scheduler
        // when an unrelated thread goes idle.
        //
        // SAFETY: `contesting_entry` and `&args` are valid, and `cleanup_threads` joins
        // `contesting_thread` before `args` goes out of scope.
        let contesting_created = unsafe {
            thread::create_with_priority(
                c"owq_test_thread".as_ptr(),
                contesting_entry,
                core::ptr::from_ref(&args).cast_mut().cast(),
                DEFAULT_PRIORITY,
            )
        };
        assert_true!(contesting_created.is_ok());
        let contesting_ptr = contesting_created.unwrap();
        args.contesting_thread.store(contesting_ptr.as_raw(), Ordering::SeqCst);
        // SAFETY: `contesting_ptr` was just created and has not been resumed or joined yet.
        unsafe {
            contesting_ptr.set_cpu_affinity(0x2);
            contesting_ptr.resume();
        }

        while args.ready_threads.load(Ordering::SeqCst) != args.threads.len() + 1 {
            let _ = thread::sleep_relative(DurationMono::from_millis(1));
        }

        // Requeue from Q0 -> Q1, specifying T1 (who owns Q1 right now) as the new
        // owner of Q1.
        let t1_ptr = args.threads[1].load(Ordering::SeqCst);
        assert!(!t1_ptr.is_null());
        // SAFETY: `t1_ptr` is non-null and alive until joined by `cleanup_threads`.
        let t1_ref = unsafe { &*t1_ptr };
        args.queues[0].wake_and_requeue(
            args.queues[1],
            Some(t1_ref),
            1,
            u32::MAX,
            &mut DefaultWakeHooks,
            &mut DefaultWakeHooks,
            WakeOption::None,
        );

        // If we didn't fail any asserts, then we passed!  Shutdown the threads and we are done.
        drop(cleanup_threads);
    }
}
