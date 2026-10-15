// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/scheduler/role.h>
#include <lib/zx/socket.h>
#include <zircon/syscalls.h>

#include <atomic>
#include <thread>
#include <utility>
#include <vector>

#include <fbl/string_printf.h>
#include <perftest/perftest.h>

#include "assert.h"

namespace {

constexpr uint32_t kBatchMessages = 32;

inline void CpuRelax() {
#if defined(__aarch64__)
  __asm__ volatile("yield" ::: "memory");
#elif defined(__x86_64__)
  __builtin_ia32_pause();
#elif defined(__riscv)
  __asm__ volatile("pause" ::: "memory");
#else
#error "Unsupported architecture"
#endif
}

template <typename Predicate>
inline void SpinWaitUntil(Predicate&& pred) {
  // Spin in userspace long enough to cover an entire batch step on multi-core
  // hardware without extra syscalls, while yielding on single-vCPU emulators
  // so unit-test mode does not wait for a scheduler quantum preemption.
  constexpr uint32_t kSpinLimit = 100000;
  uint32_t spins = 0;
  while (!pred()) {
    if (spins < kSpinLimit) {
      spins++;
      CpuRelax();
    } else {
      zx_thread_legacy_yield(0);
    }
  }
}

// Helper for running two functions concurrently across two threads while
// keeping thread creation and teardown outside the timed benchmark loop.
// The constructor spawns a background thread and waits for it to be ready,
// and each RunConcurrent() call uses generation counters with spin-waiting
// to start `work_fn` on the worker thread at roughly the same time as
// `main_fn` on the calling thread and wait for both to finish.
//
// Example usage:
//
//   // Thread setup.
//   ConcurrentWorker worker(func1);
//   // This runs func1 and func2 at roughly the same time, with func1
//   // being run on a separate thread.
//   worker.RunConcurrent(func2);
template <typename WorkFn>
class ConcurrentWorker {
 public:
  explicit ConcurrentWorker(WorkFn work_fn)
      : work_fn_(std::move(work_fn)), thread_([this] { ThreadLoop(); }) {
    SpinWaitUntil([this] { return ready_.load(std::memory_order_acquire); });
  }

  ~ConcurrentWorker() {
    stop_.store(true, std::memory_order_release);
    start_gen_.store(next_gen_ + 1, std::memory_order_release);
    thread_.join();
  }

  template <typename MainFn>
  void RunConcurrent(MainFn&& main_fn) {
    const uint64_t gen = ++next_gen_;
    start_gen_.store(gen, std::memory_order_release);
    main_fn();
    SpinWaitUntil([this, gen] { return done_gen_.load(std::memory_order_acquire) == gen; });
  }

 private:
  void ThreadLoop() {
#if BOARD_IS_VIM3
    ASSERT_OK(
        fuchsia_scheduler::SetRoleForThisThread("fuchsia.microbenchmarks.pin_to_vim3_big_cores"));
#endif
    ready_.store(true, std::memory_order_release);
    uint64_t expected_gen = 1;
    for (;;) {
      SpinWaitUntil([this, expected_gen] {
        return start_gen_.load(std::memory_order_acquire) == expected_gen ||
               stop_.load(std::memory_order_acquire);
      });
      if (stop_.load(std::memory_order_acquire)) {
        return;
      }
      work_fn_();
      done_gen_.store(expected_gen, std::memory_order_release);
      ++expected_gen;
    }
  }

