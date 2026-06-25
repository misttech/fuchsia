// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/kernel-registry.h>
#include <lib/power-management/pdev-power-level-controller.h>
#include <lib/power-management/port-power-level-controller.h>
#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/power-state.h>
#include <lib/unittest/unittest.h>
#include <zircon/errors.h>
#include <zircon/rights.h>

#include <pdev/power.h>
// TODO(https://fxbug.dev/415033686): Stop using `syscalls-next.h` on host.
#define FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <zircon/syscalls-next.h>
#undef FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <lib/stdcompat/utility.h>
#include <zircon/syscalls/port.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cstdint>

#include <arch/ops.h>
#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/cpu.h>
#include <kernel/deadline.h>
#include <kernel/event.h>
#include <kernel/thread.h>
#include <ktl/limits.h>
#include <ktl/optional.h>
#include <object/handle.h>
#include <object/port_dispatcher.h>

namespace {

using power_management::ControlInterface;
using power_management::EnergyModel;
using power_management::PDevPowerLevelController;
using power_management::PortPowerLevelController;
using power_management::PowerDomain;
using power_management::PowerDomainSet;
using power_management::PowerLevelController;
using power_management::PowerLevelUpdateRequest;

constexpr uint8_t kIdlePowerLevel = 0;
constexpr uint8_t kLowPowerLevel = 1;
constexpr uint8_t kMediumPowerLevel = 2;
constexpr uint8_t kHighPowerLevel = 3;
constexpr uint8_t kMaxPowerLevel = 4;

constexpr auto kPowerLevels = cpp20::to_array<zx_processor_power_level_t>({
    {
        .options = 0,
        .processing_rate = 0,                // 0%
        .power_coefficient_nw = 10'000'000,  // 10 mW
        .control_interface = cpp23::to_underlying(ControlInterface::kArmWfi),
        .control_argument = kIdlePowerLevel,
        .diagnostic_name = "wfi",
    },
    {
        .options = 0,
        .processing_rate = 250,               // 25%
        .power_coefficient_nw = 100'000'000,  // 100 mW
        .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
        .control_argument = kLowPowerLevel,
        .diagnostic_name = "OPP 1",
    },
    {
        .options = 0,
        .processing_rate = 500,               // 50%
        .power_coefficient_nw = 250'000'000,  // 250 mW
        .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
        .control_argument = kMediumPowerLevel,
        .diagnostic_name = "OPP 1",
    },
    {
        .options = 0,
        .processing_rate = 750,               // 75%
        .power_coefficient_nw = 400'000'000,  // 400 mW
        .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
        .control_argument = kHighPowerLevel,
        .diagnostic_name = "OPP 2",
    },
    {
        .options = 0,
        .processing_rate = 1000,              // 100%
        .power_coefficient_nw = 600'000'000,  // 600 mW
        .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
        .control_argument = kMaxPowerLevel,
        .diagnostic_name = "OPP 3",
    },
});

constexpr auto kTransitions = cpp20::to_array<zx_processor_power_level_transition_t>({{}});

bool PortPowerLevelControllerPost() {
  BEGIN_TEST;

  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights = PortDispatcher::default_rights();
  ASSERT_EQ(PortDispatcher::Create(0, &handle, &rights), ZX_OK);

  zx::result controller_result = PortPowerLevelController::Create(handle.dispatcher());
  ASSERT_TRUE(controller_result.is_ok());
  ASSERT_TRUE(controller_result->is_serving());
  ASSERT_EQ(handle.dispatcher()->current_handle_count(), 0u);

  // Fake being owned by a process. We only care about the handle count.
  auto decrese_handle_count = fit::defer([&]() { handle.dispatcher()->decrement_handle_count(); });
  handle.dispatcher()->increment_handle_count();

  PowerLevelUpdateRequest request = {
      .domain_id = 0xFEE7,
      .target_id = 0x0B00,
      .control = ControlInterface::kCpuDriver,
      .control_argument = 0xF00D,
      .options = 4321,
  };

  ASSERT_TRUE(controller_result->Post(request).is_ok());

  // Check a port with the domain id as key was queued.
  ASSERT_TRUE(handle.dispatcher()->CancelQueued(nullptr, request.domain_id));

  // Reset the controller's internal state since we manually canceled the packet
  // bypassing the standard Dequeue/Free pathway.
  controller_result->ResetForTest();

  END_TEST;
}

bool PortPowerLevelControllerStopServingOnZeroHandles() {
  BEGIN_TEST;

  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights = PortDispatcher::default_rights();
  ASSERT_EQ(PortDispatcher::Create(0, &handle, &rights), ZX_OK);

  zx::result controller_result = PortPowerLevelController::Create(handle.dispatcher());
  ASSERT_TRUE(controller_result.is_ok());
  ASSERT_TRUE(controller_result->is_serving());
  ASSERT_EQ(handle.dispatcher()->current_handle_count(), 0u);

  // Fake being owned by a process. We only care about the handle count.
  PowerLevelUpdateRequest request = {
      .domain_id = 0xFEE7,
      .target_id = 0x0B00,
      .control = ControlInterface::kCpuDriver,
      .control_argument = 0xF00D,
      .options = 4321,
  };

  ASSERT_TRUE(controller_result->Post(request).is_error());
  ASSERT_FALSE(controller_result->is_serving());

  END_TEST;
}

class FakePowerLevelController final : public PowerLevelController {
 public:
  FakePowerLevelController() : PowerLevelController(ControlInterface::kCpuDriver) {}

