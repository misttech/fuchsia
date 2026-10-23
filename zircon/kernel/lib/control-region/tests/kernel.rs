// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// control-region tests
#[cfg(ktest)]
#[unittest::suite(name = "control-region")]
mod tests {
    use crate::arch_rs::InterruptDisableGuard;
    use crate::control_region::ControlRegion;
    use crate::kernel::thread::{self, ThreadPtr};
    use crate::platform_rs::timer::{DurationMono, InstantMono};
    use core::ffi::c_void;
    use core::ptr::from_ref;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use kprint::kprintln;
    use pin_init::{PinInit, pin_data, pin_init, stack_pin_init};
    use unittest::{assert_ok, expect_true};

    /// A basic stress test for ControlRegion.
    #[test]
    fn stress() {
        // Spawns several threads, then has them all repeatedly enter and leave the control region.

        const NUM_THREADS: usize = 4;

        #[pin_data]
        struct TestState {
            /// Used to implement wait_for_all.  When a thread "arrives" (i.e. calls wait_for_all),
            /// it reads the generation value, then increments the count.  If the caller is not the
            /// last to arrive, it will spin until the generation value is bumped.  If the caller
            /// is the last to arrive (i.e. has incremented the count to NUM_THREADS), it will reset
            /// the count to zero and bump the generation, thereby releasing the other threads.
            barrier_generation: AtomicUsize,
            barrier_count: AtomicUsize,

            /// Used to signal that the tests is ending and the threads should terminate.
            done: AtomicBool,

            /// The system under test.
            #[pin]
            region: ControlRegion,
        }

        impl TestState {
            fn init() -> impl PinInit<Self> {
                pin_init!(Self {
                    barrier_generation: AtomicUsize::new(0),
                    barrier_count: AtomicUsize::new(0),
                    done: AtomicBool::new(false),
                    region <- ControlRegion::init(),
                })
            }

            extern "C" fn thread_entry(arg: *mut c_void) -> i32 {
                // SAFETY: `arg` points to the `TestState` pinned on the test's stack; every thread
                // that receives it is joined before `stress` returns.
                let state = unsafe { &*arg.cast::<TestState>() };
                state.run();
                0
            }

            fn stop(&self) {
                self.done.store(true, Ordering::Release);
            }

            /// Each thread executes this function once.
            fn run(&self) {
                // Only used for diagnostic output.
                let mut count_entered: usize = 0;
                let mut count_closed: usize = 0;

                self.region.register();
                self.wait_for_all();

                while self.keep_running() {
                    // Enter the region.
                    {
                        let _irqd = InterruptDisableGuard::new();
                        if !self.region.try_enter() {
                            self.region.close_enter();
                            count_closed += 1;
                        }
                    }

                    count_entered += 1;

                    // Leave the region.
                    {
                        let _irqd = InterruptDisableGuard::new();
                        if !self.region.try_leave() {
                            self.region.open_leave();
                        }
                    }
                }

                // Note, we don't call unregister here because we cannot guarantee that there are no
                // other threads within the control region.

                kprintln!("counts: entered={}, closed={}", count_entered, count_closed);
            }

            /// Spins until all other threads reach this point or the test is stopped.
            ///
            /// This method acts as a reusable synchronization barrier.
            fn wait_for_all(&self) {
                let orig_gen = self.barrier_generation.load(Ordering::Acquire);
                let num_waiting = self.barrier_count.fetch_add(1, Ordering::AcqRel);
                if num_waiting + 1 == NUM_THREADS {
                    self.barrier_count.store(0, Ordering::Release);
                    self.barrier_generation.fetch_add(1, Ordering::AcqRel);
                    return;
                }
                while orig_gen == self.barrier_generation.load(Ordering::Acquire)
                    && self.keep_running()
                {
                    libarch::r#yield();
                }
            }

            fn keep_running(&self) -> bool {
                !self.done.load(Ordering::Acquire)
            }
        }

        stack_pin_init!(let state = TestState::init());

        // Create a bunch of threads.
        let mut threads: [Option<ThreadPtr>; NUM_THREADS] = [None; NUM_THREADS];
        let mut create_result = Ok(());
        for thread in threads.iter_mut() {
            let arg = from_ref(&*state).cast_mut().cast::<c_void>();
            // SAFETY: `thread_entry` and `arg` are safe to run on a new thread; every thread that
            // receives `arg` is joined before `stress` returns.
            match unsafe { thread::create(c"cr-stress".as_ptr(), TestState::thread_entry, arg) } {
                Ok(t) => {
                    // SAFETY: `t` was just created and has not been resumed or joined yet.
                    unsafe { t.resume() };
                    *thread = Some(t);
                }
                Err(status) => {
                    create_result = Err(status);
                    break;
                }
            }
        }
        if create_result.is_err() {
            // Stop and join the threads created so far so that none of them outlives `state`.
            state.stop();
            for thread in threads.into_iter().flatten() {
                // SAFETY: Each thread in `threads` was created above and has not been joined yet.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }
        }
        assert_ok!(create_result);

        // Let them run for a bit before asking them to stop.
        let _ = thread::sleep_relative(DurationMono::from_seconds(1));
        state.stop();

        // Join them.
        for thread in threads {
            // SAFETY: `thread` is a valid thread that has not yet been joined.
            let result = unsafe { thread.unwrap().join(InstantMono::INFINITE) };
            expect_true!(result == Ok(0));
        }
    }
}
