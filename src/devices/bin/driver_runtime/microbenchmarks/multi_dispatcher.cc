// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/irq.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/fdf/cpp/arena.h>
#include <lib/fdf/cpp/channel.h>
#include <lib/fdf/cpp/channel_read.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fdf/cpp/env.h>
#include <lib/fdf/env.h>
#include <lib/fdf/testing.h>
#include <lib/sync/cpp/completion.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/event.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/time.h>

#include <atomic>
#include <format>
#include <functional>
#include <memory>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include <perftest/perftest.h>

#include "src/devices/bin/driver_runtime/dispatcher.h"
#include "src/devices/bin/driver_runtime/microbenchmarks/assert.h"

namespace {

constexpr uint32_t kMsgSize = 64;
constexpr uint32_t kTag = 'BNCH';

// Resets the driver runtime environment so that any extra threads spawned by
// previous ALLOW_SYNC_CALLS benchmarks do not carry over into subsequent tests.
void ResetRuntimeEnv() {
  fdf_env_destroy_all_dispatchers();
  fdf_env_reset();
  ASSERT_OK(fdf_env_start(0));
}

void BusyWait(zx::duration duration) {
  const zx::time deadline = zx::clock::get_monotonic() + duration;
  while (zx::clock::get_monotonic() < deadline) {
    perftest::DoNotOptimize(deadline);
  }
}

void LogServerDispatcherStats(std::string_view test_name, fdf_dispatcher_t* fdf_dispatcher) {
  driver_runtime::Dispatcher* dispatcher = fdf_dispatcher->GetDispatcher();
  driver_runtime::DumpState dump_state;
  dispatcher->Dump(&dump_state);
  FX_LOGS(INFO) << test_name << ": total_requests=" << dump_state.debug_stats.num_total_requests
                << " inlined=" << dump_state.debug_stats.num_inlined_requests
                << " parallel_dispatch=" << dump_state.debug_stats.non_inlined.parallel_dispatch;
}

std::unique_ptr<fdf::ChannelRead> RegisterChannelEchoReader(const fdf::Channel& channel,
                                                            fdf_dispatcher_t* dispatcher) {
  auto reader = std::make_unique<fdf::ChannelRead>(
      channel.get(), 0,
      [](fdf_dispatcher_t* disp, fdf::ChannelRead* channel_read, zx_status_t status) {
        if (status != ZX_OK) {
          return;
        }
        fdf::UnownedChannel ch(channel_read->channel());
        zx::result<fdf::Channel::ReadReturn> read = ch->Read(0);
        ASSERT_OK(read.status_value());
        // Re-arm before echoing the reply so the wait is registered before the caller wakes.
        ASSERT_OK(channel_read->Begin(disp));
        ASSERT_OK(ch->Write(0, read->arena, read->data, read->num_bytes, cpp20::span<zx_handle_t>())
                      .status_value());
      });
  ASSERT_OK(reader->Begin(dispatcher));
  return reader;
}

void CancelIrqOnDispatcher(async_dispatcher_t* dispatcher, async::Irq* irq) {
  libsync::Completion unbind_complete;
  ASSERT_OK(async::PostTask(dispatcher, [&] {
    ASSERT_OK(irq->Cancel());
    unbind_complete.Signal();
  }));
  unbind_complete.Wait();
}

// ============================================================================
// 1. Colocated Noisy-Neighbor Tail-Latency Benchmark
//    (MultiDispatcher/TailLatencyUnderLoad)
// ============================================================================

enum class VictimType : uint8_t {
  kWait,
  kIrq,
  kChannelRead,
};

struct NoisyNeighbor {
  uint32_t owner = 0;
  fdf::SynchronizedDispatcher dispatcher;
  libsync::Completion shutdown_completion;
  libsync::Completion started_completion;
  zx::event event;
  std::unique_ptr<async::Wait> wait;
  bool started = false;
};

// Measure wakeup-to-callback latency on a victim synchronized dispatcher while K colocated
// noisy-neighbor synchronized dispatchers continuously execute callbacks of duration |busy_us|.
bool TailLatencyUnderLoadTest(perftest::RepeatState* state, VictimType victim_type,
                              uint32_t num_neighbors, uint32_t busy_us) {
  ResetRuntimeEnv();

  std::atomic<bool> stop{false};

  uint32_t victim_owner = 0;
  libsync::Completion victim_shutdown;
  zx::result<fdf::SynchronizedDispatcher> victim_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &victim_owner, {}, "victim", [&](fdf_dispatcher_t*) { victim_shutdown.Signal(); });
  ASSERT_OK(victim_dispatcher.status_value());

