// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstdint>
#include <memory>

#include "src/devices/block/drivers/ufs/transfer_request_descriptor.h"
#include "src/devices/block/drivers/ufs/upiu/descriptors.h"
#include "src/devices/block/drivers/ufs/upiu/upiu_transactions.h"
#include "unit-lib.h"
#include "zircon/errors.h"

namespace ufs {
using namespace ufs_mock_device;

using TiemoutTest = UfsTest;

TEST_F(TiemoutTest, GetEarliestTimeoutDeadline) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetRequestList().GetSlotCount() - kAdminCommandSlotCount;

  // Disable IoLoop completion
  dut_->GetTransferRequestProcessor().DisableCompletion();

  // If there is no in-flight I/O, ZX_TIME_INFINITE should be returned.
  {
    zx_time_t deadline = dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline();
    ASSERT_EQ(ZX_TIME_INFINITE, deadline);
  }

  // If there are in-flight I/Os, it should return the earliest timeout deadline.
  {
    IoCommand empty_io_cmd = {};
    empty_io_cmd.device_op.op.rw.offset_dev = 0;
    empty_io_cmd.device_op.op.rw.length = 0;
    empty_io_cmd.device_op.completion_cb = [](void* cookie, zx_status_t status, block_op_t* op) {};

    uint8_t cdb_buffer[6] = {};
    auto cdb = reinterpret_cast<scsi::TestUnitReadyCDB*>(cdb_buffer);
    cdb->opcode = scsi::Opcode::TEST_UNIT_READY;

    ScsiCommandUpiu upiu(cdb_buffer, sizeof(*cdb), DataDirection::kNone);
    for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
      auto response =
          dut_->GetTransferRequestProcessor().SendIoScsiCmd(upiu, kTestLun, &empty_io_cmd);
      ASSERT_OK(response);
    }

    // Request in slot 0 is the earliest issued request.
    zx_time_t slot_0_deadline =
        dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(0).deadline;
    for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
      EXPECT_LE(slot_0_deadline,
                dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).deadline);
    }

    // Wait 100 ms for outstanding send requests to complete.
    usleep(100000);

    zx_time_t deadline = dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline();
    ASSERT_EQ(slot_0_deadline, deadline);

    dut_->GetTransferRequestProcessor().EnableCompletion();
    ASSERT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), kMaxSlotCount);
  }
}

TEST_F(TiemoutTest, AsyncCommandTimeout) {
  constexpr uint8_t kTestLun = 0;
  constexpr uint8_t target_task_tag = 0;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));

  auto block_op = std::make_unique<uint8_t[]>(dut_->BlockOpSize());
  block_op_t& op = *reinterpret_cast<block_op_t*>(block_op.get());
  scsi::DeviceOp* device_op = containerof(&op, scsi::DeviceOp, op);
  device_op->op.command.opcode = BLOCK_OPCODE_READ;
  device_op->op.rw.length = 1;
  device_op->op.rw.vmo = vmo.get();
  device_op->completion_cb = [](void* ctx, zx_status_t status, block_op_t* op) {};
  IoCommand* io_cmd = containerof(device_op, IoCommand, device_op);
  io_cmd->block_size_bytes = kMockBlockSize;

  // Emulates a timeout situation. Hook the SCSI command handler to set a response timeout.
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::READ_10,
      [](UfsMockDevice& mock_device, CommandUpiuData& command_upiu, ResponseUpiuData& response_upiu,
         cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        return zx::error(ZX_ERR_TIMED_OUT);
      });

  dut_->ExecuteCommandAsync(0, lun_id.value(), {cdb_buffer, cdb_length}, false, 4096, device_op,
                            {nullptr, 0});

  auto wait_for = [&]() -> bool {
    return dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(target_task_tag).state ==
           SlotState::kTimeout;
  };
  fbl::String timeout_message = "Timeout waiting for SCSI command timeout";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, zx::sec(10), timeout_message, zx::msec(100)));

  // Check that the timed out command is aborted and not in the request list
  ASSERT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(target_task_tag).state,
            SlotState::kTimeout);
}

