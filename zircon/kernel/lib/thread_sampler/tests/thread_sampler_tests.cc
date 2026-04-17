// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/fxt/serializer.h>
#include <lib/page/size.h>
#include <lib/thread_sampler/thread_sampler.h>
#include <lib/unittest/unittest.h>
#include <lib/zx/time.h>

#include <ktl/algorithm.h>
#include <ktl/limits.h>
#include <ktl/unique_ptr.h>
#include <vm/vm_aspace.h>

#include "kernel/mp.h"

#include <ktl/enforce.h>

namespace thread_sampler_tests {

// A test version of ThreadSampler which overrides functions
// for testing purposes.
class TestThreadSampler : public sampler::ThreadSampler {
 public:
  TestThreadSampler() = default;

  auto& get_per_cpu_state() { return per_cpu_state_; }
  auto& get_state() { return state_; }
  void set_state(sampler::SamplingState s) TA_NO_THREAD_SAFETY_ANALYSIS { SetState(s); }
  static auto get_lock() { return sampler::ThreadSampler::ThreadSamplerLock::Get(); }
  uint64_t get_buffer_ref_count() const {
    uint64_t state = state_.load(ktl::memory_order_relaxed);
    return (state & sampler::ThreadSampler::kBufferRefCountMask) >>
           sampler::ThreadSampler::kBufferRefCountShift;
  }

  void SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source, void* gregs) {
    ktl::optional<PerCpuStateRef> buffers = GetPerCpuState(arch_curr_cpu_num());
    if (!buffers) {
      return;
    }
    sampler::internal::PerCpuState& cpu_state = buffers->Get();

    constexpr size_t kMaxUserBacktraceSize = 64;
    vaddr_t bt[kMaxUserBacktraceSize]{};
    for (unsigned i = 0; i < kMaxUserBacktraceSize; ++i) {
      bt[i] = i;
    }

    constexpr fxt::StringRef<fxt::RefType::kId> empty_string{0};
    const fxt::ThreadRef current_thread{pid, tid};
    fxt::WriteLargeBlobRecordWithMetadata(&cpu_state, current_mono_ticks(), empty_string,
                                          empty_string, current_thread, bt,
                                          sizeof(uint64_t) * kMaxUserBacktraceSize);
  }

  static bool RepeatStartStopTest() {
    BEGIN_TEST;
    {
      // Construct a thread sampler state and initialize it
      zx_sampler_config_t config{
          .period = zx::msec(1).get(),
          .buffer_size = kPageSize,
      };
      {
        TestThreadSampler test_state;
        ASSERT_OK(test_state.SetUp(config).status_value());
        for (int i = 0; i < 10; i++) {
          ASSERT_OK(test_state.Start().status_value());
          ASSERT_OK(test_state.Stop().status_value());
        }
        ASSERT_OK(test_state.Destroy().status_value());
      }

      {
        TestThreadSampler test_state;
        for (int i = 0; i < 10; i++) {
          ASSERT_OK(test_state.SetUp(config).status_value());
          ASSERT_OK(test_state.Start().status_value());
          ASSERT_OK(test_state.Stop().status_value());
          ASSERT_OK(test_state.Destroy().status_value());
        }
      }

      {
        // In the case of the user closing the handle while the session is started, we'll get a
        // Start -> Destroy transition.
        TestThreadSampler test_state;
        for (int i = 0; i < 10; i++) {
          ASSERT_OK(test_state.SetUp(config).status_value());
          ASSERT_OK(test_state.Start().status_value());
          ASSERT_OK(test_state.Destroy().status_value());
        }
      }

      {
        // In the case of the user closing the handle without actually starting a session, we'll get
        // a Configured -> Destroy transition.
        TestThreadSampler test_state;
        for (int i = 0; i < 10; i++) {
          ASSERT_OK(test_state.SetUp(config).status_value());
          ASSERT_OK(test_state.Destroy().status_value());
        }
      }
    }

    END_TEST;
  }
  static bool WriteSampleTest() {
    BEGIN_TEST;
    {
      // Construct a thread sampler state and initialize it
      zx_sampler_config_t config{
          .period = zx::msec(1).get(),
          .buffer_size = kPageSize,
      };
      TestThreadSampler test_state{};
      ASSERT_OK(test_state.SetUp(config).status_value());

      ASSERT_OK(test_state.Start().status_value());

      zx_instant_mono_ticks_t before = current_mono_ticks();
      //  Write some fake samples to each buffer on each cpu
      mp_sync_exec(
          mp_ipi_target::ALL, 0,
          [](void* s) {
            auto test_thread_sampler = reinterpret_cast<TestThreadSampler*>(s);
            test_thread_sampler->SampleThread(arch_curr_cpu_num(), 1, GeneralRegsSource::None,
                                              nullptr);
          },
          &test_state);
      zx_instant_mono_ticks_t after = current_mono_ticks();
      ASSERT_OK(test_state.Stop().status_value());

      // We should now be able to read the records
      size_t num_cpus = arch_max_num_cpus();
      for (unsigned i = 0; i < num_cpus; ++i) {
        sampler::internal::PerCpuState& s = test_state.get_per_cpu_state()[i];

        // num_words = 64 backtrace + 1 large_header + 1 metadata + 1 ts + 1 inline pid + 1 inline
        // tid + 1 blob size = 70
        constexpr size_t num_words = 70;
        // We should see a large blob
        constexpr uint64_t large_blob_header =
            fxt::MakeLargeHeader(fxt::LargeRecordType::kBlob, fxt::WordSize(num_words));
        fxt::LargeBlobFields::BlobFormat::Make(ToUnderlyingType(fxt::LargeBlobFormat::kMetadata));
        uint64_t record[71];
        auto copy_fn = [&record](uint32_t offset,
                                 ktl::span<ktl::byte> data) mutable -> zx_status_t {
          ktl::ranges::copy(data, reinterpret_cast<ktl::byte*>(record) + offset);
          return ZX_OK;
        };
        zx::result<size_t> read_result = s.Read(copy_fn, sizeof(record));
        ASSERT_TRUE(read_result.is_ok());
        // We should only get the bytes of the record we wrote.
        ASSERT_EQ(*read_result, size_t{70 * sizeof(uint64_t)});

        EXPECT_EQ(large_blob_header, record[0]);
        // 0 arguments, inline thread ref, and empty name/category
        EXPECT_EQ(uint64_t{0}, record[1]);

        // timestamp
        EXPECT_GE(record[2], static_cast<uint64_t>(before));
        EXPECT_LE(record[2], static_cast<uint64_t>(after));

        // We wrote the cpu number as the pid
        EXPECT_EQ(i, record[3]);
        // And 1 as the tid
        EXPECT_EQ(uint64_t{1}, record[4]);
        // Blob size
        EXPECT_EQ(record[5], uint64_t{64} * sizeof(uint64_t));
        for (unsigned frame = 0; frame < 64; frame++) {
          EXPECT_EQ(record[6 + frame], frame);
        }
      }
    }

    END_TEST;
  }

