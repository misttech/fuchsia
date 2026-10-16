// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/features.h>
#include <zircon/syscalls/system.h>

#include <gtest/gtest.h>

#include "tools/fidlcat/interception_tests/interception_workflow_test.h"

namespace fidlcat {

// zx_system_get_dcache_line_size tests.

std::unique_ptr<SystemCallTest> ZxSystemGetDcacheLineSize(int64_t result,
                                                          std::string_view result_name) {
  auto value =
      std::make_unique<SystemCallTest>("zx_system_get_dcache_line_size", result, result_name);
  return value;
}

#define SYSTEM_GET_DCACHE_LINE_SIZE_DISPLAY_TEST_CONTENT(result, expected) \
  PerformDisplayTest("$plt(zx_system_get_dcache_line_size)",               \
                     ZxSystemGetDcacheLineSize(result, #result), expected)

#define SYSTEM_GET_DCACHE_LINE_SIZE_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                            \
    SYSTEM_GET_DCACHE_LINE_SIZE_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                                      \
  TEST_F(InterceptionWorkflowTestArm, name) {                            \
    SYSTEM_GET_DCACHE_LINE_SIZE_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_GET_DCACHE_LINE_SIZE_DISPLAY_TEST(
    ZxSystemGetDcacheLineSize, 64,
    "\n"
    "\x1B[32m0.000000\x1B[0m "
    "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m zx_system_get_dcache_line_size()\n"
    "\x1B[32m0.000000\x1B[0m "
    "  -> \x1B[34m64\x1B[0m\n")

// zx_system_get_num_cpus tests.

std::unique_ptr<SystemCallTest> ZxSystemGetNumCpus(int64_t result, std::string_view result_name) {
  auto value = std::make_unique<SystemCallTest>("zx_system_get_num_cpus", result, result_name);
  return value;
}

#define SYSTEM_GET_NUM_CPUS_DISPLAY_TEST_CONTENT(result, expected) \
  PerformDisplayTest("$plt(zx_system_get_num_cpus)", ZxSystemGetNumCpus(result, #result), expected)

#define SYSTEM_GET_NUM_CPUS_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                    \
    SYSTEM_GET_NUM_CPUS_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                              \
  TEST_F(InterceptionWorkflowTestArm, name) {                    \
    SYSTEM_GET_NUM_CPUS_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_GET_NUM_CPUS_DISPLAY_TEST(
    ZxSystemGetNumCpus, 8,
    "\n"
    "\x1B[32m0.000000\x1B[0m "
    "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m zx_system_get_num_cpus()\n"
    "\x1B[32m0.000000\x1B[0m "
    "  -> \x1B[34m8\x1B[0m\n")

// zx_system_get_physmem tests.

std::unique_ptr<SystemCallTest> ZxSystemGetPhysmem(int64_t result, std::string_view result_name) {
  auto value = std::make_unique<SystemCallTest>("zx_system_get_physmem", result, result_name);
  return value;
}

#define SYSTEM_GET_PHYSMEM_DISPLAY_TEST_CONTENT(result, expected) \
  PerformDisplayTest("$plt(zx_system_get_physmem)", ZxSystemGetPhysmem(result, #result), expected)

#define SYSTEM_GET_PHYSMEM_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                   \
    SYSTEM_GET_PHYSMEM_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                             \
  TEST_F(InterceptionWorkflowTestArm, name) {                   \
    SYSTEM_GET_PHYSMEM_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_GET_PHYSMEM_DISPLAY_TEST(
    ZxSystemGetPhysmem, 536870912,
    "\n"
    "\x1B[32m0.000000\x1B[0m "
    "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m zx_system_get_physmem()\n"
    "\x1B[32m0.000000\x1B[0m "
    "  -> \x1B[34m536870912\x1B[0m\n")

// zx_system_get_event tests.

std::unique_ptr<SystemCallTest> ZxSystemGetEvent(int64_t result, std::string_view result_name,
                                                 zx_handle_t root_job, uint32_t kind,
                                                 zx_handle_t* event) {
  auto value = std::make_unique<SystemCallTest>("zx_system_get_event", result, result_name);
  value->AddInput(root_job);
  value->AddInput(kind);
  value->AddInput(reinterpret_cast<uint64_t>(event));
  return value;
}

#define SYSTEM_GET_EVENT_DISPLAY_TEST_CONTENT(result, expected) \
  zx_handle_t event = kHandleOut;                               \
  PerformDisplayTest(                                           \
      "$plt(zx_system_get_event)",                              \
      ZxSystemGetEvent(result, #result, kHandle, ZX_SYSTEM_EVENT_OUT_OF_MEMORY, &event), expected)

#define SYSTEM_GET_EVENT_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                 \
    SYSTEM_GET_EVENT_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                           \
  TEST_F(InterceptionWorkflowTestArm, name) {                 \
    SYSTEM_GET_EVENT_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_GET_EVENT_DISPLAY_TEST(
    ZxSystemGetEvent, ZX_OK,
    "\n"
    "\x1B[32m0.000000\x1B[0m "
    "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m "
    "zx_system_get_event("
    "root_job: \x1B[32mhandle\x1B[0m = \x1B[31mcefa1db0\x1B[0m, "
    "kind: \x1B[32mzx.system_event_type\x1B[0m = \x1B[34mZX_SYSTEM_EVENT_OUT_OF_MEMORY\x1B[0m)\n"
    "\x1B[32m0.000000\x1B[0m "
    "  -> \x1B[32mZX_OK\x1B[0m (event: \x1B[32mhandle\x1B[0m = \x1B[31mbde90caf\x1B[0m)\n")

// zx_system_get_features tests.

std::unique_ptr<SystemCallTest> ZxSystemGetFeatures(int64_t result, std::string_view result_name,
                                                    uint32_t kind, uint32_t* features) {
  auto value = std::make_unique<SystemCallTest>("zx_system_get_features", result, result_name);
  value->AddInput(kind);
  value->AddInput(reinterpret_cast<uint64_t>(features));
  return value;
}

#define SYSTEM_GET_FEATURES_DISPLAY_TEST_CONTENT(result, expected)                          \
  uint32_t features = 8;                                                                    \
  PerformDisplayTest(                                                                       \
      "$plt(zx_system_get_features)",                                                       \
      ZxSystemGetFeatures(result, #result, ZX_FEATURE_KIND_HW_BREAKPOINT_COUNT, &features), \
      expected)

#define SYSTEM_GET_FEATURES_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                    \
    SYSTEM_GET_FEATURES_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                              \
  TEST_F(InterceptionWorkflowTestArm, name) {                    \
    SYSTEM_GET_FEATURES_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_GET_FEATURES_DISPLAY_TEST(
    ZxSystemGetFeatures, ZX_OK,
    "\n"
    "\x1B[32m0.000000\x1B[0m "
    "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m zx_system_get_features("
    "kind: \x1B[32mzx.feature_kind_t\x1B[0m = \x1B[31mZX_FEATURE_KIND_HW_BREAKPOINT_COUNT\x1B[0m)\n"
    "\x1B[32m0.000000\x1B[0m "
    "  -> \x1B[32mZX_OK\x1B[0m (features: \x1B[32muint32\x1B[0m = \x1B[34m8\x1B[0m)\n")

// zx_system_powerctl tests.

std::unique_ptr<SystemCallTest> ZxSystemPowerctl(int64_t result, std::string_view result_name,
                                                 zx_handle_t resource, uint32_t cmd) {
  auto value = std::make_unique<SystemCallTest>("zx_system_powerctl", result, result_name);
  value->AddInput(resource);
  value->AddInput(cmd);
  return value;
}

#define SYSTEM_POWERCTL_DISPLAY_TEST_CONTENT(result, expected)                              \
  PerformDisplayTest("$plt(zx_system_powerctl)",                                            \
                     ZxSystemPowerctl(result, #result, kHandle, ZX_SYSTEM_POWERCTL_REBOOT), \
                     expected)

#define SYSTEM_POWERCTL_DISPLAY_TEST(name, result, expected) \
  TEST_F(InterceptionWorkflowTestX64, name) {                \
    SYSTEM_POWERCTL_DISPLAY_TEST_CONTENT(result, expected);  \
  }                                                          \
  TEST_F(InterceptionWorkflowTestArm, name) {                \
    SYSTEM_POWERCTL_DISPLAY_TEST_CONTENT(result, expected);  \
  }

SYSTEM_POWERCTL_DISPLAY_TEST(ZxSystemPowerctl, ZX_OK,
                             "\n"
                             "\x1B[32m0.000000\x1B[0m "
                             "test_3141 \x1B[31m3141\x1B[0m:\x1B[31m8764\x1B[0m "
                             "zx_system_powerctl("
                             "resource: \x1B[32mhandle\x1B[0m = \x1B[31mcefa1db0\x1B[0m, "
                             "cmd: \x1B[32mzx.system_powerctl\x1B[0m = "
                             "\x1B[34mZX_SYSTEM_POWERCTL_REBOOT\x1B[0m)\n"
                             "\x1B[32m0.000000\x1B[0m "
                             "  -> \x1B[32mZX_OK\x1B[0m\n")

}  // namespace fidlcat
