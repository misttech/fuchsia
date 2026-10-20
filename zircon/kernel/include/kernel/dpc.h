// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_DPC_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_DPC_H_

#include <lib/kconcurrent/chainlock.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <kernel/cpu.h>
#include <kernel/event.h>
#include <kernel/thread.h>

// Deferred Procedure Calls - queue callback to invoke on the current cpu in thread context.
// Dpcs are executed with interrupts enabled, and do not ever migrate cpus while executing.
// A Dpc may not execute on the original current cpu if it is hotunplugged/offlined.
// Dpcs may block, though this may starve other queued work.

class Dpc : public fbl::DoublyLinkedListable<Dpc*, fbl::NodeOptions::AllowCopyMove> {
 public:
  using Func = void(Dpc*);

  explicit Dpc(Func* func, void* arg = nullptr) : func_(func), arg_(arg) {
    DEBUG_ASSERT(func_ != nullptr);
  }

  template <class ArgType>
  ArgType* arg() {
    return static_cast<ArgType*>(arg_);
  }

 private:
  Func* const func_;
  void* const arg_;
};

class DpcRunner;

extern "C" {

void rust_dpc_runner_init(DpcRunner* runner);
void rust_dpc_runner_destroy(DpcRunner* runner);
void rust_dpc_runner_init_for_current_cpu(const DpcRunner* runner);
zx_status_t rust_dpc_runner_shutdown(const DpcRunner* runner, zx_instant_mono_t deadline);
void rust_dpc_runner_transition_off_cpu(const DpcRunner* runner, const DpcRunner* source);
zx_status_t rust_dpc_runner_enqueue(const Dpc* dpc, uint32_t queue_type);

}  // extern "C"

// A DpcRunner is responsible for running queued Dpcs.  Under the hood, a given runner may manage
// multiple independent queues of Dpcs.
//
// Each cpu maintains a DpcRunner, in its percpu structure.
class DpcRunner {
 public:
  DpcRunner() { rust_dpc_runner_init(this); }
  ~DpcRunner() { rust_dpc_runner_destroy(this); }

  DpcRunner(const DpcRunner&) = delete;
  DpcRunner& operator=(const DpcRunner&) = delete;
  DpcRunner(DpcRunner&&) = delete;
  DpcRunner& operator=(DpcRunner&&) = delete;

  // Initializes this DpcRunner for the current cpu.
  //
  // The calling thread must have hard-affinity to exactly one CPU.
  void InitForCurrentCpu() { rust_dpc_runner_init_for_current_cpu(this); }

  // Begins the DpcRunner shutdown process.
  //
  // Shutting down a DpcRunner is a two-phase process.  This is the first phase.  See
  // |TransitionOffCpu| for the second phase.
  //
  // This method:
  // - tells the owning cpu's DpcRunner's thread to stop servicing its queue then
  // - waits, up to |deadline|, for it to finish any in-progress DPC and join
  //
  // Because this method blocks until the thread has terminated, it is critical that the caller
  // not hold any locks that might be needed by any previously queued DPCs.  Otheriwse, deadlock may
  // occur.
  //
  // Upon successful completion, this DpcRunner may contain unexecuted DPCs and new ones may be
  // added by |Enqueue|.  However, they will not execute (on any cpu) until |TransitionOffCpu| is
  // called.
  //
  // Once |Shutdown| has completed successfully, finish the shutdown process by calling
  // |TransitionOffCpu| on some cpu other than the owning cpu.
  //
  // If |Shutdown| fails, this DpcRunner is left in an undefined state and
  // |TransitionOffCpu| must not be called.
  zx_status_t Shutdown(zx_instant_mono_t deadline) {
    return rust_dpc_runner_shutdown(this, deadline);
  }

  // Moves queued Dpcs from |source| to this DpcRunner.
  //
  // This is the second phase of Dpc shutdown.  See |Shutdown|.
  //
  // This must only be called after |Shutdown| has completed successfully.
  //
  // This must only be called on the current cpu.
  void TransitionOffCpu(DpcRunner& source) { rust_dpc_runner_transition_off_cpu(this, &source); }

  // Dpc are placed into different queues depending on their ServiceLevel.  One queue for those that
  // need to execute with |LowLatency| (e.g. signaling a TimerDispatcher from interrupt context).
  // The other queue (|General|) is for the rest.
  //
  // |LowLatency| Dpcs are executed by a thread with a high-bandwidth scheduler profile so take
  // care to avoid creating undesirable PI interactions.  In other words, don't use |LowLatency|
  // unless you really need to, and be sure that the Dpc won't hold likely-to-be-contested locks for
  // a significant amount of time.  See also https://fxbug.dev/395669867.
  //
  // |General| Dpcs are serviced by a thread with a default scheduler profile.
  enum class QueueType : uint32_t { General, LowLatency };

  // Enqueue |dpc| in the specified queue |type| and signal its worker thread to execute it.
  //
  // |Enqueue| will not block, but it may wait briefly for a spinlock.
  //
  // |Enqueue| may return before or after the Dpc has executed.  It is the
  // caller's responsibility to ensure that a queued Dpc object is not destroyed
  // prior to its execution.
  //
  // Note: it is important that the thread calling Enqueue() not be holding its
  // own lock if there is _any_ possibility that the DPC thread could be
  // involved in a PI interaction with the thread who is performing the enqueue
  // operation.
  //
  // Returns ZX_ERR_ALREADY_EXISTS if |dpc| is already queued.
  static zx_status_t Enqueue(Dpc& dpc, QueueType type = QueueType::General)
      TA_EXCL(chainlock_transaction_token) {
    return rust_dpc_runner_enqueue(&dpc, static_cast<uint32_t>(type));
  }

 private:
  struct QueueLayout {
    bool stop;
    void* list;
    Thread* thread;
    alignas(Event) uint8_t event[sizeof(Event)];
  };

  [[maybe_unused]] cpu_num_t cpu_;
  [[maybe_unused]] bool initialized_;
  [[maybe_unused]] QueueLayout queue_general_;
  [[maybe_unused]] QueueLayout queue_low_latency_;
};

static_assert(sizeof(Dpc) == 32);
static_assert(alignof(Dpc) == 8);
static_assert(sizeof(DpcRunner) == 200);
static_assert(alignof(DpcRunner) == 8);
static_assert(static_cast<uint32_t>(DpcRunner::QueueType::General) == 0);
static_assert(static_cast<uint32_t>(DpcRunner::QueueType::LowLatency) == 1);

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_DPC_H_
