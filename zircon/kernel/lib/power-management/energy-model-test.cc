// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/energy-model.h"

#include <zircon/errors.h>
// TODO(https://fxbug.dev/415033686): Stop using `syscalls-next.h` on host.
#define FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <zircon/syscalls-next.h>
#undef FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <lib/stdcompat/utility.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <algorithm>
#include <array>
#include <cstddef>

#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "test-helper.h"

namespace {

using ffl::FromRatio;
using power_management::ControlInterface;
using power_management::EnergyModel;
using power_management::PowerDomain;
using power_management::PowerDomainRegistry;
using power_management::PowerDomainSet;
using power_management::PowerLevel;
using power_management::PowerLevelTransition;
using power_management::ProcessingRate;
using power_management::Utilization;

// Utility to test whether the given ranged object contains the given element.
template <typename Ranged, typename Element>
bool InRange(const Ranged& ranged, const Element& element) {
  return std::ranges::find(ranged, element) != std::cend(ranged);
}

ProcessingRate ToProcessingRate(uint64_t unscaled) { return FromRatio<uint64_t>(unscaled, 1000); }

TEST(PowerLevelTest, Ctor) {
  constexpr zx_processor_power_level_t kLevel = {
      .options = 0,
      .processing_rate = 456,
      .power_coefficient_nw = 789,
      .control_interface = cpp23::to_underlying(ControlInterface::kArmPsci),
      .control_argument = 12345,
      .diagnostic_name = "foobar one two three",
  };

  PowerLevel level(0, kLevel);

  EXPECT_EQ(level.level(), 0);
  EXPECT_EQ(level.processing_rate(), ToProcessingRate(kLevel.processing_rate));
  EXPECT_EQ(level.power_coefficient_nw(), kLevel.power_coefficient_nw);
  EXPECT_EQ(level.control(), static_cast<ControlInterface>(kLevel.control_interface));
  EXPECT_EQ(level.control_argument(), kLevel.control_argument);
  EXPECT_EQ(level.type(), PowerLevel::Type::kActive);
  EXPECT_EQ(level.name(), std::string_view(kLevel.diagnostic_name));
  EXPECT_TRUE(level.TargetsPowerDomain());
  EXPECT_FALSE(level.TargetsCpus());
}

TEST(PowerLevelTest, Ctor2) {
  constexpr zx_processor_power_level_t kLevel = {
      .options = ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
      .processing_rate = 123,
      .power_coefficient_nw = 789,
      .control_interface = cpp23::to_underlying(ControlInterface::kArmPsci),
      .control_argument = 12345,
      .diagnostic_name = "foobar one two three",
  };

  PowerLevel level(123, kLevel);

  EXPECT_EQ(level.level(), 123);
  EXPECT_EQ(level.processing_rate(), ToProcessingRate(kLevel.processing_rate));
  EXPECT_EQ(level.power_coefficient_nw(), kLevel.power_coefficient_nw);
  EXPECT_EQ(level.control(), static_cast<ControlInterface>(kLevel.control_interface));
  EXPECT_EQ(level.control_argument(), kLevel.control_argument);
  EXPECT_EQ(level.type(), PowerLevel::Type::kActive);
  EXPECT_EQ(level.name(), std::string_view(kLevel.diagnostic_name));
  EXPECT_FALSE(level.TargetsPowerDomain());
  EXPECT_TRUE(level.TargetsCpus());
}

TEST(PowerLevelTransitionTest, Ctor) {
  static constexpr zx_processor_power_level_transition_t kTransition = {
      .latency = 456,
      .energy_nj = 1234,
      .from = 0,
      .to = 1,
  };

  PowerLevelTransition transition(kTransition);

  EXPECT_EQ(transition.latency(), zx_duration_from_nsec(kTransition.latency));
  EXPECT_EQ(transition.energy_cost_nj(), kTransition.energy_nj);
}

TEST(PowerModelTest, Create) {
  static constexpr auto kPowerLevels = std::to_array<zx_processor_power_level_t>({
      {
          .options = 0,
          .processing_rate = 0,
          .power_coefficient_nw = 1,
          .control_interface = cpp23::to_underlying(ControlInterface::kArmPsci),
          .control_argument = 1,
          .diagnostic_name = "0",
      },
      {
          .options = 0,
          .processing_rate = 4,
          .power_coefficient_nw = 8,
          .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
          .control_argument = 3,
          .diagnostic_name = "1",
      },
      {
          .options = 0,
          .processing_rate = 0,
          .power_coefficient_nw = 2,
          .control_interface = cpp23::to_underlying(ControlInterface::kArmWfi),
          .control_argument = 0,
          .diagnostic_name = "2",
      },
      {
          .options = 0,
          .processing_rate = 4,
          .power_coefficient_nw = 10,
          .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
          .control_argument = 1,
          .diagnostic_name = "3",
      },
  });

  static constexpr auto kTransitions = std::to_array<zx_processor_power_level_transition_t>({
      {
          .latency = 1,
          .energy_nj = 2,
          .from = 1,
          .to = 0,
      },
      {

          .latency = 2,
          .energy_nj = 3,
          .from = 2,
          .to = 0,

      },
      {

          .latency = 3,
          .energy_nj = 4,
          .from = 3,
          .to = 0,
      },
      {

          .latency = 1,
          .energy_nj = 2,
          .from = 0,
          .to = 1,
      },
      {

          .latency = 3,
          .energy_nj = 4,
          .from = 2,
          .to = 1,
      },
      {

          .latency = 4,
          .energy_nj = 5,
          .from = 3,
          .to = 1,
      },
      {

          .latency = 2,
          .energy_nj = 3,
          .from = 0,
          .to = 2,
      },
      {

          .latency = 3,
          .energy_nj = 4,
          .from = 1,
          .to = 2,
      },
      {
          .latency = 5,
          .energy_nj = 6,
          .from = 3,
          .to = 2,
      },
      {

          .latency = 3,
          .energy_nj = 4,
          .from = 0,
          .to = 3,

      },
      {

          .latency = 4,
          .energy_nj = 5,
          .from = 1,
          .to = 3,
      },
      {

          .latency = 5,
          .energy_nj = 6,
          .from = 2,
          .to = 3,
      },
  });

  auto energy_model = EnergyModel::Create(kPowerLevels, kTransitions);
  ASSERT_TRUE(energy_model.is_ok());

  // Proper transformation of the model and the transition table.
  auto check_level = [](const PowerLevel& actual, const PowerLevel& expected) {
    EXPECT_EQ(actual.level(), expected.level());
    EXPECT_EQ(actual.control(), expected.control());
    EXPECT_EQ(actual.control_argument(), expected.control_argument());
    EXPECT_EQ(actual.name(), expected.name());
    EXPECT_EQ(actual.power_coefficient_nw(), expected.power_coefficient_nw());
    EXPECT_EQ(actual.processing_rate(), expected.processing_rate());
    EXPECT_EQ(actual.type(), expected.type());
    EXPECT_EQ(actual.TargetsCpus(), expected.TargetsCpus());
    EXPECT_EQ(actual.TargetsPowerDomain(), expected.TargetsPowerDomain());
  };

  ASSERT_EQ(energy_model->levels().size(), 4u);
  auto levels = energy_model->levels();
  for (size_t i = 0; i < levels.size() - 1; ++i) {
    size_t j = i + 1;
    EXPECT_LE(levels[i].processing_rate(), levels[i].processing_rate());
    if (levels[i].processing_rate() == levels[j].processing_rate()) {
      EXPECT_LE(levels[i].power_coefficient_nw(), levels[j].power_coefficient_nw());
    }
    check_level(levels[i], PowerLevel(levels[i].level(), kPowerLevels[levels[i].level()]));
    check_level(levels[j], PowerLevel(levels[j].level(), kPowerLevels[levels[j].level()]));
  }

  auto get_original_transition = [&levels](size_t i, size_t j) {
    size_t og_i = levels[i].level();
    size_t og_j = levels[j].level();
    for (const auto& transition : kTransitions) {
      if (transition.from == og_i && transition.to == og_j) {
        return PowerLevelTransition(transition);
      }
    }
    return PowerLevelTransition::Invalid();
  };

  auto transitions = energy_model->transitions();
  for (size_t i = 0; i < levels.size(); ++i) {
    for (size_t j = 0; j < levels.size(); ++j) {
      auto transition = transitions[i][j];
      auto og_transition = get_original_transition(i, j);
      EXPECT_EQ(transition.latency(), og_transition.latency());
      EXPECT_EQ(transition.energy_cost_nj(), og_transition.energy_cost_nj());
    }
  }

  // Properly partitioned.
  ASSERT_EQ(energy_model->idle_levels().size(), 2u);
  EXPECT_EQ(energy_model->idle_levels()[0].level(), 0u);
  EXPECT_EQ(energy_model->idle_levels()[1].level(), 2u);

  ASSERT_EQ(energy_model->active_levels().size(), 2u);
  EXPECT_EQ(energy_model->active_levels()[0].level(), 1u);
  EXPECT_EQ(energy_model->active_levels()[1].level(), 3u);

  // Sorter by tuple <Control Interface, Control Argument>
  for (size_t i = 0; i < levels.size(); ++i) {
    auto& level = levels[i];
    EXPECT_EQ(energy_model->FindPowerLevel(level.control(), level.control_argument()), i);
  }
  EXPECT_FALSE(energy_model->FindPowerLevel(ControlInterface::kArmPsci, 495));
  EXPECT_FALSE(energy_model->FindPowerLevel(static_cast<ControlInterface>(495), 0));
}

TEST(PowerModelTest, CreateWithEmptyTransitionsIsOk) {
  static constexpr auto kPowerLevels = std::to_array<zx_processor_power_level_t>({
      {
          .options = 0,
          .processing_rate = 0,
          .power_coefficient_nw = 1,
          .control_interface = cpp23::to_underlying(ControlInterface::kArmPsci),
          .control_argument = 1,
          .diagnostic_name = "0",
      },
      {
          .options = 0,
          .processing_rate = 4,
          .power_coefficient_nw = 8,
          .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
          .control_argument = 3,
          .diagnostic_name = "1",
      },
      {
          .options = 0,
          .processing_rate = 0,
          .power_coefficient_nw = 2,
          .control_interface = cpp23::to_underlying(ControlInterface::kArmWfi),
          .control_argument = 0,
          .diagnostic_name = "2",
      },
      {
          .options = 0,
          .processing_rate = 4,
          .power_coefficient_nw = 10,
          .control_interface = cpp23::to_underlying(ControlInterface::kCpuDriver),
          .control_argument = 1,
          .diagnostic_name = "3",
      },
  });

  auto energy_model = EnergyModel::Create(kPowerLevels, {});
  ASSERT_TRUE(energy_model.is_ok());

  // Proper transformation of the model and the transition table.
  auto check_level = [](const PowerLevel& actual, const PowerLevel& expected) {
    EXPECT_EQ(actual.level(), expected.level());
    EXPECT_EQ(actual.control(), expected.control());
    EXPECT_EQ(actual.control_argument(), expected.control_argument());
    EXPECT_EQ(actual.name(), expected.name());
    EXPECT_EQ(actual.power_coefficient_nw(), expected.power_coefficient_nw());
    EXPECT_EQ(actual.processing_rate(), expected.processing_rate());
    EXPECT_EQ(actual.type(), expected.type());
    EXPECT_EQ(actual.TargetsCpus(), expected.TargetsCpus());
    EXPECT_EQ(actual.TargetsPowerDomain(), expected.TargetsPowerDomain());
  };

  ASSERT_EQ(energy_model->levels().size(), 4u);
  auto levels = energy_model->levels();
  for (size_t i = 0; i < levels.size() - 1; ++i) {
    size_t j = i + 1;
    EXPECT_LE(levels[i].processing_rate(), levels[i].processing_rate());
    if (levels[i].processing_rate() == levels[j].processing_rate()) {
      EXPECT_LE(levels[i].power_coefficient_nw(), levels[j].power_coefficient_nw());
    }
    check_level(levels[i], PowerLevel(levels[i].level(), kPowerLevels[levels[i].level()]));
    check_level(levels[j], PowerLevel(levels[j].level(), kPowerLevels[levels[j].level()]));
  }

  for (size_t row = 0; row < energy_model->levels().size(); ++row) {
    for (size_t column = 0; column < energy_model->levels().size(); ++column) {
      const PowerLevelTransition& transition = energy_model->transitions()[row][column];
      EXPECT_EQ(transition.energy_cost_nj(), PowerLevelTransition::Zero().energy_cost_nj());
      EXPECT_EQ(transition.latency(), PowerLevelTransition::Zero().latency());
    }
  }

  ASSERT_EQ(energy_model->idle_levels().size(), 2u);
  EXPECT_EQ(energy_model->idle_levels()[0].level(), 0u);
  EXPECT_EQ(energy_model->idle_levels()[1].level(), 2u);

  ASSERT_EQ(energy_model->active_levels().size(), 2u);
  EXPECT_EQ(energy_model->active_levels()[0].level(), 1u);
  EXPECT_EQ(energy_model->active_levels()[1].level(), 3u);

  // Sorter by tuple <Control Interface, Control Argument>
  for (size_t i = 0; i < levels.size(); ++i) {
    auto& level = levels[i];
    EXPECT_EQ(energy_model->FindPowerLevel(level.control(), level.control_argument()), i);
  }
  EXPECT_FALSE(energy_model->FindPowerLevel(ControlInterface::kArmPsci, 495));
  EXPECT_FALSE(energy_model->FindPowerLevel(static_cast<ControlInterface>(495), 0));
}

TEST(PowerDomainRegistryTest, FindDomain) {
  std::array domains_to_register{
      MakePowerDomainHelper(0, 1, 2, 3),
      MakePowerDomainHelper(1, 4, 5, 6),
      MakePowerDomainHelper(2, 7, 8, 9),
  };

  PowerDomainRegistry registry;
  for (const auto& domain : domains_to_register) {
    ASSERT_TRUE(registry.Register(domain).is_ok());
  }

  for (const auto& domain : domains_to_register) {
    EXPECT_EQ(registry.Find(domain->id()), domain.get());
  }

  EXPECT_EQ(registry.Find(112345567), nullptr);
}

TEST(PowerDomainRegistryTest, RegisterUpdateUnregisterPowerDomains) {
  std::array unique_domains_to_register = {
      MakePowerDomainHelper(0, 0, 1, 2),
      MakePowerDomainHelper(1, 4, 5, 6),
      MakePowerDomainHelper(2, 8, 9, 10),
  };

  std::array unique_domains_to_update = {
      MakePowerDomainHelper(0, 0, 1, 2, 3),
      MakePowerDomainHelper(1, 4, 5, 6, 7),
      MakePowerDomainHelper(2, 8, 9, 10, 11),
  };

  std::array conflicting_domains_to_register = {
      MakePowerDomainHelper(3, 12, 13, 14, 0),
      MakePowerDomainHelper(4, 16, 17, 18, 1),
      MakePowerDomainHelper(5, 20, 21, 22, 2),
  };

  std::array conflicting_domains_to_update = {
      MakePowerDomainHelper(0, 0, 1, 2, 4),
      MakePowerDomainHelper(1, 4, 5, 7, 8),
      MakePowerDomainHelper(2, 8, 9, 10, 0),
  };

  // Called by the registry after each update to the power domain set maintained
  // by the registry. On each update, the power domain set must only contain a
  // subset of the unique power domains that are registered by the test.
  const auto update_callback = [&](const PowerDomainSet& domain_set) {
    domain_set.Visit([&](const fbl::RefPtr<PowerDomain>& domain) {
      EXPECT_TRUE(InRange(unique_domains_to_register, domain) ||
                  InRange(unique_domains_to_update, domain))
          << "domain " << domain->id();
    });
  };

  PowerDomainRegistry registry{update_callback};
  for (const fbl::RefPtr<PowerDomain>& domain : unique_domains_to_register) {
    EXPECT_EQ(domain->total_normalized_utilization(), Utilization{0});
    ASSERT_TRUE(registry.Register(domain).is_ok());
  }

  // All of the domains registered so far should be present in the registry.
  EXPECT_EQ(registry.power_domain_set().count(), unique_domains_to_register.size());
  registry.Visit([&](const fbl::RefPtr<PowerDomain>& domain) {
    EXPECT_TRUE(InRange(unique_domains_to_register, domain));
  });

  // Update each registered domain.
  for (const fbl::RefPtr<PowerDomain>& domain : unique_domains_to_update) {
    EXPECT_EQ(domain->total_normalized_utilization(), Utilization{0});
    ASSERT_TRUE(registry.Register(domain).is_ok());
  }

  // All of the updated domains should be present in the registry.
  EXPECT_EQ(registry.power_domain_set().count(), unique_domains_to_update.size());
  registry.Visit([&](const fbl::RefPtr<PowerDomain>& domain) {
    EXPECT_TRUE(InRange(unique_domains_to_update, domain));
  });

  // Attempt and fail to register new domain ids with CPU sets that intersect
  // with the already registered domains.
  for (const fbl::RefPtr<PowerDomain>& domain : conflicting_domains_to_register) {
    EXPECT_EQ(domain->total_normalized_utilization(), Utilization{0});
    ASSERT_FALSE(registry.Register(domain).is_ok());
  }

  // The domain set should remain unchanged from the last successful updates.
  EXPECT_EQ(registry.power_domain_set().count(), unique_domains_to_update.size());
  registry.Visit([&](const fbl::RefPtr<PowerDomain>& domain) {
    EXPECT_TRUE(InRange(unique_domains_to_update, domain));
  });

  // Attempt and fail to update existing domain ids with CPU sets that intersect
  // with other registered domains.
  for (const fbl::RefPtr<PowerDomain>& domain : conflicting_domains_to_update) {
    EXPECT_EQ(domain->total_normalized_utilization(), Utilization{0});
    ASSERT_FALSE(registry.Register(domain).is_ok());
  }

  // The domain set should remain unchanged from the last successful updates.
  EXPECT_EQ(registry.power_domain_set().count(), unique_domains_to_update.size());
  registry.Visit([&](const fbl::RefPtr<PowerDomain>& domain) {
    EXPECT_TRUE(InRange(unique_domains_to_update, domain));
  });

  // Unregister each domain and ensure that the updated domain set reflects the
  // change.
  for (const fbl::RefPtr<PowerDomain>& domain : unique_domains_to_update) {
    EXPECT_TRUE(registry.Unregister(domain->id()).is_ok());
    EXPECT_EQ(registry.Find(domain->id()), nullptr);
  }
  EXPECT_EQ(registry.power_domain_set().count(), 0u);
}

}  // namespace