  static bool StateChange() {
    BEGIN_TEST;
    {
      TestThreadSampler sampler;
      ASSERT_EQ(uint64_t{0}, sampler.get_state().load(ktl::memory_order_relaxed));
      zx_sampler_config_t config{
          .period = zx::msec(1).get(),
          .buffer_size = kPageSize,
      };
      ASSERT_OK(sampler.SetUp(config).status_value());
      ASSERT_EQ(sampler::SamplingState::Configured, sampler.State());
      ASSERT_TRUE(sampler.Start().is_ok());
      ASSERT_EQ(sampler::SamplingState::Running, sampler.State());
      {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(0);
        ASSERT_TRUE(ref.has_value());
        ASSERT_EQ(sampler::SamplingState::Running, sampler.State());
        uint64_t ref_count = sampler.get_buffer_ref_count();
        ASSERT_EQ(uint64_t{1}, ref_count);
        // Changing the state shouldn't change the ref count
        {
          Guard<Mutex> guard(TestThreadSampler::get_lock());
          sampler.set_state(sampler::SamplingState::Stopping);
        }
        ASSERT_EQ(sampler::SamplingState::Stopping, sampler.State());
        uint64_t ref_count2 = sampler.get_buffer_ref_count();
        ASSERT_EQ(uint64_t{1}, ref_count2);
        // Fix up the state after we manually modified it.
        {
          Guard<Mutex> guard(TestThreadSampler::get_lock());
          sampler.set_state(sampler::SamplingState::Running);
        }
      }

      ASSERT_OK(sampler.Destroy().status_value());
    }

    END_TEST;
  }

  static bool AcquireBuffers() {
    BEGIN_TEST;
    {
      TestThreadSampler sampler;
      // We shouldn't be able to get a buffer reference if we don't have buffers.
      for (cpu_num_t i = 0; i < arch_max_num_cpus() + 1; i++) {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(i);
        ASSERT_FALSE(ref.has_value());
      }
      // Construct a thread sampler state and initialize it
      zx_sampler_config_t config{
          .period = zx::msec(1).get(),
          .buffer_size = kPageSize,
      };
      ASSERT_OK(sampler.SetUp(config).status_value());

      // We shouldn't be able to get the buffers unless we're running.
      for (cpu_num_t i = 0; i < arch_max_num_cpus() + 1; i++) {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(i);
        ASSERT_FALSE(ref.has_value());
      }
      ASSERT_TRUE(sampler.Start().is_ok());

      for (cpu_num_t i = 0; i < arch_max_num_cpus(); i++) {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(i);
        ASSERT_TRUE(ref.has_value());
      }

      ktl::optional<PerCpuStateRef> bad_ref = sampler.GetPerCpuState(arch_max_num_cpus());
      ASSERT_FALSE(bad_ref.has_value());

      ASSERT_TRUE(sampler.Stop().is_ok());
      for (cpu_num_t i = 0; i < arch_max_num_cpus() + 1; i++) {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(i);
        ASSERT_FALSE(ref.has_value());
      }
      ASSERT_TRUE(sampler.Destroy().is_ok());
      for (cpu_num_t i = 0; i < arch_max_num_cpus() + 1; i++) {
        ktl::optional<PerCpuStateRef> ref = sampler.GetPerCpuState(i);
        ASSERT_FALSE(ref.has_value());
      }
    }

    END_TEST;
  }
};
}  // namespace thread_sampler_tests

UNITTEST_START_TESTCASE(thread_sampler_tests)
UNITTEST("init/start", thread_sampler_tests::TestThreadSampler::RepeatStartStopTest)
UNITTEST("read/write", thread_sampler_tests::TestThreadSampler::WriteSampleTest)
UNITTEST("state_change", thread_sampler_tests::TestThreadSampler::StateChange)
UNITTEST("acquire_buffers", thread_sampler_tests::TestThreadSampler::AcquireBuffers)
UNITTEST_END_TESTCASE(thread_sampler_tests, "thread_sampler", "Thread Sampler tests")
