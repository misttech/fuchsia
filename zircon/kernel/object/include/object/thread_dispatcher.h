// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_DISPATCHER_H_

#include <arch.h>
#include <lib/object-constants.h>
#include <platform.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <arch/exception.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/owned_wait_queue.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>
#include <ktl/string_view.h>
#include <object/channel_dispatcher.h>
#include <object/dispatcher.h>
#include <object/exceptionate.h>
#include <object/futex_context.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <vm/vm_address_region.h>

class ExceptionDispatcher;
class ProcessDispatcher;
class ThreadDispatcher;

// These are declared ahead of the class because ThreadDispatcher befriends
// cpp_thread_dispatcher_alloc, and a friend declaration requires the declaration it names to
// already be visible.
extern "C" {
void* rust_thread_dispatcher_get_exceptionate(const ThreadDispatcher* thread);
bool rust_thread_dispatcher_suspended_or_in_exception_locked(const ThreadDispatcher* thread);
bool rust_thread_dispatcher_clear_exception_locked(const ThreadDispatcher* thread);
void rust_thread_dispatcher_set_exception_locked(const ThreadDispatcher* thread,
                                                 const ExceptionDispatcher* exception,
                                                 uint32_t exceptionate_type);
ExceptionDispatcher* rust_thread_dispatcher_get_exception_locked(const ThreadDispatcher* thread);
void* rust_thread_dispatcher_get_core_thread_lock(const ThreadDispatcher* thread);

// Allocates and constructs a ThreadDispatcher. The Rust ThreadDispatcher::create() calls this
// and then attaches a lower-level kernel Thread to the result.
zx_status_t cpp_thread_dispatcher_alloc(
    ProcessDispatcher* process, uint32_t flags,
    ffi::Uninitialized<KernelHandle<ThreadDispatcher>>* out_handle);
void cpp_thread_dispatcher_set_core_thread(ThreadDispatcher* thread, Thread* core_thread);
Thread* cpp_thread_dispatcher_get_core_thread(const ThreadDispatcher* thread);
void cpp_thread_dispatcher_set_user_entry(ThreadDispatcher* thread, const UserEntryState* entry);
void cpp_thread_dispatcher_attach_to_core_thread(ThreadDispatcher* thread, Thread* core_thread);
int cpp_thread_start_routine_entry(void* thread);

// Rust FFI prototypes:
zx_status_t rust_thread_dispatcher_create(
    ProcessDispatcher* process, uint32_t flags, const char* name_ptr, size_t name_len,
    ffi::Uninitialized<KernelHandle<ThreadDispatcher>>* out_handle,
    ffi::Uninitialized<zx_rights_t>* out_rights);
void rust_thread_dispatcher_state_init(void* state, const ThreadDispatcher* disp,
                                       ProcessDispatcher* process);
void rust_thread_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_thread_dispatcher_state_get_lock(const void* state);
zx_koid_t rust_thread_dispatcher_get_related_koid(const ThreadDispatcher* thread);
ProcessDispatcher* rust_thread_dispatcher_get_process(const ThreadDispatcher* thread);
void rust_thread_dispatcher_assert_canary(const ThreadDispatcher* thread);
bool rust_thread_dispatcher_is_dying_or_dead(const ThreadDispatcher* thread);
zx_status_t rust_thread_dispatcher_initialize(const ThreadDispatcher* thread);
zx_status_t rust_thread_dispatcher_start(const ThreadDispatcher* thread,
                                         const UserEntryState* entry, bool ensure_initial_thread);
zx_status_t rust_thread_dispatcher_make_runnable(const ThreadDispatcher* thread,
                                                 const UserEntryState* entry, bool suspended);
void rust_thread_dispatcher_kill(const ThreadDispatcher* thread);
zx_status_t rust_thread_dispatcher_suspend(const ThreadDispatcher* thread);
void rust_thread_dispatcher_resume(const ThreadDispatcher* thread);
void rust_thread_dispatcher_suspending(const ThreadDispatcher* thread);
void rust_thread_dispatcher_resuming(const ThreadDispatcher* thread);

void cpp_thread_handle_single_shot_exiting(ProcessDispatcher* process, ThreadDispatcher* thread);
void cpp_thread_dispatcher_reset_core_thread(ThreadDispatcher* thread);

void* rust_thread_dispatcher_get_runtime_stats(const ThreadDispatcher* thread);
void rust_thread_dispatcher_exiting_current(const ThreadDispatcher* thread);

uint32_t rust_thread_dispatcher_set_blocked_reason(uint32_t reason);
}

