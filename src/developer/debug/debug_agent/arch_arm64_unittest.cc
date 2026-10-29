// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/zx/event.h>
#include <lib/zx/suspend_token.h>
#include <lib/zx/thread.h>
#include <zircon/assert.h>
#include <zircon/features.h>
#include <zircon/syscalls.h>

#include <thread>

#include <gtest/gtest.h>

#include "src/developer/debug/debug_agent/arch.h"
#include "src/developer/debug/debug_agent/test_utils.h"
#include "src/developer/debug/debug_agent/zircon_arch.h"
#include "src/developer/debug/shared/logging/file_line_function.h"
#include "src/developer/debug/shared/register_test_support.h"
#include "src/developer/debug/shared/zx_status.h"

namespace debug_agent {
namespace arch {

TEST(ArchArm64, WriteGeneralRegs) {
  std::vector<debug::RegisterValue> regs;
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_x0, 8));
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_x3, 8));
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_lr, 8));
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_pc, 8));

  zx_thread_state_general_regs_t out = {};
  zx_status_t res = WriteGeneralRegisters(regs, &out);
  ASSERT_EQ(res, ZX_OK) << "Expected ZX_OK, got " << debug::ZxStatusToString(res);

  EXPECT_EQ(out.r[0], 0x0102030405060708u);
  EXPECT_EQ(out.r[1], 0u);
  EXPECT_EQ(out.r[2], 0u);
  EXPECT_EQ(out.r[3], 0x0102030405060708u);
  EXPECT_EQ(out.r[4], 0u);
  EXPECT_EQ(out.r[29], 0u);
  EXPECT_EQ(out.lr, 0x0102030405060708u);
  EXPECT_EQ(out.pc, 0x0102030405060708u);

  regs.clear();
  regs.emplace_back(debug::RegisterID::kARMv8_x0, static_cast<uint64_t>(0xaabb));
  regs.emplace_back(debug::RegisterID::kARMv8_x15, static_cast<uint64_t>(0xdead));
  regs.emplace_back(debug::RegisterID::kARMv8_pc, static_cast<uint64_t>(0xbeef));

  res = WriteGeneralRegisters(regs, &out);
  ASSERT_EQ(res, ZX_OK) << "Expected ZX_OK, got " << debug::ZxStatusToString(res);

  EXPECT_EQ(out.r[0], 0xaabbu);
  EXPECT_EQ(out.r[1], 0u);
  EXPECT_EQ(out.r[15], 0xdeadu);
  EXPECT_EQ(out.r[29], 0u);
  EXPECT_EQ(out.lr, 0x0102030405060708u);
  EXPECT_EQ(out.pc, 0xbeefu);
}

TEST(ArchArm64, InvalidWriteGeneralRegs) {
  zx_thread_state_general_regs_t out;
  std::vector<debug::RegisterValue> regs;

  // Invalid length.
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_v0, 4));
  EXPECT_EQ(WriteGeneralRegisters(regs, &out), ZX_ERR_INVALID_ARGS);

  // Invalid (non-canonical) register.
  regs.push_back(debug::CreateRegisterWithTestData(debug::RegisterID::kARMv8_w3, 8));
  EXPECT_EQ(WriteGeneralRegisters(regs, &out), ZX_ERR_INVALID_ARGS);
}