  std::vector<std::unique_ptr<NoisyNeighbor>> neighbors;
  neighbors.reserve(num_neighbors);
  const zx::duration busy_duration = zx::usec(busy_us);

  for (uint32_t i = 0; i < num_neighbors; ++i) {
    auto neighbor = std::make_unique<NoisyNeighbor>();
    std::string name = std::format("neighbor-{}", i);
    zx::result<fdf::SynchronizedDispatcher> dispatcher =
        fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
            &neighbor->owner, {}, name,
            [n = neighbor.get()](fdf_dispatcher_t*) { n->shutdown_completion.Signal(); });
    ASSERT_OK(dispatcher.status_value());
    neighbor->dispatcher = *std::move(dispatcher);

    ASSERT_OK(zx::event::create(0, &neighbor->event));
    ASSERT_OK(neighbor->event.signal(0, ZX_USER_SIGNAL_0));

    neighbor->wait = std::make_unique<async::Wait>(
        neighbor->event.get(), ZX_USER_SIGNAL_0, 0,
        [n = neighbor.get(), &stop, busy_duration](async_dispatcher_t* disp, async::Wait* w,
                                                   zx_status_t status, const zx_packet_signal_t*) {
          if (status != ZX_OK || stop.load(std::memory_order_relaxed)) {
            return;
          }
          if (!n->started) {
            n->started = true;
            n->started_completion.Signal();
          }
          if (busy_duration > zx::duration(0)) {
            BusyWait(busy_duration);
          }
          if (!stop.load(std::memory_order_relaxed)) {
            (void)w->Begin(disp);
          }
        });
    ASSERT_OK(neighbor->wait->Begin(neighbor->dispatcher.async_dispatcher()));
    neighbors.push_back(std::move(neighbor));
  }

  for (auto& neighbor : neighbors) {
    neighbor->started_completion.Wait();
  }

  libsync::Completion victim_completion;

  zx::event victim_event;
  std::unique_ptr<async::Wait> victim_wait;

  zx::interrupt victim_irq_object;
  std::unique_ptr<async::Irq> victim_irq;

  fdf::Arena arena(kTag);
  void* msg = arena.Allocate(kMsgSize);
  fdf::Channel victim_ch_write;
  fdf::Channel victim_ch_read;
  std::unique_ptr<fdf::ChannelRead> victim_channel_read;

  switch (victim_type) {
    case VictimType::kWait: {
      ASSERT_OK(zx::event::create(0, &victim_event));
      victim_wait = std::make_unique<async::Wait>(
          victim_event.get(), ZX_USER_SIGNAL_0, 0,
          [&](async_dispatcher_t*, async::Wait*, zx_status_t status, const zx_packet_signal_t*) {
            ASSERT_OK(status);
            ASSERT_OK(victim_event.signal(ZX_USER_SIGNAL_0, 0));
            victim_completion.Signal();
          });
      break;
    }
    case VictimType::kIrq: {
      ASSERT_OK(
          zx::interrupt::create(zx::resource(0), 0, ZX_INTERRUPT_VIRTUAL, &victim_irq_object));
      victim_irq = std::make_unique<async::Irq>(
          victim_irq_object.get(), 0,
          [&](async_dispatcher_t*, async::Irq*, zx_status_t status, const zx_packet_interrupt_t*) {
            ASSERT_OK(status);
            ASSERT_OK(victim_irq_object.ack());
            victim_completion.Signal();
          });
      ASSERT_OK(victim_irq->Begin(victim_dispatcher->async_dispatcher()));
      break;
    }
    case VictimType::kChannelRead: {
      zx::result<fdf::ChannelPair> channels = fdf::ChannelPair::Create(0);
      ASSERT_OK(channels.status_value());
      victim_ch_write = std::move(channels->end0);
      victim_ch_read = std::move(channels->end1);
      victim_channel_read = std::make_unique<fdf::ChannelRead>(
          victim_ch_read.get(), 0,
          [&](fdf_dispatcher_t*, fdf::ChannelRead* channel_read, zx_status_t status) {
            ASSERT_OK(status);
            fdf::UnownedChannel ch(channel_read->channel());
            zx::result<fdf::Channel::ReadReturn> read = ch->Read(0);
            ASSERT_OK(read.status_value());
            victim_completion.Signal();
          });
      break;
    }
  }

  while (state->KeepRunning()) {
    switch (victim_type) {
      case VictimType::kWait:
        ASSERT_OK(victim_wait->Begin(victim_dispatcher->async_dispatcher()));
        ASSERT_OK(victim_event.signal(0, ZX_USER_SIGNAL_0));
        break;
      case VictimType::kIrq:
        ASSERT_OK(victim_irq_object.trigger(0, zx::time_boot()));
        break;
      case VictimType::kChannelRead:
        ASSERT_OK(victim_channel_read->Begin(victim_dispatcher->get()));
        ASSERT_OK(victim_ch_write.Write(0, arena, msg, kMsgSize, cpp20::span<zx_handle_t>())
                      .status_value());
        break;
    }
    victim_completion.Wait();
    victim_completion.Reset();
  }

  stop.store(true, std::memory_order_relaxed);

  if (victim_type == VictimType::kIrq) {
    CancelIrqOnDispatcher(victim_dispatcher->async_dispatcher(), victim_irq.get());
  }

  for (auto& neighbor : neighbors) {
    neighbor->dispatcher.ShutdownAsync();
  }
  victim_dispatcher->ShutdownAsync();

  for (auto& neighbor : neighbors) {
    neighbor->shutdown_completion.Wait();
    neighbor->dispatcher.reset();
  }
  victim_shutdown.Wait();
  victim_dispatcher->reset();

  ResetRuntimeEnv();
  return true;
}