TEST_F(TiemoutTest, AllAsyncCommandsTimeout) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetRequestList().GetSlotCount() - kAdminCommandSlotCount;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  // Emulates a timeout situation. Hook the SCSI command handler to set a response timeout.
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::READ_10,
      [](UfsMockDevice& mock_device, CommandUpiuData& command_upiu, ResponseUpiuData& response_upiu,
         cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        return zx::error(ZX_ERR_TIMED_OUT);
      });

  auto block_ops = std::make_unique<uint8_t[]>(dut_->BlockOpSize() * kMaxSlotCount);
  auto vmos = std::make_unique<zx::vmo[]>(kMaxSlotCount);

  auto callback = [](void* ctx, zx_status_t status, block_op_t* op) {};

  for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    block_op_t& op =
        *(reinterpret_cast<block_op_t*>(block_ops.get() + (dut_->BlockOpSize() * slot_num)));
    scsi::DeviceOp* device_op = containerof(&op, scsi::DeviceOp, op);
    device_op->op.command.opcode = BLOCK_OPCODE_READ;
    device_op->op.rw.length = 1;
    device_op->op.rw.vmo = vmos[slot_num].get();
    device_op->completion_cb = callback;

    dut_->ExecuteCommandAsync(0, lun_id.value(), {cdb_buffer, cdb_length}, false, 4096, device_op,
                              {nullptr, 0});
  }

  auto wait_for = [&]() -> bool {
    bool all_timed_out = true;
    for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
      if (dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).state !=
          SlotState::kTimeout) {
        all_timed_out = false;
      }
    }
    return all_timed_out;
  };
  fbl::String timeout_message = "Timeout waiting for SCSI command timeout";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, zx::sec(10), timeout_message, zx::msec(100)));

  // Check that the timed out command.
  for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).state,
              SlotState::kTimeout);
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).result,
              ZX_ERR_TIMED_OUT);
  }
}

TEST_F(TiemoutTest, PartialAsyncCommandsTimeout) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetRequestList().GetSlotCount() - kAdminCommandSlotCount;
  const uint8_t kTimeoutCount = kMaxSlotCount / 2;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  // Emulates a timeout situation. Hook the SCSI command handler to set a response timeout.
  // This hook is only affects the READ_10 command.
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::READ_10,
      [](UfsMockDevice& mock_device, CommandUpiuData& command_upiu, ResponseUpiuData& response_upiu,
         cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        return zx::error(ZX_ERR_TIMED_OUT);
      });

  auto block_ops = std::make_unique<uint8_t[]>(dut_->BlockOpSize() * kMaxSlotCount);
  auto vmos = std::make_unique<zx::vmo[]>(kMaxSlotCount);

  auto callback = [](void* ctx, zx_status_t status, block_op_t* op) {};

  // Execute READ_10 commands to timeout.
  cdb->opcode = scsi::Opcode::READ_10;
  for (uint8_t slot_num = 0; slot_num < kTimeoutCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    block_op_t& op =
        *(reinterpret_cast<block_op_t*>(block_ops.get() + (dut_->BlockOpSize() * slot_num)));
    scsi::DeviceOp* device_op = containerof(&op, scsi::DeviceOp, op);
    device_op->op.command.opcode = BLOCK_OPCODE_READ;
    device_op->op.rw.length = 1;
    device_op->op.rw.vmo = vmos[slot_num].get();
    device_op->completion_cb = callback;
    IoCommand* io_cmd = containerof(device_op, IoCommand, device_op);
    io_cmd->block_size_bytes = kMockBlockSize;

    dut_->ExecuteCommandAsync(0, lun_id.value(), {cdb_buffer, cdb_length}, false, 4096, device_op,
                              {nullptr, 0});
  }

  // Execute WRITE_10 commands to succeed.
  cdb->opcode = scsi::Opcode::WRITE_10;
  for (uint8_t slot_num = kTimeoutCount; slot_num < kMaxSlotCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    block_op_t& op =
        *(reinterpret_cast<block_op_t*>(block_ops.get() + (dut_->BlockOpSize() * slot_num)));
    scsi::DeviceOp* device_op = containerof(&op, scsi::DeviceOp, op);
    device_op->op.command.opcode = BLOCK_OPCODE_WRITE;
    device_op->op.rw.length = 1;
    device_op->op.rw.vmo = vmos[slot_num].get();
    device_op->completion_cb = callback;
    IoCommand* io_cmd = containerof(device_op, IoCommand, device_op);
    io_cmd->block_size_bytes = kMockBlockSize;

    dut_->ExecuteCommandAsync(0, lun_id.value(), {cdb_buffer, cdb_length}, true, 4096, device_op,
                              {nullptr, 0});
  }

  auto wait_for = [&]() -> bool {
    bool all_timed_out = true;
    for (uint8_t slot_num = 0; slot_num < kTimeoutCount; ++slot_num) {
      if (dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).state !=
          SlotState::kTimeout) {
        all_timed_out = false;
      }
    }
    return all_timed_out;
  };
  fbl::String timeout_message = "Timeout waiting for SCSI command timeout";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, zx::sec(10), timeout_message, zx::msec(100)));

  // Check that the timed out command.
  for (uint8_t slot_num = 0; slot_num < kTimeoutCount; ++slot_num) {
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).state,
              SlotState::kTimeout);
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).result,
              ZX_ERR_TIMED_OUT);
  }

  // Check that the completed command.
  for (uint8_t slot_num = kTimeoutCount; slot_num < kMaxSlotCount; ++slot_num) {
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).state,
              SlotState::kFree);
    EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestList().GetSlot(slot_num).result, ZX_OK);
  }
}

}  // namespace ufs