TEST(ArchArm64, WriteVectorRegs) {
  std::vector<debug::RegisterValue> regs;

  std::vector<uint8_t> v0_value;
  v0_value.resize(16);
  v0_value.front() = 0x42;
  v0_value.back() = 0x12;
  regs.emplace_back(debug::RegisterID::kARMv8_v0, v0_value);

  std::vector<uint8_t> v31_value = v0_value;
  v31_value.front()++;
  v31_value.back()++;
  regs.emplace_back(debug::RegisterID::kARMv8_v31, v31_value);

  std::vector<uint8_t> z1_value = v31_value;
  z1_value.front()++;
  z1_value.back()++;
  regs.emplace_back(debug::RegisterID::kARMv8_z1, z1_value);

  std::vector<uint8_t> z30_value = z1_value;
  z30_value.front()++;
  z30_value.back()++;
  regs.emplace_back(debug::RegisterID::kARMv8_z30, z30_value);

  regs.emplace_back(debug::RegisterID::kARMv8_fpcr, std::vector<uint8_t>{5, 6, 7, 8});
  regs.emplace_back(debug::RegisterID::kARMv8_fpsr, std::vector<uint8_t>{9, 0, 1, 2});

  zx_thread_state_vector_regs_t out = {};
  zx_status_t res = WriteVectorRegisters(regs, &out);
  ASSERT_EQ(res, ZX_OK) << "Expected ZX_OK, got " << debug::ZxStatusToString(res);

  EXPECT_EQ(out.v[0].low, 0x0000000000000042u);
  EXPECT_EQ(out.v[0].high, 0x1200000000000000u);
  EXPECT_EQ(out.v[1].low, 0x0000000000000044u);
  EXPECT_EQ(out.v[1].high, 0x1400000000000000u);
  EXPECT_EQ(out.v[30].low, 0x0000000000000045u);
  EXPECT_EQ(out.v[30].high, 0x1500000000000000u);
  EXPECT_EQ(out.v[31].low, 0x0000000000000043u);
  EXPECT_EQ(out.v[31].high, 0x1300000000000000u);

  EXPECT_EQ(out.fpcr, 0x08070605u);
  EXPECT_EQ(out.fpsr, 0x02010009u);

  // VG is a read-only pseudo-register and SVE predicate/FFR registers belong to
  // ZX_THREAD_STATE_ARM64_SVE rather than ZX_THREAD_STATE_VECTOR_REGS.
  EXPECT_EQ(WriteVectorRegisters({{debug::RegisterID::kARMv8_vg, uint64_t{2}}}, &out),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(WriteVectorRegisters(
                {{debug::RegisterID::kARMv8_ffr, std::vector<uint8_t>{0xff, 0x00}}}, &out),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(WriteVectorRegisters({{debug::RegisterID::kARMv8_p0, std::vector<uint8_t>{0x34, 0x12}}},
                                 &out),
            ZX_ERR_INVALID_ARGS);

  // Invalid byte lengths (including 0-byte boundary) must be rejected.
  EXPECT_EQ(WriteVectorRegisters({{debug::RegisterID::kARMv8_z0, std::vector<uint8_t>{}}}, &out),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(
      WriteVectorRegisters({{debug::RegisterID::kARMv8_z0, std::vector<uint8_t>(8, 0)}}, &out),
      ZX_ERR_INVALID_ARGS);
}

namespace {

TEST(ArchArm64, WriteArm64SveRegs) {
  zx_arm64_thread_state_sve_regs_t out = {};
  out.p[7] = 0xabcd;

  std::vector<debug::RegisterValue> regs;
  regs.emplace_back(debug::RegisterID::kARMv8_ffr, std::vector<uint8_t>{0xff, 0x00});
  regs.emplace_back(debug::RegisterID::kARMv8_p0, std::vector<uint8_t>{0x34, 0x12});
  regs.emplace_back(debug::RegisterID::kARMv8_p15, std::vector<uint8_t>{0x78, 0x56});

  ASSERT_EQ(WriteArm64SveRegisters(regs, &out), ZX_OK);
  EXPECT_EQ(out.ffr, 0x00ffu);
  EXPECT_EQ(out.p[0], 0x1234u);
  EXPECT_EQ(out.p[7], 0xabcdu);
  EXPECT_EQ(out.p[15], 0x5678u);

  // Non-SVE-predicate registers and read-only VG must be rejected.
  EXPECT_EQ(WriteArm64SveRegisters({{debug::RegisterID::kARMv8_vg, uint64_t{2}}}, &out),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(
      WriteArm64SveRegisters({{debug::RegisterID::kARMv8_v0, std::vector<uint8_t>(16, 0)}}, &out),
      ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(
      WriteArm64SveRegisters({{debug::RegisterID::kARMv8_z0, std::vector<uint8_t>(16, 0)}}, &out),
      ZX_ERR_INVALID_ARGS);

  // Invalid byte lengths (including 0-byte boundary) must be rejected.
  EXPECT_EQ(WriteArm64SveRegisters({{debug::RegisterID::kARMv8_p0, std::vector<uint8_t>{}}}, &out),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(
      WriteArm64SveRegisters({{debug::RegisterID::kARMv8_ffr, std::vector<uint8_t>(4, 0)}}, &out),
      ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(
      WriteArm64SveRegisters({{debug::RegisterID::kARMv8_p15, std::vector<uint8_t>(4, 0)}}, &out),
      ZX_ERR_INVALID_ARGS);
}

TEST(ArchArm64, ReadAndWriteVectorCategoryRegistersLiveThread) {
  zx::event ready_event;
  zx::event exit_event;
  ASSERT_EQ(zx::event::create(0, &ready_event), ZX_OK);
  ASSERT_EQ(zx::event::create(0, &exit_event), ZX_OK);

  zx::thread worker_thread;
  std::thread worker([&ready_event, &exit_event, &worker_thread] {
    zx_status_t status = zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &worker_thread);
    ZX_ASSERT(status == ZX_OK);
    status = ready_event.signal(0, ZX_USER_SIGNAL_0);
    ZX_ASSERT(status == ZX_OK);
    exit_event.wait_one(ZX_USER_SIGNAL_0, zx::time::infinite(), nullptr);
  });

  zx::suspend_token suspend_token;
  auto cleanup = fit::defer([&] {
    suspend_token.reset();
    exit_event.signal(0, ZX_USER_SIGNAL_0);
    if (worker.joinable()) {
      worker.join();
    }
  });

  ASSERT_EQ(ready_event.wait_one(ZX_USER_SIGNAL_0, zx::time::infinite(), nullptr), ZX_OK);
  ASSERT_EQ(worker_thread.suspend(&suspend_token), ZX_OK);
  ASSERT_EQ(worker_thread.wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), nullptr), ZX_OK);

  uint32_t features = 0;
  ASSERT_EQ(zx_system_get_features(ZX_FEATURE_KIND_CPU, &features), ZX_OK);
  const bool has_sve = (features & ZX_ARM64_FEATURE_ISA_SVE) != 0;

  // Empty updates succeed as a no-op and read-only VG is rejected at the category level.
  ASSERT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector, {}), ZX_OK);
  EXPECT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                           {{debug::RegisterID::kARMv8_vg, uint64_t{2}}}),
            ZX_ERR_INVALID_ARGS);

  std::vector<debug::RegisterValue> initial_regs;
  ASSERT_EQ(ReadRegisters(worker_thread, debug::RegisterCategory::kVector, initial_regs), ZX_OK);

  auto find_reg = [](const std::vector<debug::RegisterValue>& list,
                     debug::RegisterID id) -> const debug::RegisterValue* {
    for (const auto& r : list) {
      if (r.id == id)
        return &r;
    }
    return nullptr;
  };

  EXPECT_NE(find_reg(initial_regs, debug::RegisterID::kARMv8_v0), nullptr);
  if (!has_sve) {
    EXPECT_EQ(find_reg(initial_regs, debug::RegisterID::kARMv8_vg), nullptr);
    EXPECT_EQ(find_reg(initial_regs, debug::RegisterID::kARMv8_ffr), nullptr);
    EXPECT_EQ(find_reg(initial_regs, debug::RegisterID::kARMv8_p0), nullptr);
    EXPECT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                             {{debug::RegisterID::kARMv8_p0, std::vector<uint8_t>{0x34, 0x12}}}),
              ZX_ERR_NOT_SUPPORTED);
    EXPECT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                             {{debug::RegisterID::kARMv8_z0, std::vector<uint8_t>(16, 0)}}),
              ZX_ERR_NOT_SUPPORTED);
    return;
  }

  const debug::RegisterValue* vg = find_reg(initial_regs, debug::RegisterID::kARMv8_vg);
  ASSERT_NE(vg, nullptr);
  ASSERT_EQ(vg->data.size(), sizeof(uint64_t));
  uint64_t vg_val = 0;
  memcpy(&vg_val, vg->data.data(), sizeof(vg_val));
  EXPECT_EQ(vg_val, 2u);

  // Seed v0 with a known pattern first; when v0 and z0 overlap in one request, the later entry
  // wins because they share the low 128 bits.
  std::vector<uint8_t> seed_v0(16, 0x11);
  ASSERT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                           {{debug::RegisterID::kARMv8_v0, std::vector<uint8_t>(16, 0x22)},
                            {debug::RegisterID::kARMv8_z0, seed_v0}}),
            ZX_OK);

  // A combined z0 + non-monotonic ffr write must fail with ZX_ERR_INVALID_ARGS without mutating
  // v0/z0 in ZX_THREAD_STATE_VECTOR_REGS.
  std::vector<uint8_t> new_z0(16, 0x99);
  EXPECT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                           {{debug::RegisterID::kARMv8_z0, new_z0},
                            {debug::RegisterID::kARMv8_ffr, std::vector<uint8_t>{0x02, 0x00}}}),
            ZX_ERR_INVALID_ARGS);

  std::vector<debug::RegisterValue> after_failed_write;
  ASSERT_EQ(ReadRegisters(worker_thread, debug::RegisterCategory::kVector, after_failed_write),
            ZX_OK);
  const debug::RegisterValue* v0_after_fail =
      find_reg(after_failed_write, debug::RegisterID::kARMv8_v0);
  ASSERT_NE(v0_after_fail, nullptr);
  EXPECT_EQ(v0_after_fail->data, seed_v0);

  // A valid combined z0 + p0 + p15 + ffr write must update both topics.
  ASSERT_EQ(WriteRegisters(worker_thread, debug::RegisterCategory::kVector,
                           {{debug::RegisterID::kARMv8_z0, new_z0},
                            {debug::RegisterID::kARMv8_p0, std::vector<uint8_t>{0x34, 0x12}},
                            {debug::RegisterID::kARMv8_p15, std::vector<uint8_t>{0x78, 0x56}},
                            {debug::RegisterID::kARMv8_ffr, std::vector<uint8_t>{0x7f, 0x00}}}),
            ZX_OK);

  std::vector<debug::RegisterValue> after_valid_write;
  ASSERT_EQ(ReadRegisters(worker_thread, debug::RegisterCategory::kVector, after_valid_write),
            ZX_OK);
  const debug::RegisterValue* v0_after = find_reg(after_valid_write, debug::RegisterID::kARMv8_v0);
  const debug::RegisterValue* p0_after = find_reg(after_valid_write, debug::RegisterID::kARMv8_p0);
  const debug::RegisterValue* p15_after =
      find_reg(after_valid_write, debug::RegisterID::kARMv8_p15);
  const debug::RegisterValue* ffr_after =
      find_reg(after_valid_write, debug::RegisterID::kARMv8_ffr);
  ASSERT_NE(v0_after, nullptr);
  ASSERT_NE(p0_after, nullptr);
  ASSERT_NE(p15_after, nullptr);
  ASSERT_NE(ffr_after, nullptr);
  EXPECT_EQ(v0_after->data, new_z0);
  EXPECT_EQ(p0_after->data, (std::vector<uint8_t>{0x34, 0x12}));
  EXPECT_EQ(p15_after->data, (std::vector<uint8_t>{0x78, 0x56}));
  EXPECT_EQ(ffr_after->data, (std::vector<uint8_t>{0x7f, 0x00}));
}

}  // namespace