// ============================================================================
// 2. Main Port Wait Loop vs. Inlined Driver Transport Contention Benchmark
//    (Contention/PortLoopVsInlineCall)
// ============================================================================

enum class PortSource : uint8_t {
  kWait,
  kIrq,
  kTask,
};

// Measure client fdf::Channel::Call round-trip latency while the server dispatcher's main port
// wait loop is continuously woken by |port_source|.
bool ContentionClientCallTest(perftest::RepeatState* state, PortSource port_source,
                              std::string_view test_name) {
  ResetRuntimeEnv();

  std::atomic<bool> stop{false};

  uint32_t client_owner = 0;
  uint32_t server_owner = 0;
  libsync::Completion client_shutdown;
  libsync::Completion server_shutdown;

  zx::result<fdf::SynchronizedDispatcher> client_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &client_owner, fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "client",
          [&](fdf_dispatcher_t*) { client_shutdown.Signal(); });
  ASSERT_OK(client_dispatcher.status_value());

  zx::result<fdf::SynchronizedDispatcher> server_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &server_owner, {}, "server", [&](fdf_dispatcher_t*) { server_shutdown.Signal(); });
  ASSERT_OK(server_dispatcher.status_value());

  zx::result<fdf::ChannelPair> channels = fdf::ChannelPair::Create(0);
  ASSERT_OK(channels.status_value());
  fdf::Channel client_channel = std::move(channels->end0);
  fdf::Channel server_channel = std::move(channels->end1);

  std::unique_ptr<fdf::ChannelRead> server_read =
      RegisterChannelEchoReader(server_channel, server_dispatcher->get());

  libsync::Completion port_started;
  bool started_flag = false;

  zx::event wait_event;
  std::unique_ptr<async::Wait> port_wait;

  zx::interrupt irq_object;
  std::unique_ptr<async::Irq> port_irq;
  libsync::Completion irq_acked;
  std::thread irq_trigger_thread;

  std::function<void()> post_self_task;

  switch (port_source) {
    case PortSource::kWait: {
      ASSERT_OK(zx::event::create(0, &wait_event));
      ASSERT_OK(wait_event.signal(0, ZX_USER_SIGNAL_0));
      port_wait = std::make_unique<async::Wait>(
          wait_event.get(), ZX_USER_SIGNAL_0, 0,
          [&](async_dispatcher_t* disp, async::Wait* w, zx_status_t status,
              const zx_packet_signal_t*) {
            if (status != ZX_OK || stop.load(std::memory_order_relaxed)) {
              return;
            }
            if (!started_flag) {
              started_flag = true;
              port_started.Signal();
            }
            if (!stop.load(std::memory_order_relaxed)) {
              (void)w->Begin(disp);
            }
          });
      ASSERT_OK(port_wait->Begin(server_dispatcher->async_dispatcher()));
      break;
    }
    case PortSource::kIrq: {
      ASSERT_OK(zx::interrupt::create(zx::resource(0), 0, ZX_INTERRUPT_VIRTUAL, &irq_object));
      port_irq = std::make_unique<async::Irq>(
          irq_object.get(), 0,
          [&](async_dispatcher_t*, async::Irq*, zx_status_t status, const zx_packet_interrupt_t*) {
            if (status != ZX_OK) {
              return;
            }
            ASSERT_OK(irq_object.ack());
            if (!started_flag) {
              started_flag = true;
              port_started.Signal();
            }
            irq_acked.Signal();
          });
      ASSERT_OK(port_irq->Begin(server_dispatcher->async_dispatcher()));
      irq_trigger_thread = std::thread([&] {
        while (!stop.load(std::memory_order_relaxed)) {
          ASSERT_OK(irq_object.trigger(0, zx::time_boot()));
          irq_acked.Wait();
          irq_acked.Reset();
        }
      });
      break;
    }
    case PortSource::kTask: {
      post_self_task = [&] {
        if (stop.load(std::memory_order_relaxed)) {
          return;
        }
        (void)async::PostTask(server_dispatcher->async_dispatcher(), [&] {
          if (stop.load(std::memory_order_relaxed)) {
            return;
          }
          if (!started_flag) {
            started_flag = true;
            port_started.Signal();
          }
          post_self_task();
        });
      };
      post_self_task();
      break;
    }
  }

  port_started.Wait();

  fdf::Arena arena(kTag);
  void* msg = arena.Allocate(kMsgSize);

  ASSERT_OK(fdf_testing_set_default_dispatcher(client_dispatcher->get()));

  while (state->KeepRunning()) {
    zx::result<fdf::Channel::ReadReturn> reply = client_channel.Call(
        0, zx::time::infinite(), arena, msg, kMsgSize, cpp20::span<zx_handle_t>());
    ASSERT_OK(reply.status_value());
  }

  ASSERT_OK(fdf_testing_set_default_dispatcher(nullptr));

  LogServerDispatcherStats(test_name, server_dispatcher->get());

  stop.store(true, std::memory_order_relaxed);

  if (port_source == PortSource::kIrq) {
    irq_acked.Signal();
    irq_trigger_thread.join();

    CancelIrqOnDispatcher(server_dispatcher->async_dispatcher(), port_irq.get());
  }

  client_dispatcher->ShutdownAsync();
  server_dispatcher->ShutdownAsync();
  client_shutdown.Wait();
  server_shutdown.Wait();
  client_dispatcher->reset();
  server_dispatcher->reset();

  ResetRuntimeEnv();
  return true;
}

