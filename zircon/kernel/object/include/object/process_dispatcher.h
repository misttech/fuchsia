// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_

#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <fbl/array.h>
#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/name.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/brwlock.h>
#include <kernel/event.h>
#include <kernel/mutex.h>
#include <kernel/task_runtime_stats.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/forward.h>
#include <ktl/span.h>
#include <object/dispatcher.h>
#include <object/exceptionate.h>
#include <object/futex_context.h>
#include <object/handle.h>
#include <object/handle_table.h>
#include <object/job_policy.h>
#include <object/shareable_process_state.h>
#include <object/thread_dispatcher.h>
#include <vm/vm_aspace.h>

class JobDispatcher;
class VmarMapsInfoWriter;
class VmoInfoWriter;

// To allow this function to be friended by ProcessDispatcher.
template <typename T>
[[noreturn]] extern void RestrictedLeave(const T* restricted_state_source,
                                         zx_restricted_reason_t reason);

namespace internal {
// Tag for a ProcessDispatcher's parent JobDispatcher's raw job list.
struct ProcessDispatcherRawJobListTag {};
// Tag for a ProcessDispatcher's parent JobDispatcher's job list.
struct ProcessDispatcherJobListTag {};
}  // namespace internal

class ProcessDispatcher final
    : public SoloDispatcher<ProcessDispatcher, ZX_DEFAULT_PROCESS_RIGHTS>,
      public fbl::ContainableBaseClasses<
          fbl::TaggedDoublyLinkedListable<ProcessDispatcher*,
                                          internal::ProcessDispatcherRawJobListTag>,
          fbl::TaggedSinglyLinkedListable<fbl::RefPtr<ProcessDispatcher>,
                                          internal::ProcessDispatcherJobListTag>> {
 public:
  using RawJobListTag = internal::ProcessDispatcherRawJobListTag;
  using JobListTag = internal::ProcessDispatcherJobListTag;

  static zx_status_t Create(fbl::RefPtr<JobDispatcher> job, ktl::string_view name, uint32_t flags,
                            KernelHandle<ProcessDispatcher>* handle, zx_rights_t* rights,
                            KernelHandle<VmAddressRegionDispatcher>* root_vmar_handle,
                            zx_rights_t* root_vmar_rights);

  // Creates a new process dispatcher for a process that will share its `shareable_state_` with
  // other processes.
  //
  // The shared state will be instantiated from `shared_proc`.
  //
  // `restricted_vmar_handle` is the VMAR for the restricted aspace.
  static zx_status_t CreateShared(fbl::RefPtr<ProcessDispatcher> shared_proc, ktl::string_view name,
                                  uint32_t flags, KernelHandle<ProcessDispatcher>* handle,
                                  zx_rights_t* rights,
                                  KernelHandle<VmAddressRegionDispatcher>* restricted_vmar_handle,
                                  zx_rights_t* restricted_vmar_rights);

  static ProcessDispatcher* GetCurrent() {
    ThreadDispatcher* current = ThreadDispatcher::GetCurrent();
    DEBUG_ASSERT(current);
    return current->process();
  }

  static void ExitCurrent(int64_t retcode) __NO_RETURN {
    ThreadDispatcher* current = ThreadDispatcher::GetCurrent();
    DEBUG_ASSERT(current);
    current->process()->Exit(retcode);
  }

  // Dispatcher implementation
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PROCESS; }
  void on_zero_handles() final;
  zx_koid_t get_related_koid() const final;

  ~ProcessDispatcher() final;

  // state of the process
  enum class State {
    INITIAL,  // initial state, no thread present in process
    RUNNING,  // first thread has started and is running
    DYING,    // process has delivered kill signal to all threads
    DEAD,     // all threads have entered DEAD state and potentially dropped refs on process
  };

  // The type of address space used to initialize a ProcessDispatcher for a shared process.
  enum class SharedAspaceType {
    // Top half: a new shareable address space
    // Bottom half: nothing
    New,
    // Top half: shared address space from another process
    // Bottom half: a new restricted address space
    Shared
  };

  // Performs initialization on a newly constructed ProcessDispatcher
  //
  // This should be used to initialize ProcessDispatchers without a restricted aspace.
  //
  // If this fails, then the object is invalid and should be deleted
  zx_status_t Initialize();

  // Performs initialization on a newly constructed ProcessDispatcher
  // If this fails, then the object is invalid and should be deleted
  //
  // This should be used to initialize ProcessDispatchers with a restricted aspace.
  //
  // |type| is used to determine how to initialize the restricted and normal aspaces.
  zx_status_t Initialize(SharedAspaceType type);

  // Accessors.
  HandleTable& handle_table() { return shareable_state_->handle_table(); }
  const HandleTable& handle_table() const { return shareable_state_->handle_table(); }

  FutexContext& futex_context() { return shareable_state_->futex_context(); }

  // Returns a pointer to the process's VmAspace containing |va| if such an aspace exists, otherwise
  // it returns the normal aspace of the process.
  VmAspace* aspace_at(vaddr_t va);