  zx::result<uint32_t> Post(const PowerLevelUpdateRequest& request) final {
    count_++;
    request_ = request;
    signal_posted_.Signal(ZX_OK);
    return zx::ok(0);
  }

  uint64_t id() const final { return 0; }

  zx_status_t Wait(Deadline deadline) {
    return signal_posted_.WaitDeadline(deadline.when(), Interruptible::Yes);
  }

  auto& request() { return request_; }
  size_t count() const { return count_; }

 private:
  AutounsignalEvent signal_posted_;
  ktl::atomic<size_t> count_ = 0;
  ktl::optional<PowerLevelUpdateRequest> request_ = ktl::nullopt;
};

bool SchedulerFlushesPendingControlRequests() {
  BEGIN_TEST;

  // Use the scheduler for the current CPU to test the power level control functionality. It doesn't
  // matter if the test thread remains on the same CPU, since power level requests can be initiated
  // from a different CPU than the target CPU.
  Scheduler& scheduler = percpu::GetCurrent().scheduler;

  zx::result energy_model = EnergyModel::Create(kPowerLevels, kTransitions);
  ASSERT_TRUE(energy_model.is_ok());

  fbl::AllocChecker ac;
  fbl::RefPtr controller = fbl::MakeRefCountedChecked<FakePowerLevelController>(&ac);
  ASSERT_TRUE(ac.check());

  fbl::RefPtr domain = fbl::MakeRefCountedChecked<PowerDomain>(
      &ac, 1, zx_cpu_set_t{.mask = {cpu_num_to_mask(scheduler.this_cpu())}},
      ktl::move(*energy_model), controller);
  ASSERT_TRUE(ac.check());

  PowerDomainSet domain_set = PowerDomainSet::CreateForTest(domain);

  // Set the current power domain, saving the previous domain to restore at the end of the test.
  const ktl::optional<uint8_t> restore_power_level = scheduler.GetActivePowerLevel();
  auto restore_domain =
      fit::defer([&, previous_domian_set = scheduler.ExchangePowerDomainSet(domain_set)] {
        scheduler.ExchangePowerDomainSet(previous_domian_set);
        if (restore_power_level.has_value()) {
          DEBUG_ASSERT(scheduler.UpdateActivePowerLevel(*restore_power_level).is_ok());
        }
      });
  ASSERT_EQ(domain.get(), scheduler.GetPowerDomainForTesting().get());
  ASSERT_OK(scheduler.UpdateActivePowerLevel(kMaxPowerLevel).status_value());

  // Request a transition to each active power level.
  for (size_t count = 0;
       uint8_t power_level : {kLowPowerLevel, kMediumPowerLevel, kHighPowerLevel, kMaxPowerLevel}) {
    scheduler.RequestPowerLevelForTesting(power_level);

    ASSERT_EQ(controller->Wait(Deadline::infinite()), ZX_OK);
    ASSERT_EQ(controller->count(), ++count);

    ASSERT_TRUE(controller->request());
    EXPECT_EQ(controller->request()->control, ControlInterface::kCpuDriver);
    EXPECT_EQ(controller->request()->control_argument, power_level);
    EXPECT_EQ(controller->request()->domain_id, domain->id());
    EXPECT_EQ(controller->request()->options, 0u);
    EXPECT_EQ(controller->request()->target_id, domain->id());

    // Simulate the controller acking the transition. Failing to ack the transition will cause this
    // loop to get stuck in FakePowerLevelController::Wait if the requested transition matches the
    // current power level (i.e. kMaxPowerLevel set above), since redundant requests are dropped.
    ASSERT_OK(scheduler.UpdateActivePowerLevel(power_level).status_value());

    controller->request().reset();
  }

  // Requesting the same power level as the current power level should be ignored.
  scheduler.RequestPowerLevelForTesting(kMaxPowerLevel);
  EXPECT_EQ(controller->Wait(Deadline::after_mono(zx_duration_from_sec(1))), ZX_ERR_TIMED_OUT);

  END_TEST;
}

bool SchedulerElidesPendingControlRequests() {
  BEGIN_TEST;

  // Use the scheduler for the current CPU to test the power level control functionality. It doesn't
  // matter if the test thread remains on the same CPU, since power level requests can be initiated
  // from a different CPU than the target CPU.
  Scheduler& scheduler = percpu::GetCurrent().scheduler;

  zx::result energy_model = EnergyModel::Create(kPowerLevels, kTransitions);
  ASSERT_TRUE(energy_model.is_ok());

  fbl::AllocChecker ac;
  fbl::RefPtr controller = fbl::MakeRefCountedChecked<FakePowerLevelController>(&ac);
  ASSERT_TRUE(ac.check());

  fbl::RefPtr domain = fbl::MakeRefCountedChecked<PowerDomain>(
      &ac, 1, zx_cpu_set_t{.mask = {cpu_num_to_mask(scheduler.this_cpu())}},
      ktl::move(*energy_model), controller);
  ASSERT_TRUE(ac.check());

  PowerDomainSet domain_set = PowerDomainSet::CreateForTest(domain);

  // Set the current power domain, saving the previous domain to restore at the end of the test.
  const ktl::optional<uint8_t> restore_power_level = scheduler.GetActivePowerLevel();
  auto restore_domain =
      fit::defer([&, previous_domian_set = scheduler.ExchangePowerDomainSet(domain_set)] {
        scheduler.ExchangePowerDomainSet(previous_domian_set);
        if (restore_power_level.has_value()) {
          DEBUG_ASSERT(scheduler.UpdateActivePowerLevel(*restore_power_level).is_ok());
        }
      });
  ASSERT_EQ(domain.get(), scheduler.GetPowerDomainForTesting().get());
  ASSERT_OK(scheduler.UpdateActivePowerLevel(kMaxPowerLevel).status_value());

  for (size_t i = 0; i < 150; ++i) {
    scheduler.RequestPowerLevelForTesting(kHighPowerLevel);
  }

  ASSERT_EQ(controller->Wait(Deadline::infinite()), ZX_OK);
  ASSERT_GE(controller->count(), 1u);

  ASSERT_TRUE(controller->request());
  EXPECT_EQ(controller->request()->control, ControlInterface::kCpuDriver);
  EXPECT_EQ(controller->request()->control_argument, kHighPowerLevel);
  EXPECT_EQ(controller->request()->domain_id, domain->id());
  EXPECT_EQ(controller->request()->options, 0u);
  EXPECT_EQ(controller->request()->target_id, domain->id());

  END_TEST;
}

bool SchedulerCanPendControlRequestsInIrqContext() {
  BEGIN_TEST;
  // Use the scheduler for the current CPU to test the power level control functionality. It doesn't
  // matter if the test thread remains on the same CPU, since power level requests can be initiated
  // from a different CPU than the target CPU.
  Scheduler& scheduler = percpu::GetCurrent().scheduler;

  zx::result energy_model = EnergyModel::Create(kPowerLevels, kTransitions);
  ASSERT_TRUE(energy_model.is_ok());

  fbl::AllocChecker ac;
  fbl::RefPtr controller = fbl::MakeRefCountedChecked<FakePowerLevelController>(&ac);
  ASSERT_TRUE(ac.check());

  fbl::RefPtr domain = fbl::MakeRefCountedChecked<PowerDomain>(
      &ac, 1, zx_cpu_set_t{.mask = {cpu_num_to_mask(scheduler.this_cpu())}},
      ktl::move(*energy_model), controller);
  ASSERT_TRUE(ac.check());

  PowerDomainSet domain_set = PowerDomainSet::CreateForTest(domain);

  // Set the current power domain, saving the previous domain to restore at the end of the test.
  const ktl::optional<uint8_t> restore_power_level = scheduler.GetActivePowerLevel();
  auto restore_domain =
      fit::defer([&, previous_domian_set = scheduler.ExchangePowerDomainSet(domain_set)] {
        scheduler.ExchangePowerDomainSet(previous_domian_set);
        if (restore_power_level.has_value()) {
          DEBUG_ASSERT(scheduler.UpdateActivePowerLevel(*restore_power_level).is_ok());
        }
      });
  ASSERT_EQ(domain.get(), scheduler.GetPowerDomainForTesting().get());
  ASSERT_OK(scheduler.UpdateActivePowerLevel(kMaxPowerLevel).status_value());

  auto timer_handler = +[](Timer* timer, zx_instant_mono_t now, void* arg) {
    Scheduler* scheduler = static_cast<Scheduler*>(arg);
    scheduler->RequestPowerLevelForTesting(kHighPowerLevel);
  };

  Timer timer;
  timer.Set(Deadline::infinite_past(), timer_handler, &scheduler);

  ASSERT_EQ(controller->Wait(Deadline::infinite()), ZX_OK);
  ASSERT_EQ(controller->count(), 1u);

  ASSERT_TRUE(controller->request());
  EXPECT_EQ(controller->request()->control, ControlInterface::kCpuDriver);
  EXPECT_EQ(controller->request()->control_argument, kHighPowerLevel);
  EXPECT_EQ(controller->request()->domain_id, domain->id());
  EXPECT_EQ(controller->request()->options, 0u);
  EXPECT_EQ(controller->request()->target_id, domain->id());

  END_TEST;
}

bool SchedulerCanPendControlRequestsAcrossCpus() {
  BEGIN_TEST;

  if (arch_max_num_cpus() < 2) {
    printf("Skipping test that requires more than one CPU.\n");
    END_TEST;
  }

  // Use the scheduler for the current CPU to test the power level control functionality. It doesn't
  // matter if the test thread remains on the same CPU, since power level requests can be initiated
  // from a different CPU than the target CPU.
  Scheduler& scheduler = percpu::GetCurrent().scheduler;

  zx::result energy_model = EnergyModel::Create(kPowerLevels, kTransitions);
  ASSERT_TRUE(energy_model.is_ok());

  fbl::AllocChecker ac;
  fbl::RefPtr controller = fbl::MakeRefCountedChecked<FakePowerLevelController>(&ac);
  ASSERT_TRUE(ac.check());

  fbl::RefPtr domain = fbl::MakeRefCountedChecked<PowerDomain>(
      &ac, 1, zx_cpu_set_t{.mask = {cpu_num_to_mask(scheduler.this_cpu())}},
      ktl::move(*energy_model), controller);
  ASSERT_TRUE(ac.check());

  PowerDomainSet domain_set = PowerDomainSet::CreateForTest(domain);

  // Set the current power domain, saving the previous domain to restore at the end of the test.
  const ktl::optional<uint8_t> restore_power_level = scheduler.GetActivePowerLevel();
  auto restore_domain =
      fit::defer([&, previous_domian_set = scheduler.ExchangePowerDomainSet(domain_set)] {
        scheduler.ExchangePowerDomainSet(previous_domian_set);
        if (restore_power_level.has_value()) {
          DEBUG_ASSERT(scheduler.UpdateActivePowerLevel(*restore_power_level).is_ok());
        }
      });
  ASSERT_EQ(domain.get(), scheduler.GetPowerDomainForTesting().get());
  ASSERT_OK(scheduler.UpdateActivePowerLevel(kMaxPowerLevel).status_value());

  // Move the test thread to a different CPU than the target scheduler serves.
  const cpu_mask_t temporary_affinity =
      Scheduler::PeekActiveMask() & ~cpu_num_to_mask(scheduler.this_cpu());
  auto restore_affinity =
      fit::defer([previous_affinity = Thread::Current::Get()->SetCpuAffinity(temporary_affinity)] {
        Thread::Current::Get()->SetCpuAffinity(previous_affinity);
      });
  ASSERT_NE(scheduler.this_cpu(), arch_curr_cpu_num());

  scheduler.RequestPowerLevelForTesting(kHighPowerLevel);

  ASSERT_EQ(controller->Wait(Deadline::infinite()), ZX_OK);
  ASSERT_EQ(controller->count(), 1u);

  ASSERT_TRUE(controller->request());
  EXPECT_EQ(controller->request()->control, ControlInterface::kCpuDriver);
  EXPECT_EQ(controller->request()->control_argument, kHighPowerLevel);
  EXPECT_EQ(controller->request()->domain_id, domain->id());
  EXPECT_EQ(controller->request()->options, 0u);
  EXPECT_EQ(controller->request()->target_id, domain->id());

  END_TEST;
}

ktl::optional<uint32_t> g_mock_opp_set_domain;
ktl::optional<uint64_t> g_mock_opp_set_opp;
zx_status_t g_mock_opp_set_status;
ktl::optional<uint32_t> g_mock_opp_get_domain;
zx::result<uint64_t> g_mock_opp_get_result;
zx::result<size_t> g_mock_opp_get_domain_count_result;

const pdev_power_ops kMockPowerOps = {
    .reboot = [](power_reboot_flags flags) -> zx_status_t { return ZX_OK; },
    .shutdown = []() -> zx_status_t { return ZX_OK; },
    .cpu_off = []() -> zx_status_t { return ZX_OK; },
    .cpu_on = [](uint64_t mpid, paddr_t entry, uint64_t context) -> zx_status_t { return ZX_OK; },
    .get_cpu_state = [](uint64_t hw_cpu_id) -> zx::result<power_cpu_state> {
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    },
    .opp_set = [](uint32_t domain_id, uint64_t opp) -> zx_status_t {
      g_mock_opp_set_domain = domain_id;
      g_mock_opp_set_opp = opp;
      return g_mock_opp_set_status;
    },
    .opp_get = [](uint32_t domain_id) -> zx::result<uint64_t> {
      g_mock_opp_get_domain = domain_id;
      return g_mock_opp_get_result;
    },
    .opp_get_domain_count = []() -> zx::result<size_t> {
      return g_mock_opp_get_domain_count_result;
    },
};

void ResetMockPowerOps() {
  g_mock_opp_set_domain = ktl::nullopt;
  g_mock_opp_set_opp = ktl::nullopt;
  g_mock_opp_set_status = ZX_OK;
  g_mock_opp_get_domain = ktl::nullopt;
  g_mock_opp_get_result = zx::ok(uint64_t{0});
  g_mock_opp_get_domain_count_result = zx::ok(size_t{0});
}

struct AutoMockPowerOps {
  AutoMockPowerOps() {
    ResetMockPowerOps();
    original_ops_ = pdev_swap_power_for_test(&kMockPowerOps);
  }
  ~AutoMockPowerOps() { pdev_swap_power_for_test(original_ops_); }