// Measure server port-callback wakeup latency (Wait, Irq, or Task) while a client dispatcher
// continuously issues synchronous fdf::Channel::Call requests to the server dispatcher.
bool ContentionServerPortWakeupTest(perftest::RepeatState* state, PortSource port_source) {
  ResetRuntimeEnv();

  std::atomic<bool> stop{false};

  uint32_t client_owner = 0;
  uint32_t server_owner = 0;
  libsync::Completion client_shutdown;
  libsync::Completion server_shutdown;

  zx::result<fdf::SynchronizedDispatcher> client_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &client_owner, fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "client",
          [&](fdf_dispatcher_t*) { client_shutdown.Signal(); });
  ASSERT_OK(client_dispatcher.status_value());

  zx::result<fdf::SynchronizedDispatcher> server_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &server_owner, {}, "server", [&](fdf_dispatcher_t*) { server_shutdown.Signal(); });
  ASSERT_OK(server_dispatcher.status_value());

  zx::result<fdf::ChannelPair> channels = fdf::ChannelPair::Create(0);
  ASSERT_OK(channels.status_value());
  fdf::Channel client_channel = std::move(channels->end0);
  fdf::Channel server_channel = std::move(channels->end1);

  std::unique_ptr<fdf::ChannelRead> server_read =
      RegisterChannelEchoReader(server_channel, server_dispatcher->get());

  libsync::Completion client_started;
  std::thread client_thread([&] {
    ASSERT_OK(fdf_testing_set_default_dispatcher(client_dispatcher->get()));
    fdf::Arena arena(kTag);
    void* msg = arena.Allocate(kMsgSize);

    bool first = true;
    while (!stop.load(std::memory_order_relaxed)) {
      zx::result<fdf::Channel::ReadReturn> reply = client_channel.Call(
          0, zx::time::infinite(), arena, msg, kMsgSize, cpp20::span<zx_handle_t>());
      ASSERT_OK(reply.status_value());
      if (first) {
        first = false;
        client_started.Signal();
      }
    }
    ASSERT_OK(fdf_testing_set_default_dispatcher(nullptr));
  });

  client_started.Wait();

  libsync::Completion port_completion;
  zx::event wait_event;
  std::unique_ptr<async::Wait> port_wait;
  zx::interrupt irq_object;
  std::unique_ptr<async::Irq> port_irq;

  if (port_source == PortSource::kWait) {
    ASSERT_OK(zx::event::create(0, &wait_event));
    port_wait = std::make_unique<async::Wait>(
        wait_event.get(), ZX_USER_SIGNAL_0, 0,
        [&](async_dispatcher_t*, async::Wait*, zx_status_t status, const zx_packet_signal_t*) {
          ASSERT_OK(status);
          ASSERT_OK(wait_event.signal(ZX_USER_SIGNAL_0, 0));
          port_completion.Signal();
        });
  } else if (port_source == PortSource::kIrq) {
    ASSERT_OK(zx::interrupt::create(zx::resource(0), 0, ZX_INTERRUPT_VIRTUAL, &irq_object));
    port_irq = std::make_unique<async::Irq>(
        irq_object.get(), 0,
        [&](async_dispatcher_t*, async::Irq*, zx_status_t status, const zx_packet_interrupt_t*) {
          ASSERT_OK(status);
          ASSERT_OK(irq_object.ack());
          port_completion.Signal();
        });
    ASSERT_OK(port_irq->Begin(server_dispatcher->async_dispatcher()));
  }

  while (state->KeepRunning()) {
    switch (port_source) {
      case PortSource::kWait:
        ASSERT_OK(port_wait->Begin(server_dispatcher->async_dispatcher()));
        ASSERT_OK(wait_event.signal(0, ZX_USER_SIGNAL_0));
        break;
      case PortSource::kIrq:
        ASSERT_OK(irq_object.trigger(0, zx::time_boot()));
        break;
      case PortSource::kTask:
        ASSERT_OK(async::PostTask(server_dispatcher->async_dispatcher(),
                                  [&] { port_completion.Signal(); }));
        break;
    }
    port_completion.Wait();
    port_completion.Reset();
  }

  stop.store(true, std::memory_order_relaxed);
  client_thread.join();

  if (port_source == PortSource::kIrq) {
    CancelIrqOnDispatcher(server_dispatcher->async_dispatcher(), port_irq.get());
  }

  client_dispatcher->ShutdownAsync();
  server_dispatcher->ShutdownAsync();
  client_shutdown.Wait();
  server_shutdown.Wait();
  client_dispatcher->reset();
  server_dispatcher->reset();

  ResetRuntimeEnv();
  return true;
}

