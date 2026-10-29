// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_H_

#include <lib/kconcurrent/chainlock.h>

#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <kernel/scheduler_state.h>
#include <kernel/thread.h>
#include <kernel/wait.h>
#include <ktl/optional.h>

class OwnedWaitQueue;

// The `ChainLock::Result` values returned by the Rust implementation cross the FFI boundary as
// `int32_t`s, so make sure that the enumerator values match those of the Rust `ChainLockResult`.
static_assert(static_cast<int32_t>(ChainLock::Result::Ok) == 0);
static_assert(static_cast<int32_t>(ChainLock::Result::Retry) == 1);
static_assert(static_cast<int32_t>(ChainLock::Result::Backoff) == 2);
static_assert(static_cast<int32_t>(ChainLock::Result::Cycle) == 3);

extern "C" {
int32_t rust_owned_wait_queue_lock_pi_chain_thread(Thread* start);
int32_t rust_owned_wait_queue_lock_pi_chain_wait_queue(WaitQueueBase* start);
void rust_owned_wait_queue_assert_not_owned(const OwnedWaitQueue* wait_queue);
void rust_owned_wait_queue_assert_safe_for_pooling(const OwnedWaitQueue* wait_queue);
}  // extern "C"

// Owned wait queues are an extension of wait queues which adds the concept of
// ownership for use when profile inheritance semantics are needed.
//
// An owned wait queue maintains an unmanaged pointer to a Thread in order to
// track who owns it at any point in time.  In addition, it contains node state
// which can be used by the owning thread in order to track the wait queues that
// the thread is currently an owner of.  This also makes use of unmanaged
// pointer.
//
// It should be an error for any thread to destruct while it owns an
// OwnedWaitQueue.  Likewise, it should be an error for any wait queue to
// destruct while it has an owner.  These invariants are enforced in the
// destructor for OwnedWaitQueue and Thread.  This enforcement is considered
// to be the reasoning why holding unmanaged pointers is considered to be safe.
//
class OwnedWaitQueue : public WaitQueueBase, public fbl::DoublyLinkedListable<OwnedWaitQueue*> {
 public:
  struct IWakeRequeueHook {
   public:
    IWakeRequeueHook() = default;

    // Note; do not force a virtual destructor here.  This interface exists
    // only for the purpose of injecting into various wake operations to be
    // used during thread selection and to update external bookkeeping.
    // Instances of it are meant to be stack allocated and should always be
    // destroyed in the scope where they were created.  They are never meant to
    // be heap allocated or have their lifecycle managed using an IWakeRequeueHook
    // pointer.

    IWakeRequeueHook(const IWakeRequeueHook&) = delete;
    IWakeRequeueHook(IWakeRequeueHook&&) = delete;
    IWakeRequeueHook& operator=(const IWakeRequeueHook&) = delete;
    IWakeRequeueHook& operator=(IWakeRequeueHook&&) = delete;

    // The Allow hook is called during the locking phase of a wake or a requeue
    // operation.  The locking code is holding the queue's lock at this point,
    // but not the thread's lock yet.  The thread is still a member of the
    // queue, meaning we have read-only access to the thread's variables.
    //
    // The implementation may use this hook to decide if this thread should be
    // woken or not, returning true if it should be and false otherwise.  If
    // true is returned, the locking operation will attempt to lock the thread
    // in order to commit it to waking, backing of if needed.  If false is
    // returned, the thread will be left in the queue and the enumeration of
    // threads in the queue ceases.
    //
    // Allow may be called multiple times for the same thread during a locking
    // operation if the code needs to back off and attempt to obtain lock the
    // set of threads to be woken multiple times.
    virtual bool Allow(const Thread& t) TA_REQ_SHARED(t.get_lock()) { return true; }

    // OnWakeOrRequeue is called in the second phase of a wake/requeue
    // operation.  At this point, all of the threads to wake have been
    // identified and successfully locked.  This hook is called _just_ before
    // the thread is removed from its queue, and either unblocked (wake
    // operation) or moved to a different queue (requeue).
    virtual void OnWakeOrRequeue(Thread& t) TA_REQ(t.get_lock()) {}

   protected:
    ~IWakeRequeueHook() = default;
  };

