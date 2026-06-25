// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/standalone-test/standalone.h>
#include <lib/zx/event.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <stdint.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>
#include <zircon/syscalls/resource.h>
#include <zircon/syscalls/system.h>
#include <zircon/syscalls/types.h>
#include <zircon/system/public/zircon/errors.h>
#include <zircon/system/public/zircon/syscalls-next.h>
#include <zircon/types.h>

#include <concepts>
#include <cstdint>
#include <cstdlib>
#include <limits>
#include <utility>

#include <zxtest/base/test.h>
#include <zxtest/cpp/assert.h>
#include <zxtest/zxtest.h>

#include "../needs-next.h"

NEEDS_NEXT_SYSCALL(zx_system_set_processor_power_domain);
NEEDS_NEXT_SYSCALL(zx_system_set_processor_power_state);

namespace {

constexpr uint64_t kForceUserDriver = ZX_SYSTEM_SET_PROCESSOR_POWER_DOMAIN_OPTION_FORCE_USER_DRIVER;

zx::result<> Unregister(uint32_t power_domain_id) {
  zx_processor_power_domain_t domain = {.domain_id = power_domain_id};
  return zx::make_result(
      zx_system_set_processor_power_domain(standalone::GetSystemResource()->get(), 0, &domain,
                                           ZX_HANDLE_INVALID, nullptr, 0, nullptr, 0));
}

auto Cleanup(uint32_t id) {
  return fit::defer([id] {
    if (Unregister(id).is_error()) {
      FAIL("Cleanup Failed.");
    }
  });
}

std::pair<std::vector<zx_processor_power_level_t>,
          std::vector<zx_processor_power_level_transition_t>>
GetModel() {
  std::vector<zx_processor_power_level_t> levels;
  std::vector<zx_processor_power_level_transition_t> transitions;
  levels = {
      zx_processor_power_level_t{
          .options = ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
          .processing_rate = 0,
          .power_coefficient_nw = 40'000'000,  // 40 mW
          .control_interface = ZX_PROCESSOR_POWER_CONTROL_ARM_WFI,
          .control_argument = 0,
          .diagnostic_name = "WFI",
      },
      zx_processor_power_level_t{
          .options = 0,
          .processing_rate = 2000,
          .power_coefficient_nw = 500'000'000,  // 500 mW
          .control_interface = ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER,
          .control_argument = 0x1234,
          .diagnostic_name = "MAX",
      },
  };

  transitions = {
      zx_processor_power_level_transition_t{
          .latency = 123,
          .energy_nj = 1234,
          .from = 0,
          .to = 1,
      },
      zx_processor_power_level_transition_t{
          .latency = 124,
          .energy_nj = 123,
          .from = 1,
          .to = 0,
      },
  };

  return std::make_pair(std::move(levels), std::move(transitions));
}

template <std::convertible_to<uint32_t>... Cpus>
zx_processor_power_domain_t MakeDomain(uint32_t domain_id, Cpus... cpus) {
  zx_processor_power_domain_t domain{.domain_id = domain_id};
  auto set_cpu = [&domain](uint32_t cpu_num) {
    size_t bucket = cpu_num / ZX_CPU_SET_BITS_PER_WORD;
    size_t bit = cpu_num % ZX_CPU_SET_BITS_PER_WORD;
    domain.cpus.mask[bucket] |= 1ull << bit;
  };

  (set_cpu(cpus), ...);
  return domain;
}

// Check if we are running in a single core.
zx_processor_power_domain_t GetDomainWithDefaultCpus(uint32_t domain_id) {
  size_t num_cpus = zx_system_get_num_cpus();
  if (num_cpus >= 2) {
    return MakeDomain(domain_id, 0, 1);
  }
  return MakeDomain(domain_id, 0);
}

TEST(SetPowerDomainTest, ValidPowerDomainSucceeds) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup = Cleanup(domain.domain_id);
}

TEST(SetPowerDomainTest, SameIdUpdates) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup = Cleanup(domain.domain_id);

  auto domain_updated = MakeDomain(123, 0);
  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain_updated,
                                                 p.get(), levels.data(), levels.size(),
                                                 transitions.data(), transitions.size()));
}