// ============================================================================
// 3. Multi-Driver Colocated Pipeline & Fan-In Benchmark
//    (Topology/Pipeline and Topology/FanIn)
// ============================================================================

enum class PipelineMode : uint8_t {
  kInlined,
  kStaged,
};

// Measure end-to-end latency across a 3-hop colocated driver pipeline (D0 -> D1 -> D2 -> D3),
// comparing direct inlined fdf::Channel::Write calls against cross-thread staged async::PostTask
// forwarding at each hop.
bool PipelineTest(perftest::RepeatState* state, PipelineMode mode) {
  ResetRuntimeEnv();

  constexpr size_t kNumStages = 3;
  uint32_t caller_owner = 0;
  libsync::Completion caller_shutdown;
  zx::result<fdf::SynchronizedDispatcher> caller_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &caller_owner, {}, "stage-0", [&](fdf_dispatcher_t*) { caller_shutdown.Signal(); });
  ASSERT_OK(caller_dispatcher.status_value());

  struct Stage {
    uint32_t owner = 0;
    fdf::SynchronizedDispatcher dispatcher;
    libsync::Completion shutdown;
    fdf::Channel incoming;
    fdf::Channel outgoing;
    std::unique_ptr<fdf::ChannelRead> reader;
  };

  std::vector<std::unique_ptr<Stage>> stages;
  stages.reserve(kNumStages);
  for (size_t i = 0; i < kNumStages; ++i) {
    auto stage = std::make_unique<Stage>();
    std::string name = std::format("stage-{}", i + 1);
    zx::result<fdf::SynchronizedDispatcher> dispatcher =
        fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
            &stage->owner, {}, name,
            [s = stage.get()](fdf_dispatcher_t*) { s->shutdown.Signal(); });
    ASSERT_OK(dispatcher.status_value());
    stage->dispatcher = *std::move(dispatcher);
    stages.push_back(std::move(stage));
  }

  fdf::Channel entry_channel;
  for (size_t i = 0; i < kNumStages; ++i) {
    zx::result<fdf::ChannelPair> pair = fdf::ChannelPair::Create(0);
    ASSERT_OK(pair.status_value());
    if (i == 0) {
      entry_channel = std::move(pair->end0);
    } else {
      stages[i - 1]->outgoing = std::move(pair->end0);
    }
    stages[i]->incoming = std::move(pair->end1);
  }

  libsync::Completion pipeline_completion;

  for (size_t i = 0; i < kNumStages; ++i) {
    Stage* stage = stages[i].get();
    const bool is_last = (i == kNumStages - 1);
    stage->reader = std::make_unique<fdf::ChannelRead>(
        stage->incoming.get(), 0,
        [stage, is_last, mode, &pipeline_completion](
            fdf_dispatcher_t* disp, fdf::ChannelRead* channel_read, zx_status_t status) {
          if (status != ZX_OK) {
            return;
          }
          fdf::UnownedChannel ch(channel_read->channel());
          zx::result<fdf::Channel::ReadReturn> read = ch->Read(0);
          ASSERT_OK(read.status_value());
          ASSERT_OK(channel_read->Begin(disp));

          if (mode == PipelineMode::kInlined) {
            if (is_last) {
              pipeline_completion.Signal();
            } else {
              ASSERT_OK(stage->outgoing
                            .Write(0, read->arena, read->data, read->num_bytes,
                                   cpp20::span<zx_handle_t>())
                            .status_value());
            }
          } else {
            fdf::Arena arena = std::move(read->arena);
            void* data = read->data;
            uint32_t num_bytes = read->num_bytes;
            ASSERT_OK(async::PostTask(
                stage->dispatcher.async_dispatcher(),
                [stage, is_last, &pipeline_completion, arena = std::move(arena), data,
                 num_bytes]() mutable {
                  if (is_last) {
                    pipeline_completion.Signal();
                  } else {
                    ASSERT_OK(
                        stage->outgoing.Write(0, arena, data, num_bytes, cpp20::span<zx_handle_t>())
                            .status_value());
                  }
                }));
          }
        });
    ASSERT_OK(stage->reader->Begin(stage->dispatcher.get()));
  }

  fdf::Arena arena(kTag);
  void* msg = arena.Allocate(kMsgSize);

  ASSERT_OK(fdf_testing_set_default_dispatcher(caller_dispatcher->get()));

  while (state->KeepRunning()) {
    ASSERT_OK(
        entry_channel.Write(0, arena, msg, kMsgSize, cpp20::span<zx_handle_t>()).status_value());
    pipeline_completion.Wait();
    pipeline_completion.Reset();
  }

  ASSERT_OK(fdf_testing_set_default_dispatcher(nullptr));

  caller_dispatcher->ShutdownAsync();
  for (auto& stage : stages) {
    stage->dispatcher.ShutdownAsync();
  }
  caller_shutdown.Wait();
  caller_dispatcher->reset();
  for (auto& stage : stages) {
    stage->shutdown.Wait();
    stage->dispatcher.reset();
  }

  ResetRuntimeEnv();
  return true;
}