  static IWakeRequeueHook& default_wake_hooks() { return default_wake_hooks_; }

  // A small struct which contains details about the locking used in order to
  // prepare for a BlockAndAssignOwner operation.  See the comments in
  // LockForBAAOOperation for details.
  struct BAAOLockingDetails {
    BAAOLockingDetails() = default;
    explicit BAAOLockingDetails(Thread* initial_new_owner) : new_owner{initial_new_owner} {}

    Thread* new_owner{nullptr};
    const void* stop_point{nullptr};
  };

  // A structure which holds the state of the queue as observed just before the
  // queue lock was dropped.  Used by futex and kernel mutex implementations to
  // update their bookkeeping before unwinding after a wake operation.
  struct WakeThreadsResult {
    uint32_t woken{0};
    uint32_t still_waiting{0};
    Thread* owner{nullptr};
  };

  // Behavior control for wake/requeue operations
  enum class WakeOption { None, AssignOwner };

  static constexpr uint32_t kOwnedMagic = fbl::magic("ownq");
  constexpr OwnedWaitQueue() : WaitQueueBase(kOwnedMagic) {}
  ~OwnedWaitQueue();

  // No copy or move is permitted.
  DISALLOW_COPY_ASSIGN_AND_MOVE(OwnedWaitQueue);

  // Release ownership of all wait queues currently owned by |t| and update
  // bookkeeping as appropriate.  This is meant to be called from the thread
  // itself and therefore it is assumed that the thread in question is not
  // blocked on any other wait queues.
  static void DisownAllQueues(Thread* t) TA_EXCL(chainlock_transaction_token, t->get_lock());

  // Get the effective profile of an OwnedWaitQueue. The effective profile of an
  // OwnedWaitQueue is the composition of the effective profiles of all the
  // threads currently waiting in that queue. As such, the effective profile may
  // change as threads are added to or removed from the queue or when the
  // effective profile of any thread in the queue changes, either due to base
  // profile changes or transitive propagation of upstream profile changes from
  // owned wait queues that they own.
  SchedulerState::EffectiveProfile GetEffectiveProfile() const TA_REQ(get_lock());

  // Change a thread's base profile and deal with profile propagation side effects.
  static void SetThreadBaseProfileAndPropagate(Thread& thread,
                                               const SchedulerState::BaseProfile& profile)
      TA_EXCL(chainlock_transaction_token, thread.get_lock());

  // Attempt to lock the rest of a PI chain, starting from the (already locked)
  // node given by |start|.
  static ChainLock::Result LockPiChain(Thread& start)
      TA_REQ(chainlock_transaction_token, start.get_lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return static_cast<ChainLock::Result>(rust_owned_wait_queue_lock_pi_chain_thread(&start));
  }

  static ChainLock::Result LockPiChain(WaitQueueBase& start)
      TA_REQ(chainlock_transaction_token, start.get_lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    return static_cast<ChainLock::Result>(rust_owned_wait_queue_lock_pi_chain_wait_queue(&start));
  }

  // const accessor for the owner member.
  Thread* owner() const TA_REQ(get_lock()) { return owner_; }