TEST(SetPowerDomainTest, UnregisterDomain) {
  // Smoke test that unregister is actually working.
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));

  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));

  zx_processor_power_domain_t domain_updated{.cpus = {}, .domain_id = domain.domain_id};

  // Successful registering.
  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain_updated,
                                                 p.get(), levels.data(), levels.size(),
                                                 transitions.data(), transitions.size()));

  // Unregistring an unexistant domain id is `ZX_ERR_NOT_FOUND`.
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), 0, &domain_updated,
                                                     ZX_HANDLE_INVALID, nullptr, 0, nullptr, 0),
                ZX_ERR_NOT_FOUND);

  domain_updated.domain_id = 1234587;
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), 0, &domain_updated,
                                                     ZX_HANDLE_INVALID, nullptr, 0, nullptr, 0),
                ZX_ERR_NOT_FOUND);
}

TEST(SetPowerDomainTest, RegisterDomainWithInvalidPortIsError) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(
                    rsrc->get(), kForceUserDriver, &domain, ZX_HANDLE_INVALID, levels.data(),
                    levels.size(), transitions.data(), transitions.size()),
                ZX_ERR_BAD_HANDLE);
}

TEST(SetPowerDomainTest, RegisterDomainWithPortWithoutWriteRights) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  zx::port p2;
  ASSERT_OK(p.duplicate(ZX_RIGHT_READ, &p2));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p2.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_ACCESS_DENIED);
}

TEST(SetPowerDomainTest, RegisterDomainWithPortWithoutReadRights) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  zx::port p2;
  ASSERT_OK(p.duplicate(ZX_RIGHT_WRITE, &p2));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p2.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_ACCESS_DENIED);
}

TEST(SetPowerDomainTest, RegisterDomainWithWrongHandleType) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::event e;
  ASSERT_OK(zx::event::create(0, &e));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     e.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_WRONG_TYPE);
}

TEST(SetPowerDomainTest, RegisterDomainWithNonEmptyMaskWithInvalidLevels) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = MakeDomain(123, 0, 1);

  ASSERT_STATUS(
      zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(), nullptr,
                                           0, transitions.data(), transitions.size()),
      ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, RegisterDomainWithNonEmptyMaskWithEmptyTransitions) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), nullptr, 0));
}

TEST(SetPowerDomainTest, RegisterDomainWithNonEmptyMaskWithTooManyTransitions) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  transitions.resize(levels.size() * levels.size() + 1);
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, RegisterDomainWithCpuOutOfBounds) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = MakeDomain(123, 0, zx_system_get_num_cpus());
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, RegisterDomainWithTransitionReferencingLevelsOutOfBounds) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  transitions[0].from = static_cast<uint8_t>(levels.size());
  transitions[1].to = static_cast<uint8_t>(levels.size() + 1);
  auto domain = GetDomainWithDefaultCpus(123);
  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, RegisterDomainWithNonEmptyMaskWithUnknownControlInterface) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  levels[0].control_interface = std::numeric_limits<zx_processor_power_control_t>::max();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_STATUS(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain,
                                                     p.get(), levels.data(), levels.size(),
                                                     transitions.data(), transitions.size()),
                ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, BadLevelPointer) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_STATUS(
      zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                           reinterpret_cast<zx_processor_power_level_t*>(0x01),
                                           levels.size(), transitions.data(), transitions.size()),
      ZX_ERR_INVALID_ARGS);
}

TEST(SetPowerDomainTest, BadTransitionPointer) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_STATUS(
      zx_system_set_processor_power_domain(
          rsrc->get(), kForceUserDriver, &domain, p.get(), levels.data(), levels.size(),
          reinterpret_cast<zx_processor_power_level_transition_t*>(0x01), transitions.size()),
      ZX_ERR_INVALID_ARGS);
}