// Measure aggregate round-trip latency when N colocated client dispatchers concurrently issue
// batches of synchronous fdf::Channel::Call requests to a single synchronized server dispatcher.
bool FanInTest(perftest::RepeatState* state, uint32_t num_clients, std::string_view test_name) {
  ResetRuntimeEnv();

  constexpr uint32_t kCallsPerClient = 10;

  std::atomic<bool> stop{false};

  uint32_t server_owner = 0;
  libsync::Completion server_shutdown;
  zx::result<fdf::SynchronizedDispatcher> server_dispatcher =
      fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
          &server_owner, {}, "server", [&](fdf_dispatcher_t*) { server_shutdown.Signal(); });
  ASSERT_OK(server_dispatcher.status_value());

  struct FanInClient {
    uint32_t owner = 0;
    fdf::SynchronizedDispatcher dispatcher;
    libsync::Completion shutdown;
    fdf::Channel client_ch;
    fdf::Channel server_ch;
    std::unique_ptr<fdf::ChannelRead> server_read;
    libsync::Completion start_iter;
    libsync::Completion done_iter;
    std::thread worker;
  };

  std::vector<std::unique_ptr<FanInClient>> clients;
  clients.reserve(num_clients);

  for (uint32_t i = 0; i < num_clients; ++i) {
    auto client = std::make_unique<FanInClient>();
    std::string name = std::format("client-{}", i);
    zx::result<fdf::SynchronizedDispatcher> dispatcher =
        fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
            &client->owner, fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, name,
            [c = client.get()](fdf_dispatcher_t*) { c->shutdown.Signal(); });
    ASSERT_OK(dispatcher.status_value());
    client->dispatcher = *std::move(dispatcher);

    zx::result<fdf::ChannelPair> pair = fdf::ChannelPair::Create(0);
    ASSERT_OK(pair.status_value());
    client->client_ch = std::move(pair->end0);
    client->server_ch = std::move(pair->end1);

    client->server_read = RegisterChannelEchoReader(client->server_ch, server_dispatcher->get());

    client->worker = std::thread([c = client.get(), &stop] {
      ASSERT_OK(fdf_testing_set_default_dispatcher(c->dispatcher.get()));
      fdf::Arena arena(kTag);
      void* msg = arena.Allocate(kMsgSize);

      while (true) {
        c->start_iter.Wait();
        c->start_iter.Reset();
        if (stop.load(std::memory_order_relaxed)) {
          break;
        }
        for (uint32_t call = 0; call < kCallsPerClient; ++call) {
          zx::result<fdf::Channel::ReadReturn> reply = c->client_ch.Call(
              0, zx::time::infinite(), arena, msg, kMsgSize, cpp20::span<zx_handle_t>());
          ASSERT_OK(reply.status_value());
        }
        c->done_iter.Signal();
      }
      ASSERT_OK(fdf_testing_set_default_dispatcher(nullptr));
    });

    clients.push_back(std::move(client));
  }

  while (state->KeepRunning()) {
    for (auto& client : clients) {
      client->start_iter.Signal();
    }
    for (auto& client : clients) {
      client->done_iter.Wait();
      client->done_iter.Reset();
    }
  }

  LogServerDispatcherStats(test_name, server_dispatcher->get());

  stop.store(true, std::memory_order_relaxed);
  for (auto& client : clients) {
    client->start_iter.Signal();
  }
  for (auto& client : clients) {
    client->worker.join();
  }

  for (auto& client : clients) {
    client->dispatcher.ShutdownAsync();
  }
  server_dispatcher->ShutdownAsync();

  for (auto& client : clients) {
    client->shutdown.Wait();
    client->dispatcher.reset();
  }
  server_shutdown.Wait();
  server_dispatcher->reset();

  ResetRuntimeEnv();
  return true;
}