 private:
  const pdev_power_ops* original_ops_;
};

bool PDevPowerLevelControllerIsSupported() {
  BEGIN_TEST;

  AutoMockPowerOps mock;

  // The controller is not supported if retrieving the domain count fails.
  g_mock_opp_get_domain_count_result = zx::error(ZX_ERR_NOT_SUPPORTED);
  EXPECT_FALSE(PDevPowerLevelController::IsSupported());

  // The controller is not supported if the domain count is 0.
  g_mock_opp_get_domain_count_result = zx::ok(size_t{0});
  EXPECT_FALSE(PDevPowerLevelController::IsSupported());

  // The controller is supported if the domain count is greater than 0.
  g_mock_opp_get_domain_count_result = zx::ok(size_t{2});
  EXPECT_TRUE(PDevPowerLevelController::IsSupported());

  END_TEST;
}

bool PDevPowerLevelControllerPostValidation() {
  BEGIN_TEST;

  PDevPowerLevelController::ResetForTest();

  AutoMockPowerOps mock;
  g_mock_opp_get_domain_count_result = zx::ok(size_t{2});

  zx::result<fbl::RefPtr<PDevPowerLevelController>> controller_result =
      PDevPowerLevelController::Get(0);
  ASSERT_TRUE(controller_result.is_ok());
  fbl::RefPtr controller = ktl::move(controller_result.value());

  // Validate that interface requests not matching kCpuDriver are rejected.
  PowerLevelUpdateRequest request_wfi = {
      .domain_id = 0,
      .target_id = 0,
      .control = ControlInterface::kArmWfi,
      .control_argument = 1,
      .options = 0,
  };
  const zx::result<uint32_t> post_wfi_result = controller->Post(request_wfi);
  EXPECT_EQ(post_wfi_result.error_value(), ZX_ERR_NOT_SUPPORTED);

  // Validate that domain_id is checked against the total domain count
  // (domain_id 2 >= count 2).
  PowerLevelUpdateRequest request_oob = {
      .domain_id = 2,
      .target_id = 2,
      .control = ControlInterface::kCpuDriver,
      .control_argument = 1,
      .options = 0,
  };
  const zx::result<uint32_t> post_oob_result = controller->Post(request_oob);
  EXPECT_EQ(post_oob_result.error_value(), ZX_ERR_OUT_OF_RANGE);

  // Validate that errors from power_opp_get_domain_count are correctly
  // propagated on creation.
  PDevPowerLevelController::ResetForTest();
  g_mock_opp_get_domain_count_result = zx::error(ZX_ERR_BAD_STATE);
  const zx::result<fbl::RefPtr<PDevPowerLevelController>> get_error_result =
      PDevPowerLevelController::Get(0);
  EXPECT_EQ(get_error_result.error_value(), ZX_ERR_BAD_STATE);

  // Validate that domain_id is checked against domain count during Get().
  PDevPowerLevelController::ResetForTest();
  g_mock_opp_get_domain_count_result = zx::ok(size_t{2});
  const zx::result<fbl::RefPtr<PDevPowerLevelController>> get_oob_result =
      PDevPowerLevelController::Get(2);
  EXPECT_EQ(get_oob_result.error_value(), ZX_ERR_OUT_OF_RANGE);

  // Re-acquire the controller for the valid POST test.
  PDevPowerLevelController::ResetForTest();
  controller_result = PDevPowerLevelController::Get(0);
  ASSERT_TRUE(controller_result.is_ok());
  controller = ktl::move(controller_result.value());

  // Validate that a request passing all checks is allowed to proceed to the
  // registry update. Since domain 0 is not registered in this test registry,
  // UpdatePowerLevel returns ZX_ERR_NOT_FOUND, which verifies that all earlier
  // validation checks successfully passed.
  PowerLevelUpdateRequest request_valid = {
      .domain_id = 0,
      .target_id = 0,
      .control = ControlInterface::kCpuDriver,
      .control_argument = 1,
      .options = 0,
  };
  g_mock_opp_set_status = ZX_OK;
  const zx::result<uint32_t> post_valid_result = controller->Post(request_valid);
  EXPECT_EQ(post_valid_result.error_value(), ZX_ERR_NOT_FOUND);

  END_TEST;
}

bool PDevPowerLevelControllerGetPowerLevelValidation() {
  BEGIN_TEST;

  PDevPowerLevelController::ResetForTest();

  AutoMockPowerOps mock;
  g_mock_opp_get_domain_count_result = zx::ok(size_t{2});

  zx::result<fbl::RefPtr<PDevPowerLevelController>> controller_result =
      PDevPowerLevelController::Get(0);
  ASSERT_TRUE(controller_result.is_ok());
  fbl::RefPtr controller = ktl::move(controller_result.value());

  // Validate that domain_id is checked against the total domain count
  // (domain_id 2 >= count 2).
  const zx::result<uint64_t> current_level_oob_result = controller->GetCurrentPowerLevel(2);
  EXPECT_EQ(current_level_oob_result.error_value(), ZX_ERR_OUT_OF_RANGE);

  // Validate that a valid domain query returns the value fetched from the pdev
  // backend.
  g_mock_opp_get_result = zx::ok(uint64_t{42});
  zx::result<uint64_t> current_power_level_result = controller->GetCurrentPowerLevel(0);
  ASSERT_TRUE(current_power_level_result.is_ok());
  ASSERT_TRUE(g_mock_opp_get_domain.has_value());
  EXPECT_EQ(current_power_level_result.value(), uint64_t{42});
  EXPECT_EQ(g_mock_opp_get_domain.value(), 0u);

  END_TEST;
}

UNITTEST_START_TESTCASE(pm_controller)
UNITTEST("Port controller queues a packet.", PortPowerLevelControllerPost)
UNITTEST("Port controller stops serving when there are zero handles.",
         PortPowerLevelControllerStopServingOnZeroHandles)
UNITTEST("Scheduler flushes pending requests to the controller.",
         SchedulerFlushesPendingControlRequests)
UNITTEST("Scheduler elides multiple pending requests to the controller.",
         SchedulerElidesPendingControlRequests)
UNITTEST("Scheduler control requests may occur in IRQ context.",
         SchedulerCanPendControlRequestsInIrqContext)
UNITTEST("Scheduler control requests may pend from a different CPU.",
         SchedulerCanPendControlRequestsAcrossCpus)
UNITTEST("PDev controller supported.", PDevPowerLevelControllerIsSupported)
UNITTEST("PDev controller post validation.", PDevPowerLevelControllerPostValidation)
UNITTEST("PDev controller get power level validation.",
         PDevPowerLevelControllerGetPowerLevelValidation)
UNITTEST_END_TESTCASE(pm_controller, "pm_controller", "Kernel CPU power level controller tests.")

}  // namespace