// Regression test for https://fxbug.dev/502355555 / https://fxbug.dev/517175842.
// This test reproduces a race condition between setting performance limits
// (triggering power transitions) and re-registering power domains.
TEST(SetPowerDomainTest, PerformanceLimitRegisterRacePanicStressTest) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);

  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());

  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));

  std::atomic<bool> stop{false};
  std::atomic<int> packets_received{0};

  auto wait_thread_func = [&]() {
    while (!stop.load()) {
      zx_port_packet_t packet;
      zx_status_t status = p.wait(zx::deadline_after(zx::msec(10)), &packet);
      if (status == ZX_OK) {
        packets_received.fetch_add(1);
      }
    }
  };

  std::thread wait_threads[4];
  for (int i = 0; i < 4; ++i) {
    wait_threads[i] = std::thread(wait_thread_func);
  }

  size_t num_cpus = zx_system_get_num_cpus();
  zx_processor_power_domain_t domain = {};
  domain.cpus.mask[0] = (1ull << num_cpus) - 1;
  domain.domain_id = 100;

  std::vector<zx_processor_power_level_t> levels(2);
  levels[0].options = 0;
  levels[0].processing_rate = 500;
  levels[0].power_coefficient_nw = 100;
  levels[0].control_interface = ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER;
  levels[0].control_argument = 0;
  strcpy(levels[0].diagnostic_name, "Level 0");

  levels[1].options = 0;
  levels[1].processing_rate = 1000;
  levels[1].power_coefficient_nw = 200;
  levels[1].control_interface = ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER;
  levels[1].control_argument = 1;
  strcpy(levels[1].diagnostic_name, "Level 1");

  std::vector<zx_processor_power_level_transition_t> transitions(2);
  transitions[0].from = 0;
  transitions[0].to = 1;
  transitions[0].latency = 100;
  transitions[0].energy_nj = 10;
  transitions[1].from = 1;
  transitions[1].to = 0;
  transitions[1].latency = 100;
  transitions[1].energy_nj = 10;

  auto racer_thread_func = [&]() {
    while (!stop.load()) {
      // 1. Register domain.
      zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                           levels.data(), 2, transitions.data(), 2);

      // 2. Set initial state.
      zx_processor_power_state_t state = {};
      state.domain_id = 100;
      state.control_interface = ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER;
      state.control_argument = 1;
      zx_system_set_processor_power_state(p.get(), &state);

      // 3. Trigger transition by changing limits on all CPUs.
      for (size_t cpu = 0; cpu < num_cpus; cpu++) {
        zx_cpu_perf_limit_t limit = {};
        limit.logical_cpu_number = static_cast<uint32_t>(cpu);
        limit.limit_type = ZX_CPU_PERF_LIMIT_TYPE_RATE;
        limit.min = 0;
        limit.max = 500;  // Force Level 0
        zx_system_set_performance_info(rsrc->get(), ZX_CPU_PERF_LIMIT, &limit, 1);
      }

      std::this_thread::yield();

      // 4. Tight race: Re-register domain.
      zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                           levels.data(), 2, transitions.data(), 2);

      // Reset limits.
      for (size_t cpu = 0; cpu < num_cpus; cpu++) {
        zx_cpu_perf_limit_t limit = {};
        limit.logical_cpu_number = static_cast<uint32_t>(cpu);
        limit.limit_type = ZX_CPU_PERF_LIMIT_TYPE_RATE;
        limit.min = 0;
        limit.max = 1000;
        zx_system_set_performance_info(rsrc->get(), ZX_CPU_PERF_LIMIT, &limit, 1);
      }
    }
  };

  std::thread racer_threads[4];
  for (int i = 0; i < 4; ++i) {
    racer_threads[i] = std::thread(racer_thread_func);
  }

  zx_nanosleep(zx_deadline_after(ZX_SEC(10)));
  stop.store(true);

  for (auto& t : racer_threads) {
    t.join();
  }
  for (auto& t : wait_threads) {
    t.join();
  }
}

class SetPowerStateTest : public zxtest::Test {
 public:
  void SetUp() final {
    NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
    ASSERT_OK(zx::port::create(0, &port_));
    auto rsrc = standalone::GetSystemResource();
    ASSERT_TRUE(rsrc->is_valid());
    auto [levels, transitions] = GetModel();
    levels_ = std::move(levels);
    transitions_ = std::move(transitions);
    domain_info_ = GetDomainWithDefaultCpus(123);

    // Override in-kernel CPU driver control to enable testing the userspace
    // control path.
    const uint64_t options = ZX_SYSTEM_SET_PROCESSOR_POWER_DOMAIN_OPTION_FORCE_USER_DRIVER;

    ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), options, &domain_info_, port_.get(),
                                                   levels_.data(), levels_.size(),
                                                   transitions_.data(), transitions_.size()));
    cleanup_ = true;
  }
  void TearDown() final {
    if (cleanup_) {
      Cleanup(domain_info_.domain_id);
    }
  }

 protected:
  zx::port port_;
  std::vector<zx_processor_power_level_t> levels_;
  std::vector<zx_processor_power_level_transition_t> transitions_;
  zx_processor_power_domain_t domain_info_;

 private:
  bool cleanup_ = false;
};

TEST_F(SetPowerStateTest, UpdateActivePowerLevel) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);
  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[1].control_interface,
      .control_argument = levels_[1].control_argument,
  };

  ASSERT_OK(zx_system_set_processor_power_state(port_.get(), &pstate));
}