// ============================================================================
// 4. Dispatcher Lifecycle Overhead Benchmark
//    (Dispatcher/Lifecycle/CreateShutdownDestroy)
// ============================================================================

// Measure the time taken to create, asynchronously shut down, and destroy a batch of M
// managed synchronized dispatchers.
bool DispatcherLifecycleTest(perftest::RepeatState* state, uint32_t num_dispatchers) {
  ResetRuntimeEnv();

  std::vector<uint32_t> fake_drivers(num_dispatchers, 0);
  std::vector<fdf::SynchronizedDispatcher> dispatchers(num_dispatchers);
  std::vector<libsync::Completion> completions(num_dispatchers);

  while (state->KeepRunning()) {
    for (uint32_t i = 0; i < num_dispatchers; ++i) {
      zx::result<fdf::SynchronizedDispatcher> dispatcher =
          fdf_env::DispatcherBuilder::CreateSynchronizedWithOwner(
              &fake_drivers[i], {}, "lifecycle",
              [&completions, i](fdf_dispatcher_t*) { completions[i].Signal(); });
      ASSERT_OK(dispatcher.status_value());
      dispatchers[i] = *std::move(dispatcher);
    }

    for (uint32_t i = 0; i < num_dispatchers; ++i) {
      dispatchers[i].ShutdownAsync();
    }

    for (uint32_t i = 0; i < num_dispatchers; ++i) {
      completions[i].Wait();
      completions[i].Reset();
      dispatchers[i].reset();
    }
  }

  ResetRuntimeEnv();
  return true;
}