  WorkFn work_fn_;
  uint64_t next_gen_ = 0;
  alignas(64) std::atomic<uint64_t> start_gen_{0};
  alignas(64) std::atomic<uint64_t> done_gen_{0};
  std::atomic<bool> ready_{false};
  std::atomic<bool> stop_{false};
  std::thread thread_;
};

// Measure the times taken to enqueue and then dequeue some bytes from a
// Zircon socket, on a single thread.  This does not involve any
// cross-thread wakeups. The |socket_flags| can be used to control whether the
// socket is a stream or a datagram, and |queued| represents how many messages
// to write to the socket before beginning the benchmark. Having messages queued
// on the socket allows for testing scenarios where the socket stays non-empty,
// versus transitions from empty to non-empty and back on each iteration.
bool SocketWriteReadTest(perftest::RepeatState* state, uint32_t socket_flags, uint32_t message_size,
                         uint32_t queued) {
  state->DeclareStep("write");
  state->DeclareStep("read");

  zx::socket socket1;
  zx::socket socket2;
  ASSERT_OK(zx::socket::create(socket_flags, &socket1, &socket2));
  std::vector<char> buffer(message_size);

  for (uint32_t i = 0; i < queued; i++) {
    size_t bytes_written;
    ASSERT_OK(socket1.write(0, buffer.data(), buffer.size(), &bytes_written));
    ZX_ASSERT(bytes_written == buffer.size());
  }

  while (state->KeepRunning()) {
    size_t bytes_written;
    ASSERT_OK(socket1.write(0, buffer.data(), buffer.size(), &bytes_written));
    ZX_ASSERT(bytes_written == buffer.size());
    state->NextStep();

    size_t bytes_read;
    ASSERT_OK(socket2.read(0, buffer.data(), buffer.size(), &bytes_read));
    ZX_ASSERT(bytes_read == buffer.size());
  }
  return true;
}

// The concurrent tests below measure multi-threaded throughput and kernel lock
// contention across the two directions of a connected socket pair. Whereas the
// single-threaded SocketWriteReadTest above only exercises uncontended lock
// acquisition, each endpoint of a Zircon socket pair has its own independent
// receive queue (`MBufChain`) that can be accessed concurrently when reading
// from both endpoints, writing and reading on the same endpoint, or performing
// full-duplex writes and reads.
//
// Measure concurrent reads from both endpoints of a connected socket pair
// across two threads (`socket1.read` on the main thread and `socket2.read`
// on a helper thread).
//
// Queue-depth invariants:
// - Before the loop, 1 message is pre-queued on both `socket1` and `socket2`.
// - In each iteration's single-threaded "fill" step, `kBatchMessages` messages
//   are written to each endpoint, bringing both receive queues from 1 to
//   `kBatchMessages + 1` messages.
// - In the concurrent "read" step, each thread reads `kBatchMessages` messages
//   from its endpoint, draining both receive queues from `kBatchMessages + 1`
//   back down to 1 message.
// This keeps both endpoints non-empty (`>= 1` message) and well below buffer
// capacity throughout the benchmark, so reads never run out of messages.
bool SocketDualEndpointReadTest(perftest::RepeatState* state, uint32_t socket_flags,
                                uint32_t message_size) {
  state->DeclareStep("fill");
  state->DeclareStep("read");

  zx::socket socket1;
  zx::socket socket2;
  ASSERT_OK(zx::socket::create(socket_flags, &socket1, &socket2));
  std::vector<char> main_buf(message_size);
  std::vector<char> worker_buf(message_size);

  size_t bytes_written;
  ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
  ZX_ASSERT(bytes_written == main_buf.size());
  ASSERT_OK(socket2.write(0, main_buf.data(), main_buf.size(), &bytes_written));
  ZX_ASSERT(bytes_written == main_buf.size());

  ConcurrentWorker worker([&] {
    for (uint32_t i = 0; i < kBatchMessages; i++) {
      size_t bytes_read;
      ASSERT_OK(socket2.read(0, worker_buf.data(), worker_buf.size(), &bytes_read));
      ZX_ASSERT(bytes_read == worker_buf.size());
    }
  });

  while (state->KeepRunning()) {
    for (uint32_t i = 0; i < kBatchMessages; i++) {
      ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
      ZX_ASSERT(bytes_written == main_buf.size());
      ASSERT_OK(socket2.write(0, main_buf.data(), main_buf.size(), &bytes_written));
      ZX_ASSERT(bytes_written == main_buf.size());
    }
    state->NextStep();

    worker.RunConcurrent([&] {
      for (uint32_t i = 0; i < kBatchMessages; i++) {
        size_t bytes_read;
        ASSERT_OK(socket1.read(0, main_buf.data(), main_buf.size(), &bytes_read));
        ZX_ASSERT(bytes_read == main_buf.size());
      }
    });
  }
  return true;
}

// Measure concurrent writes and reads on the same socket endpoint across two
// threads (`socket1.write` on the main thread enqueuing into `socket2`'s
// receive queue, while a helper thread concurrently calls `socket1.read`
// dequeuing from `socket1`'s receive queue).
//
// Queue-depth invariants:
// - Before the loop, `kBatchMessages + 1` messages are written to `socket1`
//   (so `socket2`'s receive queue holds `kBatchMessages + 1` messages) and
//   1 message is written to `socket2` (so `socket1`'s receive queue holds
//   1 message).
// - In each iteration's single-threaded "setup" step:
//   - `socket2.read` is called `kBatchMessages` times, draining `socket2`'s
//     receive queue from `kBatchMessages + 1` down to 1 message.
//   - `socket2.write` is called `kBatchMessages` times, filling `socket1`'s
//     receive queue from 1 up to `kBatchMessages + 1` messages.
// - In the concurrent "write_and_read" step:
//   - The main thread calls `socket1.write` `kBatchMessages` times, refilling
//     `socket2`'s receive queue from 1 back to `kBatchMessages + 1` messages.
//   - The worker thread calls `socket1.read` `kBatchMessages` times, draining
//     `socket1`'s receive queue from `kBatchMessages + 1` back down to
//     1 message.
// Because `socket1.write` and `socket1.read` operate on separate receive
// queues (`socket2`'s and `socket1`'s respectively) and `socket1`'s queue
// already contains `kBatchMessages + 1` messages before the concurrent step
// begins, neither endpoint ever runs out of messages or exceeds
// `kBatchMessages + 1` messages regardless of thread scheduling.
bool SocketSameEndpointWriteAndReadTest(perftest::RepeatState* state, uint32_t socket_flags,
                                        uint32_t message_size) {
  state->DeclareStep("setup");
  state->DeclareStep("write_and_read");

  zx::socket socket1;
  zx::socket socket2;
  ASSERT_OK(zx::socket::create(socket_flags, &socket1, &socket2));
  std::vector<char> main_buf(message_size);
  std::vector<char> worker_buf(message_size);

  for (uint32_t i = 0; i < kBatchMessages + 1; i++) {
    size_t bytes_written;
    ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
    ZX_ASSERT(bytes_written == main_buf.size());
  }
  {
    size_t bytes_written;
    ASSERT_OK(socket2.write(0, main_buf.data(), main_buf.size(), &bytes_written));
    ZX_ASSERT(bytes_written == main_buf.size());
  }

  ConcurrentWorker worker([&] {
    for (uint32_t i = 0; i < kBatchMessages; i++) {
      size_t bytes_read;
      ASSERT_OK(socket1.read(0, worker_buf.data(), worker_buf.size(), &bytes_read));
      ZX_ASSERT(bytes_read == worker_buf.size());
    }
  });

  while (state->KeepRunning()) {
    for (uint32_t i = 0; i < kBatchMessages; i++) {
      size_t bytes_read;
      ASSERT_OK(socket2.read(0, main_buf.data(), main_buf.size(), &bytes_read));
      ZX_ASSERT(bytes_read == main_buf.size());
      size_t bytes_written;
      ASSERT_OK(socket2.write(0, main_buf.data(), main_buf.size(), &bytes_written));
      ZX_ASSERT(bytes_written == main_buf.size());
    }
    state->NextStep();

    worker.RunConcurrent([&] {
      for (uint32_t i = 0; i < kBatchMessages; i++) {
        size_t bytes_written;
        ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
        ZX_ASSERT(bytes_written == main_buf.size());
      }
    });
  }
  return true;
}

// Measure full-duplex concurrent writes and reads on both endpoints of a
// connected socket pair (`socket1` on the main thread and `socket2` on a
// helper thread).
//
// Queue-depth invariants:
// - Before the loop, `kBatchMessages + 1` messages are pre-queued in each
//   direction (`socket1` and `socket2` each start with `kBatchMessages + 1`
//   messages in their receive queue).
// - In each iteration, both threads execute `kBatchMessages` pairs of
//   `(write, read)`. Even if one thread completes all `kBatchMessages` reads
//   before the other thread performs its first write, the receive queue depth
//   only drops from `kBatchMessages + 1` to 1 message (never empty).
//   Conversely, if one thread completes all `kBatchMessages` writes before the
//   other thread performs its first read, the receive queue depth rises to at
//   most `2 * kBatchMessages + 1` (65) messages, which fits well within the
//   256 KB socket buffer capacity. At the end of each iteration, both receive
//   queues return to `kBatchMessages + 1` messages.
bool SocketBidirectionalWriteReadTest(perftest::RepeatState* state, uint32_t socket_flags,
                                      uint32_t message_size) {
  zx::socket socket1;
  zx::socket socket2;
  ASSERT_OK(zx::socket::create(socket_flags, &socket1, &socket2));
  std::vector<char> main_buf(message_size);
  std::vector<char> worker_buf(message_size);

  for (uint32_t i = 0; i < kBatchMessages + 1; i++) {
    size_t bytes_written;
    ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
    ZX_ASSERT(bytes_written == main_buf.size());
    ASSERT_OK(socket2.write(0, main_buf.data(), main_buf.size(), &bytes_written));
    ZX_ASSERT(bytes_written == main_buf.size());
  }

  ConcurrentWorker worker([&] {
    for (uint32_t i = 0; i < kBatchMessages; i++) {
      size_t bytes_written;
      ASSERT_OK(socket2.write(0, worker_buf.data(), worker_buf.size(), &bytes_written));
      ZX_ASSERT(bytes_written == worker_buf.size());
      size_t bytes_read;
      ASSERT_OK(socket2.read(0, worker_buf.data(), worker_buf.size(), &bytes_read));
      ZX_ASSERT(bytes_read == worker_buf.size());
    }
  });

  while (state->KeepRunning()) {
    worker.RunConcurrent([&] {
      for (uint32_t i = 0; i < kBatchMessages; i++) {
        size_t bytes_written;
        ASSERT_OK(socket1.write(0, main_buf.data(), main_buf.size(), &bytes_written));
        ZX_ASSERT(bytes_written == main_buf.size());
        size_t bytes_read;
        ASSERT_OK(socket1.read(0, main_buf.data(), main_buf.size(), &bytes_read));
        ZX_ASSERT(bytes_read == main_buf.size());
      }
    });
  }
  return true;
}

void RegisterTests() {
  // Since stream payloads can coexist in different MBufChains internally we want to test cases
  // where messages always sit inside a buffer, will stride across a single buffer boundary, and
  // require multiple buffers.
  static const unsigned kStreamMessageSizesInBytes[] = {
      64,
      1 * 1024,
      64 * 1024,
  };
  // Datagrams always occupy their own MBufChain, so just test a very small and very large message
  // to show baseline costs versus copying overhead.
  static const unsigned kDatagramMessageSizesInBytes[] = {
      64,
      64 * 1024,
  };
  static const unsigned kMessagesToQueue[] = {
      0,
      1,
  };
  // For multi-threaded batched benchmarks (2 * kBatchMessages + 1 = 65 queued messages max), test
  // small/medium message sizes where lock contention dominates and 65 messages fit within the
  // 256 KB socket buffer capacity without blocking.
  static const unsigned kConcurrentStreamMessageSizesInBytes[] = {
      64,
      1 * 1024,
  };
  static const unsigned kConcurrentDatagramMessageSizesInBytes[] = {
      64,
  };

  for (auto message_size : kStreamMessageSizesInBytes) {
    for (auto queued_messages : kMessagesToQueue) {
      auto name = fbl::StringPrintf("Socket/Stream/WriteRead/%ubytes/%uqueued", message_size,
                                    queued_messages);
      perftest::RegisterTest(name.c_str(), SocketWriteReadTest, ZX_SOCKET_STREAM, message_size,
                             queued_messages);
    }
  }
  for (auto message_size : kDatagramMessageSizesInBytes) {
    for (auto queued_messages : kMessagesToQueue) {
      auto name = fbl::StringPrintf("Socket/Datagram/WriteRead/%ubytes/%uqueued", message_size,
                                    queued_messages);
      perftest::RegisterTest(name.c_str(), SocketWriteReadTest, ZX_SOCKET_DATAGRAM, message_size,
                             queued_messages);
    }
  }

  for (auto message_size : kConcurrentStreamMessageSizesInBytes) {
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Stream/DualEndpointRead/%ubytes", message_size).c_str(),
        SocketDualEndpointReadTest, ZX_SOCKET_STREAM, message_size);
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Stream/SameEndpointWriteAndRead/%ubytes", message_size).c_str(),
        SocketSameEndpointWriteAndReadTest, ZX_SOCKET_STREAM, message_size);
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Stream/BidirectionalWriteRead/%ubytes", message_size).c_str(),
        SocketBidirectionalWriteReadTest, ZX_SOCKET_STREAM, message_size);
  }
  for (auto message_size : kConcurrentDatagramMessageSizesInBytes) {
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Datagram/DualEndpointRead/%ubytes", message_size).c_str(),
        SocketDualEndpointReadTest, ZX_SOCKET_DATAGRAM, message_size);
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Datagram/SameEndpointWriteAndRead/%ubytes", message_size).c_str(),
        SocketSameEndpointWriteAndReadTest, ZX_SOCKET_DATAGRAM, message_size);
    perftest::RegisterTest(
        fbl::StringPrintf("Socket/Datagram/BidirectionalWriteRead/%ubytes", message_size).c_str(),
        SocketBidirectionalWriteReadTest, ZX_SOCKET_DATAGRAM, message_size);
  }
}
PERFTEST_CTOR(RegisterTests)

}  // namespace