  // Debug Assert wrapper which skips the thread analysis checks just to
  // assert that a specific queue is unowned.  Used by FutexContext
  void AssertNotOwned() const TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_owned_wait_queue_assert_not_owned(this);
  }

  void ResetOwnerIfNoWaiters() TA_EXCL(chainlock_transaction_token, get_lock());

  // Check invariants before returning to the pool.
  void AssertSafeForPooling() const TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_owned_wait_queue_assert_safe_for_pooling(this);
  }

  // Block the current thread on this wait queue, and re-assign ownership to
  // the specified thread (or remove ownership if new_owner is null).  If
  // assigning new_owner would produce a cycle, the proposed owner is rejected
  // (and any existing owner is removed), but the current thread still blocks.
  //
  // Note, if the new owner exists, but is dead or dying, it will not be
  // permitted to become the new owner of the wait_queue.  Any existing owner
  // will be replaced with no owner in this situation.
  zx_status_t BlockAndAssignOwner(const Deadline& deadline, Thread* new_owner,
                                  ResourceOwnership resource_ownership, Interruptible interruptible)
      TA_EXCL(chainlock_transaction_token) TA_REQ(preempt_disabled_token);

  zx_status_t BlockAndAssignOwnerLocked(Thread* current_thread, const Deadline& deadline,
                                        const BAAOLockingDetails& details,
                                        ResourceOwnership resource_ownership,
                                        Interruptible interruptible,
                                        ForceInheritance force_inheritance = ForceInheritance::No)
      TA_REQ(chainlock_transaction_token, current_thread->get_lock(), preempt_disabled_token)
          TA_REL(get_lock());

  // Attempt to obtain all of the locks needed for a BAAO operation while
  // already holding this queue's lock.  On success, the current thread's lock
  // will have been obtained in addition to any other locks in the PI chain
  // which are needed, and the details of the locking operation will be
  // returned.  On failure, returns ktl::nullopt after dropping any locks which
  // had been obtained.
  bool TryLockForBAAOOperationLocked(Thread* current_thread, Thread* new_owner,
                                     BAAOLockingDetails& details_out)
      TA_REQ(chainlock_transaction_token, get_lock()) TA_TRY_ACQ(true, current_thread->get_lock());

  // Wake the up to specified number of threads from the wait queue, removing
  // any current queue owner in the process.
  WakeThreadsResult WakeThreads(uint32_t wake_count,
                                IWakeRequeueHook& wake_hooks = default_wake_hooks_)
      TA_EXCL(chainlock_transaction_token, get_lock());

  // Attempt to wake exactly one thread, and assign ownership of the queue to
  // the woken thread (if any).
  WakeThreadsResult WakeThreadAndAssignOwner(IWakeRequeueHook& wake_hooks = default_wake_hooks_)
      TA_EXCL(chainlock_transaction_token, get_lock());

  WakeThreadsResult WakeThreadsLocked(Thread::UnblockList threads, IWakeRequeueHook& wake_hooks,
                                      WakeOption option = WakeOption::None,
                                      ForceInheritance force_inheritance = ForceInheritance::No)
      TA_REQ(chainlock_transaction_token, get_lock(), preempt_disabled_token);

  // Obtain all of the locks needed for a WakeThreads operation.
  bool LockForWakeOperationOrBackoff(uint32_t max_wake, IWakeRequeueHook& wake_hooks,
                                     Thread::UnblockList& unblock_list_out)
      TA_REQ(chainlock_transaction_token) TA_TRY_ACQ(true, get_lock());

  ktl::optional<Thread::UnblockList> LockForWakeOperationLocked(uint32_t max_wake,
                                                                IWakeRequeueHook& wake_hooks)
      TA_REQ(chainlock_transaction_token, get_lock());

  // Accessor used only by the scheduler's PiNodeAdapter to handle bookkeeping
  // during profile inheritance situations.
  SchedulerState::WaitQueueInheritedSchedulerState* inherited_scheduler_state_storage()
      TA_REQ(get_lock()) {
    return inherited_scheduler_state_storage_;
  }

  const SchedulerState::WaitQueueInheritedSchedulerState* inherited_scheduler_state_storage() const
      TA_REQ(get_lock()) {
    return inherited_scheduler_state_storage_;
  }

  const SchedulerState::WaitQueueInheritedSchedulerState* const_inherited_scheduler_state_storage()
      const TA_REQ(get_lock()) {
    return inherited_scheduler_state_storage_;
  }

 private:
  struct DefaultWakeHooks : public IWakeRequeueHook {};
  static inline DefaultWakeHooks default_wake_hooks_;

  TA_GUARDED(get_lock()) Thread* owner_ = nullptr;

  // A pointer to a thread (which _must_ be a current member of this owned wait queue's)
  // collection which holds the current inherited scheduler state storage for
  // this wait queue.
  //
  // Anytime that a thread is added to this queue, if the collection was
  // empty, then the thread added becomes the new location for the storage.
  //
  // Anytime that a thread is removed from this queue, if the thread being
  // removed was the active location of the storage, then a new thread needs to
  // be selected and the values moved into the new storage location.
  //
  // The primary purpose of this indirection is to keep the OwnedWaitQueue object
  // relatively-lightweight.  The only time that a wait queue has defined
  // inherited scheduler state is when there are one or more threads blocked in
  // the queue, so storing this bookkeeping in a blocked thread object seems
  // reasonable.
  //
  // TODO(johngro): An alternative approach would be to copy the token-ante
  // system used by Theads and FutexContext.
  //
  // Pros: Easier to reason about code and reduced risk of accidental
  //       use-after-free.  Also, no need to copy bookkeeping from one thread to
  //       another when the currently active thread leaves the OwnedWaitQueue.
  // Cons: Storage of the free tokens would require some central pool system
  //       which would act as a central choke point, which might become a
  //       scalability issue on systems where the processor count is high.
  //
  SchedulerState::WaitQueueInheritedSchedulerState* inherited_scheduler_state_storage_{nullptr};
};