#if ARCH_X86
  // Returns an identifier that can be used to associate hardware trace
  // data with this process.
  uintptr_t hw_trace_context_id() const {
    // TODO(https://fxbug.dev/42055932): Figure out how to make HW tracing work in restricted mode.
    return shareable_state_->aspace()->arch_aspace().pt_phys();
  }
#endif

  uintptr_t arch_table_phys() const {
    // TODO(https://fxbug.dev/42055932): Figure out how to make tracing works in restricted mode.
    return shareable_state_->aspace()->arch_aspace().arch_table_phys();
  }

  uintptr_t vdso_base_address() { return shareable_state_->aspace()->vdso_base_address(); }

  void EnumerateAspaceChildren(VmEnumerator* ve) {
    fbl::RefPtr<VmAddressRegion> root_vmar = shareable_state_->aspace()->RootVmar();
    if (root_vmar) {
      root_vmar->EnumerateChildren(ve);
    }
    if (restricted_aspace_) {
      root_vmar = restricted_aspace_->RootVmar();
      if (root_vmar) {
        root_vmar->EnumerateChildren(ve);
      }
    }
  }

  void DumpAspace(bool verbose) {
    shareable_state_->aspace()->Dump(true);
    if (restricted_aspace_) {
      restricted_aspace_->Dump(true);
    }
  }

  State state() const;

  fbl::RefPtr<JobDispatcher> job();

  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final;
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final;

  zx_status_t Start(fbl::RefPtr<ThreadDispatcher> thread, zx_vaddr_t pc, zx_vaddr_t sp,
                    HandleOwner arg_handle, uintptr_t arg2);

  void Kill(int64_t retcode);

  // Suspends the process.
  //
  // Suspending a process causes all child threads to suspend as well as any new children
  // that are added until the process is resumed. Suspend() is cumulative, so the process
  // will only resume once Resume() has been called an equal number of times.
  //
  // Returns ZX_OK on success, or ZX_ERR_BAD_STATE iff the process is dying or dead.
  zx_status_t Suspend();
  void Resume();

  // Syscall helpers
  zx_info_process_t GetInfo() const;
  zx_status_t GetStats(zx_info_task_stats_t* stats) const;

  // Get the runtime of all threads that previously ran or are currently running under this process.
  TaskRuntimeStats GetTaskRuntimeStats() const TA_EXCL(get_lock());

  zx_status_t GetAspaceMaps(VmarMapsInfoWriter& maps, size_t max, size_t* actual,
                            size_t* available) const;
  zx_status_t GetVmos(VmoInfoWriter& vmos, size_t max, size_t* actual, size_t* available);
  zx_status_t GetThreads(fbl::Array<zx_koid_t>* threads) const;
  zx_status_t SetCriticalToJob(fbl::RefPtr<JobDispatcher> critical_to_job, bool retcode_nonzero);

  bool CriticalToRootJob() const;

  Exceptionate* exceptionate();
  Exceptionate* debug_exceptionate();

  // The following two methods can be slow and inaccurate and should only be
  // called from diagnostics code.
  uint32_t ThreadCount() const;
  VmObject::AttributionCounts GetAttributedMemory() const;

  // Look up a process given its koid.
  // Returns nullptr if not found.
  static fbl::RefPtr<ProcessDispatcher> LookupProcessById(zx_koid_t koid);

  // Look up a thread in this process given its koid.
  // Returns nullptr if not found.
  fbl::RefPtr<ThreadDispatcher> LookupThreadById(zx_koid_t koid);

  uintptr_t get_debug_addr() const;
  zx_status_t set_debug_addr(uintptr_t addr);

  uintptr_t get_dyn_break_on_load() const;
  zx_status_t set_dyn_break_on_load(uintptr_t break_on_load);

  // Checks |condition| and enforces the parent job's policy.
  //
  // Depending on the parent job's policy, this method may signal an exception
  // on the calling thread or signal that the current process should be
  // killed.
  //
  // Must be called by syscalls before performing an action represented by an
  // ZX_POL_xxxxx condition. If the return value is ZX_OK the action can
  // proceed; otherwise, the process is not allowed to perform the action,
  // and the status value should be returned to the usermode caller.
  //
  // E.g., in sys_channel_create:
  //
  //     auto up = ProcessDispatcher::GetCurrent();
  //     zx_status_t res = up->EnforceBasicPolicy(ZX_POL_NEW_CHANNEL);
  //     if (res != ZX_OK) {
  //         // Channel creation denied by the calling process's
  //         // parent job's policy.
  //         return res;
  //     }
  //     // Ok to create a channel.
  __WARN_UNUSED_RESULT
  zx_status_t EnforceBasicPolicy(uint32_t condition);

  // Returns this job's timer slack policy.
  TimerSlack GetTimerSlackPolicy() const;

  // return a cached copy of the vdso code address or compute a new one
  uintptr_t vdso_code_address() {
    if (unlikely(vdso_code_address_ == 0)) {
      return cache_vdso_code_address();
    }
    return vdso_code_address_;
  }

  // Allocates a handle with the given rights to the given dispatcher. The handle is added to the
  // calling process' handle table, and its value is returned in out.
  zx_status_t MakeAndAddHandle(fbl::RefPtr<Dispatcher> dispatcher, zx_rights_t rights,
                               zx_handle_t* out);
  // Allocates a handle with the given rights to the dispatcher enclosed in the given kernel handle.
  // The handle is added to the calling process' handle table, and its value is returned in out.
  zx_status_t MakeAndAddHandle(KernelHandle<Dispatcher> kernel_handle, zx_rights_t rights,
                               zx_handle_t* out);

  // Returns the "restricted" address space for a process, or nullptr if it does not have a
  // restricted address space.
  //
  // The restricted address space spans the bottom half of the process' total address space, and is
  // private to the process. Threads executing in restricted mode are restricted to this address
  // space.
  VmAspace* restricted_aspace() { return restricted_aspace_.get(); }

  // Dispatch a user exception to job debugger exception channels.
  void OnUserExceptionForJobDebugger(ThreadDispatcher* t, const arch_exception_context_t* context);

 private:
  // Returns the normal address space for this process.
  //
  // All processes have a normal address space.  The normal aspace is the
  // address space that's active when a thread is in normal mode.
  //
  // For "shared processes", on architectures that support unified aspaces, the normal aspace
  // is a unified aspace. A unified aspace is an aspace that spans both the shared and restricted
  // aspace, and is used by threads in normal mode to avoid having to switch between the shared and
  // restricted aspaces.
  //
  // On architectures that don't yet support unified aspaces, the normal
  // aspace is a shared aspace (`ShareableProcessState::aspace()`).
  //
  // For non-shared processes (regular ones), the normal aspace is the one and only aspace
  // belonging to the process (`ShareableProcessState::aspace()`).
  //
  // TODO(https://fxbug.dev/42083004): Update this comment once all architectures support unified
  // aspaces.
  VmAspace* normal_aspace() {
    if (unified_aspace_) {
      return unified_aspace_.get();
    }
    return shareable_state_->aspace();
  }

  // Restricted mode is allowed to know about the internals of the aspaces.
  friend zx_status_t RestrictedEnter(uintptr_t vector_table_ptr, uintptr_t context);
  friend void RedirectRestrictedExceptionToNormalMode(RestrictedState* rs);
  template <typename T>
  friend void RestrictedLeave(const T* restricted_state_source, zx_restricted_reason_t reason);

  // Exit the current Process. It is an error to call this on anything other than the current
  // process. Please use ExitCurrent() instead of calling this directly.
  void Exit(int64_t retcode) __NO_RETURN;

  // compute the vdso code address and store in vdso_code_address_
  uintptr_t cache_vdso_code_address();

  // The diagnostic code is allow to know about the internals of this code.
  friend void DumpProcessList();
  friend void KillProcess(zx_koid_t id);
  friend void DumpProcessMemoryUsage(const char* prefix, size_t min_bytes);

  ProcessDispatcher(fbl::RefPtr<ShareableProcessState> shareable_state,
                    fbl::RefPtr<JobDispatcher> job, ktl::string_view name, uint32_t flags);

  ProcessDispatcher(const ProcessDispatcher&) = delete;
  ProcessDispatcher& operator=(const ProcessDispatcher&) = delete;

  void OnProcessStartForJobDebugger(ThreadDispatcher* t, const arch_exception_context_t* context);

  // Thread lifecycle support.
  friend class ThreadDispatcher;
  // Takes the given ThreadDispatcher and transitions it from the INITIALIZED state to a runnable
  // state (RUNNING or SUSPENDED depending on whether this process is suspended) by calling
  // ThreadDispatcher::MakeRunnable. The thread is then added to the thread_list_ for this process
  // and we transition to running if this is the initial_thread.
  //
  // If `ensure_initial_thread` is true, adding the thread will fail if is not the initial thread in
  // the process.
  zx_status_t AddInitializedThread(ThreadDispatcher* t, bool ensure_initial_thread,
                                   const ThreadDispatcher::EntryState& entry);
  void RemoveThread(ThreadDispatcher* t);

  void SetStateLocked(State) TA_REQ(get_lock());
  void FinishDeadTransition();

  // Kill all threads
  void KillAllThreadsLocked() TA_REQ(get_lock());

  const fbl::RefPtr<ShareableProcessState> shareable_state_;

  // the enclosing job
  const fbl::RefPtr<JobDispatcher> job_;

  // Job that this process is critical to.
  //
  // We require that the job is the parent of this process, or an ancestor.
  fbl::RefPtr<JobDispatcher> critical_to_job_ TA_GUARDED(get_lock());
  bool retcode_nonzero_ TA_GUARDED(get_lock()) = false;

  // Policy set by the Job during Create().
  //
  // It is critical that this field is immutable as it will be accessed without synchronization.
  const JobPolicy policy_;

  // list of threads in this process
  fbl::DoublyLinkedList<ThreadDispatcher*> thread_list_ TA_GUARDED(get_lock());

  // The address space used when a thread of this process is executing in restricted mode.  This
  // field is only non-null if this process is a "shared process".
  //
  // This field is logically const and may not be changed after initialization.  Resetting or
  // assigning to this field post-initialization is a programming error.
  fbl::RefPtr<VmAspace> restricted_aspace_;

  // The address space used when a thread is in normal mode but has a restricted address space.
  // This can be null if the process was not initialized with a restricted aspace, or the
  // architecture does not support unified aspaces.
  fbl::RefPtr<VmAspace> unified_aspace_;

  // our state
  State state_ TA_GUARDED(get_lock()) = State::INITIAL;

  // Suspend count; incremented on Suspend(), decremented on Resume().
  int suspend_count_ TA_GUARDED(get_lock()) = 0;

  // True if FinishDeadTransition has been called.
  // This is used as a sanity check only.
  bool completely_dead_ = false;

  // process return code
  int64_t retcode_ = 0;

  Exceptionate exceptionate_;
  Exceptionate debug_exceptionate_;

  // This is the value of _dl_debug_addr from ld.so.
  // See third_party/ulib/musl/ldso/dynlink.c.
  uintptr_t debug_addr_ TA_GUARDED(get_lock()) = 0;

  // Whether the dynamic loader should issue a debug trap when loading a shared library,
  // either initially or when running (e.g. dlopen).
  //
  // See docs/reference/syscalls/object_get_property.md
  // See third_party/ulib/musl/ldso/dynlink.c.
  uintptr_t dyn_break_on_load_ TA_GUARDED(get_lock()) = 0;

  // This is a cache of aspace()->vdso_code_address().
  uintptr_t vdso_code_address_ = 0;

  // The time at which the process was started.
  zx_instant_mono_t start_time_ = 0;

  // Hold accumulated stats for threads who have exited.
  TaskRuntimeStats accumulated_stats_ TA_GUARDED(get_lock());

  // The user-friendly process name. For debug purposes only. That
  // is, there is no mechanism to mint a handle to a process via this name.
  fbl::Name<ZX_MAX_NAME_LEN> name_;
};

const char* StateToString(ProcessDispatcher::State state);

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_