TEST_F(SetPowerStateTest, UpdateIdlePowerLevel) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);
  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[0].control_interface,
      .control_argument = levels_[0].control_argument,
  };

  ASSERT_STATUS(zx_system_set_processor_power_state(port_.get(), &pstate), ZX_ERR_OUT_OF_RANGE);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelWithWrongPort) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[1].control_interface,
      .control_argument = levels_[1].control_argument,
  };

  zx::port p2;
  ASSERT_OK(zx::port::create(0, &p2));
  ASSERT_STATUS(zx_system_set_processor_power_state(p2.get(), &pstate), ZX_ERR_ACCESS_DENIED);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelUnknownDomain) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id + 1,
      .control_interface = levels_[1].control_interface,
      .control_argument = levels_[1].control_argument,
  };

  ASSERT_STATUS(zx_system_set_processor_power_state(port_.get(), &pstate), ZX_ERR_NOT_FOUND);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelUnknownControlArgument) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[1].control_interface,
      .control_argument = levels_[1].control_argument + 0xDEAD,
  };

  ASSERT_STATUS(zx_system_set_processor_power_state(port_.get(), &pstate), ZX_ERR_NOT_FOUND);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelUnknownControlInterface) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[1].control_interface + 0xDEAD,
      .control_argument = levels_[1].control_argument,
  };

  ASSERT_STATUS(zx_system_set_processor_power_state(port_.get(), &pstate), ZX_ERR_NOT_FOUND);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelBadBuffer) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  ASSERT_STATUS(zx_system_set_processor_power_state(
                    port_.get(), reinterpret_cast<zx_processor_power_state_t*>(0x01)),
                ZX_ERR_INVALID_ARGS);
}

TEST_F(SetPowerStateTest, UpdatePowerLevelWithPortWithoutRead) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_state);

  zx_processor_power_state_t pstate = {
      .domain_id = domain_info_.domain_id,
      .control_interface = levels_[1].control_interface,
      .control_argument = levels_[1].control_argument,
  };

  zx::port p2;
  ASSERT_OK(port_.duplicate(ZX_DEFAULT_PORT_RIGHTS & ~ZX_RIGHT_READ, &p2));
  ASSERT_STATUS(zx_system_set_processor_power_state(p2.get(), &pstate), ZX_ERR_ACCESS_DENIED);
}

zx::resource GetInfoResource() {
  auto rsrc = standalone::GetSystemResource();
  auto info_rsrc = standalone::GetSystemResourceWithBase(rsrc, ZX_RSRC_SYSTEM_INFO_BASE);
  ZX_ASSERT(info_rsrc.is_ok());
  return std::move(info_rsrc).value();
}

TEST(GetPowerDomainInfo, NoDomainRegistered) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  auto info_rsrc = GetInfoResource();
  size_t actual = 12345;
  size_t available = 12345;
  ASSERT_OK(
      zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, nullptr, 0, &actual, &available));
  EXPECT_EQ(actual, 0);
  EXPECT_EQ(available, 0);

  // With buffer.
  std::array<zx_power_domain_info_t, 10> buff = {};
  ASSERT_OK(zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, buff.data(),
                               buff.size() * sizeof(zx_power_domain_info_t), &actual, &available));
  EXPECT_EQ(actual, 0);
  EXPECT_EQ(available, 0);
}

TEST(GetPowerDomainInfo, OneDomainRegistered) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);

  auto info_rsrc = GetInfoResource();
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup = Cleanup(domain.domain_id);

  size_t actual = 12345;
  size_t available = 12345;
  ASSERT_OK(
      zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, nullptr, 0, &actual, &available));
  EXPECT_EQ(actual, 0);
  EXPECT_EQ(available, 1);

  // With Bigger buffer.
  std::array<zx_power_domain_info_t, 10> buff = {};
  ASSERT_OK(zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, buff.data(),
                               buff.size() * sizeof(zx_power_domain_info_t), &actual, &available));
  EXPECT_EQ(available, 1);
  ASSERT_EQ(actual, 1);

  auto& reg_domain = buff[0];
  EXPECT_EQ(reg_domain.domain_id, domain.domain_id);
  EXPECT_TRUE(memcmp(reg_domain.cpus.mask, domain.cpus.mask, sizeof(domain.cpus.mask)) == 0);
  EXPECT_EQ(reg_domain.idle_power_levels, 1);
  EXPECT_EQ(reg_domain.active_power_levels, 1);

  // With smaller buffer.
  ASSERT_OK(zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, buff.data(), 0, &actual,
                               &available));
  EXPECT_EQ(available, 1);
  ASSERT_EQ(actual, 0);
}