class ThreadDispatcher final : public Dispatcher,
                               public fbl::DoublyLinkedListable<ThreadDispatcher*> {
 public:
  // When in a blocking syscall, or blocked in an exception, the blocking reason.
  // There is one of these for each syscall marked "blocking".
  // See //zircon/vdso.
  enum class Blocked : uint32_t {
    // Not blocked.
    NONE,
    // The thread is blocked in an exception.
    EXCEPTION,
    // The thread is sleeping (zx_nanosleep).
    SLEEPING,
    // zx_futex_wait
    FUTEX,
    // zx_port_wait
    PORT,
    // zx_channel_call
    CHANNEL,
    // zx_object_wait_one
    WAIT_ONE,
    // zx_object_wait_many
    WAIT_MANY,
    // zx_interrupt_wait
    INTERRUPT,
    // pager
    PAGER,
  };

  using EntryState = UserEntryState;

  static zx_status_t Create(fbl::RefPtr<ProcessDispatcher> process, uint32_t flags,
                            ktl::string_view name, KernelHandle<ThreadDispatcher>* out_handle,
                            zx_rights_t* out_rights);
  ~ThreadDispatcher();

  static ThreadDispatcher* GetCurrent() { return Thread::Current::Get()->user_thread(); }

  // Terminates the current thread. Does not return.
  static void ExitCurrent() __NO_RETURN { Thread::Current::Exit(0); }
  // Marks the current thread for termination. The thread will actually termiante when
  // the kernel stack unwinds.
  static void KillCurrent() { Thread::Current::Kill(); }

  // Asserts that the Rust ThreadDispatcherState's canary is valid.
  void AssertCanary() const { rust_thread_dispatcher_assert_canary(this); }

  // Dispatcher implementation.
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_THREAD; }
  zx_koid_t get_related_koid() const final {
    AssertCanary();
    return rust_thread_dispatcher_get_related_koid(this);
  }

  static constexpr zx_rights_t default_rights() { return ZX_DEFAULT_THREAD_RIGHTS; }
  bool is_waitable() const final { return ZX_DEFAULT_THREAD_RIGHTS & ZX_RIGHT_WAIT; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }

  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Sets whether or not this is the initial thread in its process.
  // Should only be called by ProcessDispatcher upon adding the initialized thread.
  void set_is_initial_thread(bool is_initial_thread) { is_initial_thread_ = is_initial_thread; }

  // Performs initialization on a newly constructed ThreadDispatcher
  // If this fails, then the object is invalid and should be deleted
  zx_status_t Initialize() TA_EXCL(get_lock()) { return rust_thread_dispatcher_initialize(this); }
  // Start this thread running inside the parent process with the provided entry state, only
  // valid to be called on a thread in the INITIALIZED state that has not yet been started. If
  // `ensure_initial_thread` is true, the thread will only start if it is the first thread in the
  // process.
  zx_status_t Start(const EntryState& entry, bool ensure_initial_thread) {
    return rust_thread_dispatcher_start(this, &entry, ensure_initial_thread);
  }
  // Transitions a thread from the INITIALIZED state to either the RUNNING or SUSPENDED state.
  // Is the caller's responsibility to ensure this thread is registered with the parent process,
  // as such this is only expected to be called from the ProcessDispatcher.
  zx_status_t MakeRunnable(const EntryState& entry, bool suspended) {
    return rust_thread_dispatcher_make_runnable(this, &entry, suspended);
  }
  void Kill() { rust_thread_dispatcher_kill(this); }

  // Suspends the thread.
  // Returns ZX_OK on success, or ZX_ERR_BAD_STATE iff the thread is dying or dead.
  zx_status_t Suspend() { return rust_thread_dispatcher_suspend(this); }
  void Resume() { rust_thread_dispatcher_resume(this); }

  // accessors
  ProcessDispatcher* process() const { return rust_thread_dispatcher_get_process(this); }

  // Returns the lower level kernel thread backing this dispatcher, or nullptr once
  // ExitingCurrent() has cleared it.
  Thread* core_thread() const TA_REQ(get_lock()) { return core_thread_; }

  // Stores the register values userspace will start executing with. Only meaningful before the
  // thread has started, so this is not guarded.
  void set_user_entry(const EntryState& entry) { user_entry_ = entry; }
  // Returns true if the thread is dying or dead. Threads never return to a previous state
  // from dying/dead so once this is true it will never flip back to false.
  bool IsDyingOrDead() const TA_EXCL(get_lock()) {
    return rust_thread_dispatcher_is_dying_or_dead(this);
  }

  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final __NONNULL((2))
      TA_EXCL(get_lock());
  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final
      TA_EXCL(get_lock());

  // Assuming the thread is stopped waiting for an exception response,
  // fill in |*report| with the exception report.
  // Returns ZX_ERR_BAD_STATE if not in an exception.
  zx_status_t GetExceptionReport(zx_exception_report_t* report);

  Exceptionate* exceptionate() {
    AssertCanary();
    return static_cast<Exceptionate*>(rust_thread_dispatcher_get_exceptionate(this));
  }

  // Sends an exception over the exception channel and blocks for a response.
  //
  // |sent| will indicate whether the exception was successfully sent over
  // the given |exceptionate| channel. This can be used in the ZX_ERR_NEXT
  // case to determine whether the exception channel didn't exist or it did
  // exist but the receiver opted not to handle the exception.
  //
  // Returns:
  //   ZX_OK if the exception was processed and the thread should resume.
  //   ZX_ERR_NEXT if there is no channel or the receiver opted to skip.
  //   ZX_ERR_NO_MEMORY on allocation failure.
  //   ZX_ERR_INTERNAL_INTR_KILLED if the thread was killed before
  //       receiving a response.
  zx_status_t HandleException(Exceptionate* exceptionate,
                              fbl::RefPtr<ExceptionDispatcher> exception, bool* sent);

  // Similar to HandleException(), but for single-shot exceptions which are
  // sent to at most one handler, e.g. ZX_EXCP_THREAD_STARTING.
  //
  // The main difference is that this takes |exception_type| and |context|
  // rather than a full exception object, and internally sets up the required
  // state and creates the exception object.
  //
  // Returns true if the exception was sent.
  bool HandleSingleShotException(Exceptionate* exceptionate, zx_excp_type_t exception_type,
                                 const arch_exception_context_t& context) TA_EXCL(get_lock());

  // Fetch a consistent snapshot of the runtime stats, compensated for unaccumulated runtime in the
  // ready or running state.
  TaskRuntimeStats GetCompensatedTaskRuntimeStats() const {
    return runtime_stats().GetCompensatedTaskRuntimeStats();
  }
  zx_info_task_runtime_t GetRuntimeStats() const { return GetCompensatedTaskRuntimeStats(); }

  // For debugger usage.
  zx_status_t ReadState(zx_thread_state_topic_t state_kind, user_out_ptr<void> buffer,
                        size_t buffer_size) TA_EXCL(get_lock());
  zx_status_t WriteState(zx_thread_state_topic_t state_kind, user_in_ptr<const void> buffer,
                         size_t buffer_size) TA_EXCL(get_lock());

  // Blocking syscalls, once they commit to a path that will likely block the
  // thread, use this helper class to properly set/restore |blocked_reason_|.
  class AutoBlocked final {
   public:
    explicit AutoBlocked(Blocked reason)
        : prev_reason_(ThreadDispatcher::SetBlockedReason(reason)) {
      DEBUG_ASSERT(reason != Blocked::NONE);
    }
    ~AutoBlocked() { ThreadDispatcher::SetBlockedReason(prev_reason_); }

   private:
    const Blocked prev_reason_;
  };

  static Blocked SetBlockedReason(Blocked reason) {
    return static_cast<Blocked>(
        rust_thread_dispatcher_set_blocked_reason(static_cast<uint32_t>(reason)));
  }

  // This is called from Thread as it is exiting, just before it stops for good.
  // It is an error to call this on anything other than the current thread.
  void ExitingCurrent() { rust_thread_dispatcher_exiting_current(this); }

  // callback from kernel when thread is suspending
  void Suspending() { rust_thread_dispatcher_suspending(this); }
  // callback from kernel when thread is resuming
  void Resuming() { rust_thread_dispatcher_resuming(this); }

  // Update the runtime stats for this thread/process. This is called by
  // Scheduler to update the runtime stats of the thread as it changes thread
  // state.
  //
  // Must be called with interrupts disabled.
  void UpdateRuntimeStats(thread_state new_state) {
    DEBUG_ASSERT(arch_ints_disabled());
    runtime_stats().Update(new_state, ThreadRuntimeStats::NoIrqSave);
  }

  // Update time spent handling page faults. This is called by the VM during page fault handling.
  void AddPageFaultTicks(zx_duration_mono_ticks_t ticks) {
    runtime_stats().AddPageFaultTicks(ticks);
  }

  // Update time spent contended on locks. This is called by lock implementations.
  void AddLockContentionTicks(zx_duration_mono_ticks_t ticks) {
    runtime_stats().AddLockContentionTicks(ticks);
  }

  class CoreThreadObservation {
   public:
    CoreThreadObservation() = default;
    ~CoreThreadObservation() { Release(); }

    void Release() {
      if (core_thread_ != nullptr) {
        DEBUG_ASSERT(lock_ != nullptr);

        lock_->AssertHeld();
        [this]() TA_NO_THREAD_SAFETY_ANALYSIS { lock_->Release(); }();

        core_thread_ = nullptr;
        lock_ = nullptr;
      } else {
        DEBUG_ASSERT(lock_ == nullptr);
      }
    }

    Thread* core_thread() const { return core_thread_; }

    CoreThreadObservation(const CoreThreadObservation&) = delete;
    CoreThreadObservation& operator=(const CoreThreadObservation&) = delete;

    CoreThreadObservation(CoreThreadObservation&& other) { *this = ktl::move(other); }

    CoreThreadObservation& operator=(CoreThreadObservation&& other) {
      // We should only ever be moving into a CoreThreadObservation instance
      // which has been default constructed (eg, empty)
      DEBUG_ASSERT((core_thread_ == nullptr) && (lock_ == nullptr));

      core_thread_ = other.core_thread_;
      lock_ = other.lock_;
      other.core_thread_ = nullptr;
      other.lock_ = nullptr;

      return *this;
    }

   private:
    friend class ThreadDispatcher;

    CoreThreadObservation(Thread* core_thread, SpinLock* lock)
        : core_thread_(core_thread), lock_(lock) {
      if (lock_ != nullptr) {
        [this]() TA_NO_THREAD_SAFETY_ANALYSIS { lock_->Acquire(); }();
      }
    }

    Thread* core_thread_{nullptr};
    SpinLock* lock_{nullptr};
  };

  CoreThreadObservation ObserveCoreThread() TA_REQ(get_lock()) {
    SpinLock* lock =
        &static_cast<Lock<SpinLock>*>(rust_thread_dispatcher_get_core_thread_lock(this))->lock();
    return CoreThreadObservation{core_thread_, lock};
  }

  // WARNING: The members below, through `get_lock`, are public *only* so that the Rust port of
  // FutexContext (object/futex_context.rs) can reach them through FFI shims. In C++ they were
  // private and accessed exclusively by FutexContext via `friend class FutexContext`.
  //
  // It is *not* generally correct to use any of them outside of FutexContext. In particular,
  // holding the locks named in their thread-safety annotations (if any) is strictly insufficient
  // for safe usage: callers must also follow FutexContext's synchronization protocol (holding the
  // owning FutexContext's state lock, holding pending-operation references on the involved
  // threads, and obeying its chain-lock ordering). Do not add new callers.
  //
  // TODO(https://fxbug.dev/565800859): Find a better way to hide these from everything other than
  // FutexContext.

  // Accessors for `blocking_futex_id_`, the ID of the futex this thread is currently waiting on.
  // These are guarded by FutexContext's own locking protocol, which cannot be expressed with
  // static thread-safety annotations (see the comment on `blocking_futex_id_`).
  //
  // Only intended for use by FutexContext; see the WARNING above.
  FutexId blocking_futex_id() const { return blocking_futex_id_; }
  void set_blocking_futex_id(FutexId id) { blocking_futex_id_ = id; }

  // Exposes this thread's dispatcher lock so that FutexContext can acquire it (as a "null-able"
  // guard on a potential futex owner) through FFI shims. The lock itself is supplied by the Rust
  // ThreadDispatcherState's KMutex.
  //
  // Only intended for use by FutexContext; see the WARNING above.
  Lock<CriticalMutex>* get_lock() const final;

 private:
  // The constructor, StartRoutine, and set_core_thread stay private. They are only reachable
  // through these shims, and friendship does not follow the caller across the FFI boundary, so
  // the shims themselves are the friends rather than the Rust code that calls them.
  friend zx_status_t cpp_thread_dispatcher_alloc(
      ProcessDispatcher* process, uint32_t flags,
      ffi::Uninitialized<KernelHandle<ThreadDispatcher>>* out_handle);
  friend void cpp_thread_dispatcher_set_core_thread(ThreadDispatcher* thread, Thread* core_thread);
  friend int cpp_thread_start_routine_entry(void* thread);

  ThreadDispatcher(fbl::RefPtr<ProcessDispatcher> process, uint32_t flags);
  ThreadDispatcher(const ThreadDispatcher&) = delete;
  ThreadDispatcher& operator=(const ThreadDispatcher&) = delete;

  friend void cpp_thread_dispatcher_reset_core_thread(ThreadDispatcher* thread)
      TA_REQ(thread->get_lock());

  // OwnedWaitQueue is a friend only so that it can access the blocking_futex_id_ member for tracing
  // purposes.
  friend class OwnedWaitQueue;

  // kernel level entry point
  static int StartRoutine(void* arg);

  // Returns true if the thread is suspended or processing an exception.
  bool SuspendedOrInExceptionLocked() TA_REQ(get_lock()) {
    return rust_thread_dispatcher_suspended_or_in_exception_locked(this);
  }

  template <typename T, typename F>
  zx_status_t ReadStateGeneric(F get_state_func, user_out_ptr<void> buffer, size_t buffer_size)
      TA_EXCL(get_lock());
  template <typename T, typename F>
  zx_status_t WriteStateGeneric(F set_state_func, user_in_ptr<const void> buffer,
                                size_t buffer_size) TA_EXCL(get_lock());

  // Sets the lower level kernel thread backing this dispatcher.
  //
  // Thread safety analysis is disabled here because this runs before the dispatcher has been
  // published anywhere, so there is nothing to race with and no need to take the lock.
  void set_core_thread(Thread* core_thread) TA_NO_THREAD_SAFETY_ANALYSIS {
    core_thread_ = core_thread;
  }

  // TRANSITIONAL: the runtime stats are held as opaque storage in the Rust ThreadDispatcherState
  // while ThreadRuntimeStats remains a C++ type. This resolves that storage back to the object;
  // the Rust side checks the canary.
  ThreadRuntimeStats& runtime_stats() const {
    return *static_cast<ThreadRuntimeStats*>(rust_thread_dispatcher_get_runtime_stats(this));
  }

  // The thread as understood by the lower kernel. This is set to nullptr when
  // the thread's lifecycle transitions to DEAD.
  Thread* core_thread_ TA_GUARDED(get_lock()) = nullptr;

  // User thread starting register values.
  EntryState user_entry_;

  // If true and ancestor job has a debugger attached, thread will block on
  // start and will send a process start exception.
  bool is_initial_thread_ = false;

  // The ID of the futex we are currently waiting on, or 0 if we are not
  // waiting on any futex at the moment.
  //
  // TODO(johngro): figure out some way to apply clang static thread analysis
  // to this.  Right now, there is no good (cost free) way for the compiler to
  // figure out that this thread belongs to a specific process/futex-context,
  // and therefor the thread's futex-context lock can be used to guard this
  // futex ID.
  FutexId blocking_futex_id_{FutexId::Null()};

  // Stores the Rust ThreadDispatcherState instance. It currently holds the canary, parent
  // process reference, state machine, runtime stats, blocked reason, exceptionate, exception
  // dispatcher, exceptionate type, suspend count, channel waiter, core thread lock, and state
  // lock; the remaining fields above migrate into it as the methods that use them move to Rust.
  OpaqueStorage<kThreadDispatcherStateSize, kThreadDispatcherStateAlign> opaque_storage_;
};