extern "C" {
void cpp_thread_unblock_list_init(ffi::Uninitialized<Thread::UnblockList>* list);
void cpp_thread_unblock_list_destroy(Thread::UnblockList* list);

bool cpp_owned_wait_queue_wake_requeue_hook_allow(OwnedWaitQueue::IWakeRequeueHook* hook,
                                                  const Thread* t);
void cpp_owned_wait_queue_wake_requeue_hook_on_wake_or_requeue(
    OwnedWaitQueue::IWakeRequeueHook* hook, Thread* t);

void rust_owned_wait_queue_destroy(OwnedWaitQueue* wait_queue);
void rust_owned_wait_queue_disown_all_queues(Thread* t);
void rust_owned_wait_queue_get_effective_profile(const OwnedWaitQueue* wait_queue,
                                                 SchedulerState::EffectiveProfile* out_profile);
void rust_owned_wait_queue_set_thread_base_profile_and_propagate(
    Thread* thread, const SchedulerState::BaseProfile* profile);
void rust_owned_wait_queue_reset_owner_if_no_waiters(OwnedWaitQueue* wait_queue);
zx_status_t rust_owned_wait_queue_block_and_assign_owner(OwnedWaitQueue* wait_queue,
                                                         const Deadline* deadline,
                                                         Thread* new_owner,
                                                         ResourceOwnership resource_ownership,
                                                         Interruptible interruptible);
zx_status_t rust_owned_wait_queue_block_and_assign_owner_locked(
    OwnedWaitQueue* wait_queue, Thread* current_thread, const Deadline* deadline,
    const OwnedWaitQueue::BAAOLockingDetails* details, ResourceOwnership resource_ownership,
    Interruptible interruptible, ForceInheritance force_inheritance);
bool rust_owned_wait_queue_try_lock_for_baao_operation_locked(
    OwnedWaitQueue* wait_queue, Thread* current_thread, Thread* new_owner,
    OwnedWaitQueue::BAAOLockingDetails* details_out);
void rust_owned_wait_queue_wake_threads(OwnedWaitQueue* wait_queue, uint32_t wake_count,
                                        OwnedWaitQueue::IWakeRequeueHook* wake_hooks,
                                        OwnedWaitQueue::WakeThreadsResult* out_result);
void rust_owned_wait_queue_wake_thread_and_assign_owner(
    OwnedWaitQueue* wait_queue, OwnedWaitQueue::IWakeRequeueHook* wake_hooks,
    OwnedWaitQueue::WakeThreadsResult* out_result);
void rust_owned_wait_queue_wake_threads_locked(OwnedWaitQueue* wait_queue,
                                               Thread::UnblockList* threads,
                                               OwnedWaitQueue::IWakeRequeueHook* wake_hooks,
                                               OwnedWaitQueue::WakeOption option,
                                               ForceInheritance force_inheritance,
                                               OwnedWaitQueue::WakeThreadsResult* out_result);
bool rust_owned_wait_queue_lock_for_wake_operation_or_backoff(
    OwnedWaitQueue* wait_queue, uint32_t max_wake, OwnedWaitQueue::IWakeRequeueHook* wake_hooks,
    Thread::UnblockList* unblock_list_out);
bool rust_owned_wait_queue_lock_for_wake_operation_locked(
    OwnedWaitQueue* wait_queue, uint32_t max_wake, OwnedWaitQueue::IWakeRequeueHook* wake_hooks,
    Thread::UnblockList* unblock_list_out);
}  // extern "C"

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_H_