void RegisterTests() {
  // 1. MultiDispatcher/TailLatencyUnderLoad
  static constexpr std::pair<VictimType, std::string_view> kVictimTypes[] = {
      {VictimType::kWait, "Wait"},
      {VictimType::kIrq, "Irq"},
      {VictimType::kChannelRead, "ChannelRead"},
  };
  static constexpr uint32_t kNeighborCounts[] = {1, 4, 8};
  static constexpr uint32_t kBusyUs[] = {10, 100};

  for (const auto& [victim_type, victim_name] : kVictimTypes) {
    std::string baseline_name =
        std::format("MultiDispatcher/TailLatencyUnderLoad/{}/0neighbors", victim_name);
    perftest::RegisterTest(baseline_name.c_str(), TailLatencyUnderLoadTest, victim_type, 0u, 0u);

    for (uint32_t neighbors : kNeighborCounts) {
      for (uint32_t busy_us : kBusyUs) {
        std::string name = std::format("MultiDispatcher/TailLatencyUnderLoad/{}/{}neighbors/{}us",
                                       victim_name, neighbors, busy_us);
        perftest::RegisterTest(name.c_str(), TailLatencyUnderLoadTest, victim_type, neighbors,
                               busy_us);
      }
    }
  }

  // 2. Contention/PortLoopVsInlineCall
  static constexpr std::pair<PortSource, std::string_view> kPortSources[] = {
      {PortSource::kWait, "Wait"},
      {PortSource::kIrq, "Irq"},
      {PortSource::kTask, "Task"},
  };
  for (const auto& [source, source_name] : kPortSources) {
    std::string client_call_name =
        std::format("Contention/PortLoopVsInlineCall/ClientCall/{}", source_name);
    perftest::RegisterTest(client_call_name.c_str(), ContentionClientCallTest, source,
                           client_call_name);

    std::string wakeup_name =
        std::format("Contention/PortLoopVsInlineCall/ServerPortWakeup/{}", source_name);
    perftest::RegisterTest(wakeup_name.c_str(), ContentionServerPortWakeupTest, source);
  }

  // 3. Topology/Pipeline and Topology/FanIn
  perftest::RegisterTest("Topology/Pipeline/Inlined/3hops", PipelineTest, PipelineMode::kInlined);
  perftest::RegisterTest("Topology/Pipeline/Staged/3hops", PipelineTest, PipelineMode::kStaged);

  static constexpr uint32_t kFanInClients[] = {1, 2, 4};
  for (uint32_t num_clients : kFanInClients) {
    std::string fan_in_name = std::format("Topology/FanIn/{}clients", num_clients);
    perftest::RegisterTest(fan_in_name.c_str(), FanInTest, num_clients, fan_in_name);
  }

  // 4. Dispatcher/Lifecycle/CreateShutdownDestroy
  static constexpr uint32_t kLifecycleCounts[] = {1, 10};
  for (uint32_t count : kLifecycleCounts) {
    std::string lifecycle_name =
        std::format("Dispatcher/Lifecycle/CreateShutdownDestroy/{}dispatchers", count);
    perftest::RegisterTest(lifecycle_name.c_str(), DispatcherLifecycleTest, count);
  }
}

PERFTEST_CTOR(RegisterTests)

}  // namespace