TEST(ArchArm64, WriteDebugRegs) {
  std::vector<debug::RegisterValue> regs;
  regs.emplace_back(debug::RegisterID::kARMv8_dbgbcr0_el1, std::vector<uint8_t>{1, 2, 3, 4});
  regs.emplace_back(debug::RegisterID::kARMv8_dbgbcr1_el1, std::vector<uint8_t>{2, 3, 4, 5});
  regs.emplace_back(debug::RegisterID::kARMv8_dbgbcr15_el1, std::vector<uint8_t>{3, 4, 5, 6});

  regs.emplace_back(debug::RegisterID::kARMv8_dbgbvr0_el1,
                    std::vector<uint8_t>{4, 5, 6, 7, 8, 9, 0, 1});
  regs.emplace_back(debug::RegisterID::kARMv8_dbgbvr1_el1,
                    std::vector<uint8_t>{5, 6, 7, 8, 9, 0, 1, 2});
  regs.emplace_back(debug::RegisterID::kARMv8_dbgbvr15_el1,
                    std::vector<uint8_t>{6, 7, 8, 9, 0, 1, 2, 3});

  // TODO(bug 40992) Add ARM64 hardware watchpoint registers here.

  zx_thread_state_debug_regs_t out = {};
  zx_status_t res = WriteDebugRegisters(regs, &out);
  ASSERT_EQ(res, ZX_OK) << "Expected ZX_OK, got " << debug::ZxStatusToString(res);

  EXPECT_EQ(out.hw_bps[0].dbgbcr, 0x04030201u);
  EXPECT_EQ(out.hw_bps[1].dbgbcr, 0x05040302u);
  EXPECT_EQ(out.hw_bps[15].dbgbcr, 0x06050403u);
  EXPECT_EQ(out.hw_bps[0].dbgbvr, 0x0100090807060504u);
  EXPECT_EQ(out.hw_bps[1].dbgbvr, 0x0201000908070605u);
  EXPECT_EQ(out.hw_bps[15].dbgbvr, 0x0302010009080706u);
}

TEST(ArchArm64, ReadTPIDR) {
  zx_thread_state_general_regs_t regs_in;
  regs_in.tpidr = 0xdeadbeeff00dbabe;

  std::vector<debug::RegisterValue> regs_out;
  arch::SaveGeneralRegs(regs_in, regs_out);

  const debug::RegisterValue* found = nullptr;
  for (const auto& reg : regs_out) {
    if (reg.id == debug::RegisterID::kARMv8_tpidr) {
      found = &reg;
      break;
    }
  }

  ASSERT_NE(nullptr, found);
  ASSERT_GE(8u, found->data.size());
  EXPECT_EQ(8u, found->data.size());

  EXPECT_EQ(0xbe, found->data[0]);
  EXPECT_EQ(0xba, found->data[1]);
  EXPECT_EQ(0x0d, found->data[2]);
  EXPECT_EQ(0xf0, found->data[3]);
  EXPECT_EQ(0xef, found->data[4]);
  EXPECT_EQ(0xbe, found->data[5]);
  EXPECT_EQ(0xad, found->data[6]);
  EXPECT_EQ(0xde, found->data[7]);
}

}  // namespace arch
}  // namespace debug_agent