TEST(GetPowerDomainInfo, MultipleDomainRegistered) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);

  if (zx_system_get_num_cpus() < 2) {
    ZXTEST_SKIP("Require at least 2 CPUs.\n");
  }

  auto info_rsrc = GetInfoResource();
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = MakeDomain(123, 0);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup = Cleanup(domain.domain_id);

  auto domain_2 = MakeDomain(1234, 1);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain_2, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup_2 = Cleanup(domain_2.domain_id);

  size_t actual = 12345;
  size_t available = 12345;
  ASSERT_OK(
      zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, nullptr, 0, &actual, &available));
  EXPECT_EQ(actual, 0);
  EXPECT_EQ(available, 2);

  auto check_domain = [&](zx_power_domain_info_t& domain_info) {
    if (domain_info.domain_id == domain.domain_id) {
      EXPECT_EQ(domain_info.domain_id, domain.domain_id);
      EXPECT_TRUE(memcmp(domain_info.cpus.mask, domain.cpus.mask, sizeof(domain.cpus.mask)) == 0);
      EXPECT_EQ(domain_info.idle_power_levels, 1);
      EXPECT_EQ(domain_info.active_power_levels, 1);
      return;
    }
    if (domain_info.domain_id == domain_2.domain_id) {
      EXPECT_EQ(domain_info.domain_id, domain_2.domain_id);
      EXPECT_TRUE(memcmp(domain_info.cpus.mask, domain_2.cpus.mask, sizeof(domain_2.cpus.mask)) ==
                  0);
      EXPECT_EQ(domain_info.idle_power_levels, 1);
      EXPECT_EQ(domain_info.active_power_levels, 1);
      return;
    }
    FAIL("Unknown Power Domain ID: %zu\n", static_cast<size_t>(domain_info.domain_id));
  };

  // With bigger buffer.
  std::array<zx_power_domain_info_t, 10> buff = {};
  ASSERT_OK(zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, buff.data(),
                               buff.size() * sizeof(zx_power_domain_info_t), &actual, &available));
  EXPECT_EQ(available, 2);
  ASSERT_EQ(actual, 2);

  check_domain(buff[0]);
  check_domain(buff[1]);

  // Smaller buffer
  ASSERT_OK(zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, buff.data(),
                               sizeof(zx_power_domain_info_t), &actual, &available));
  EXPECT_EQ(available, 2);
  ASSERT_EQ(actual, 1);

  check_domain(buff[0]);
}

TEST(GetPowerDomainInfo, BadHandle) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  size_t actual = 12345;
  size_t available = 12345;

  // Invalid.
  ASSERT_STATUS(
      zx_object_get_info(ZX_HANDLE_INVALID, ZX_INFO_POWER_DOMAINS, nullptr, 0, &actual, &available),
      ZX_ERR_BAD_HANDLE);

  // System resource but wrong base.
  auto sys_rsrc = standalone::GetSystemResource();
  auto wrong_rsrc_base = standalone::GetSystemResourceWithBase(sys_rsrc, ZX_RSRC_SYSTEM_CPU_BASE);
  ASSERT_OK(wrong_rsrc_base);
  ASSERT_STATUS(zx_object_get_info(wrong_rsrc_base->get(), ZX_INFO_POWER_DOMAINS, nullptr, 0,
                                   &actual, &available),
                ZX_ERR_OUT_OF_RANGE);

  zx::event e;
  ASSERT_OK(zx::event::create(0, &e));

  // Wrong type
  ASSERT_STATUS(zx_object_get_info(e.get(), ZX_INFO_POWER_DOMAINS, nullptr, 0, &actual, &available),
                ZX_ERR_WRONG_TYPE);
}

TEST(GetPowerDomainInfo, BadBuffer) {
  NEEDS_NEXT_SKIP(zx_system_set_processor_power_domain);
  auto info_rsrc = GetInfoResource();
  zx::port p;
  ASSERT_OK(zx::port::create(0, &p));
  auto rsrc = standalone::GetSystemResource();
  ASSERT_TRUE(rsrc->is_valid());
  auto [levels, transitions] = GetModel();
  auto domain = GetDomainWithDefaultCpus(123);

  ASSERT_OK(zx_system_set_processor_power_domain(rsrc->get(), kForceUserDriver, &domain, p.get(),
                                                 levels.data(), levels.size(), transitions.data(),
                                                 transitions.size()));
  auto cleanup = Cleanup(domain.domain_id);

  size_t actual = 12345;
  size_t available = 12345;

  // Bad buffer ptr.
  ASSERT_STATUS(
      zx_object_get_info(info_rsrc.get(), ZX_INFO_POWER_DOMAINS, reinterpret_cast<void*>(0x01),
                         sizeof(zx_power_domain_info_t), &actual, &available),
      ZX_ERR_INVALID_ARGS);
}

}  // namespace