extern "C" {
ThreadDispatcher* cpp_thread_dispatcher_current();
bool cpp_thread_dispatcher_is_current(const ThreadDispatcher* thread);
uintptr_t cpp_thread_dispatcher_blocking_futex_id(const ThreadDispatcher* thread);
void cpp_thread_dispatcher_set_blocking_futex_id(ThreadDispatcher* thread, uintptr_t id);
bool cpp_thread_dispatcher_lock_acquire(ThreadDispatcher* thread);
void cpp_thread_dispatcher_lock_release(ThreadDispatcher* thread, bool should_clear);
void cpp_thread_dispatcher_observe_core_thread(
    ThreadDispatcher* thread, ffi::Uninitialized<ThreadDispatcher::CoreThreadObservation>* out_obs);
void cpp_thread_dispatcher_release_core_thread_observation(
    ThreadDispatcher::CoreThreadObservation* obs);
zx_status_t cpp_thread_dispatcher_read_state(ThreadDispatcher* thread, uint32_t state_kind,
                                             void* buffer, size_t buffer_size);
zx_status_t cpp_thread_dispatcher_write_state(ThreadDispatcher* thread, uint32_t state_kind,
                                              const void* buffer, size_t buffer_size);
zx_status_t cpp_sys_thread_raise_exception(uint32_t options, zx_excp_type_t type,
                                           const zx_exception_context_t* user_context);
zx_status_t cpp_sys_thread_legacy_yield(uint32_t options);
zx_status_t cpp_thread_dispatcher_get_exception_report(
    const ThreadDispatcher* thread, ffi::Uninitialized<zx_exception_report_t>* out_report);
}

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_DISPATCHER_H_
