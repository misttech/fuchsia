// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/abr/abr.h>
#include <lib/abr/data.h>
#include <lib/zircon_boot/zircon_boot.h>
#include <zircon/hw/gpt.h>

#include <cstdarg>
#include <cstring>

#include <efi/types.h>
#include <gbl/gbl_efi_protocols.h>
#include <gbl/uefi/protocols/gbl_efi_boot_control_protocol.h>
#include <gtest/gtest.h>

#include "gpt.h"
#include "mock_boot_service.h"
#include "utils.h"
#include "zircon_boot_ops.h"

namespace gigaboot {
namespace {

bool TestReadFromPartition(ZirconBootOps* ops, const char* part, size_t offset, size_t size,
                           void* dst, size_t* read_size) {
  auto gpt_device = FindEfiGptDevice();
  if (gpt_device.is_error()) {
    return false;
  }

  auto load_res = gpt_device.value().Load();
  if (load_res.is_error()) {
    return false;
  }

  *read_size = size;
  part = MaybeMapPartitionName(gpt_device.value(), part).data();
  return gpt_device.value().ReadPartition(part, offset, size, dst).is_ok();
}

bool TestWriteToPartition(ZirconBootOps* ops, const char* part, size_t offset, size_t size,
                          const void* src, size_t* write_size) {
  auto gpt_device = FindEfiGptDevice();
  if (gpt_device.is_error()) {
    return false;
  }

  auto load_res = gpt_device.value().Load();
  if (load_res.is_error()) {
    return false;
  }

  *write_size = size;
  part = MaybeMapPartitionName(gpt_device.value(), part).data();
  return gpt_device.value().WritePartition(part, src, offset, size).is_ok();
}

}  // namespace

ZirconBootOps GetZirconBootOps() {
  ZirconBootOps zircon_boot_ops = {};
  zircon_boot_ops.read_from_partition = TestReadFromPartition;
  zircon_boot_ops.write_to_partition = TestWriteToPartition;
  return zircon_boot_ops;
}

namespace {

class GblEfiBootControlProtocolTest : public ::testing::Test {
 public:
  GblEfiBootControlProtocolTest()
      : image_device_({"path-A", "path-B", "path-C", "image"}),
        block_device_({"path-A", "path-B", "path-C"}, 1024) {}

  static EFIAPI efi_status CaptureBootControlProtocol(efi_handle* handle, ...) {
    auto* test = static_cast<GblEfiBootControlProtocolTest*>(gEfiImageHandle);
    if (!test) {
      return EFI_INVALID_PARAMETER;
    }

#if defined(__x86_64__) && !defined(_WIN32)
    __builtin_ms_va_list args;
    __builtin_ms_va_start(args, handle);
    const efi_guid* guid = va_arg(args, const efi_guid*);
    void* protocol = va_arg(args, void*);
    __builtin_ms_va_end(args);
#else
    va_list args;
    va_start(args, handle);
    const efi_guid* guid = va_arg(args, const efi_guid*);
    void* protocol = va_arg(args, void*);
    va_end(args);
#endif

    if (guid) {
      test->installed_boot_control_guid_ = *guid;
    }
    test->installed_boot_control_protocol_ = static_cast<GblEfiBootControlProtocol*>(protocol);
    return EFI_SUCCESS;
  }

  void SetUp() override {
    installed_boot_control_protocol_ = nullptr;
    installed_boot_control_guid_ = {};
    g_reboot_mode = RebootMode::kNormal;
    gEfiImageHandle = this;
    stub_service_.services()->InstallMultipleProtocolInterfaces = CaptureBootControlProtocol;
  }

  void TearDown() override {
    g_reboot_mode = RebootMode::kNormal;
    gEfiImageHandle = nullptr;
    installed_boot_control_protocol_ = nullptr;
  }

  auto SetupEfiStateWithGpt() {
    auto cleanup = SetupEfiGlobalState(stub_service_, image_device_);
    stub_service_.AddDevice(&image_device_);
    stub_service_.AddDevice(&block_device_);
    block_device_.InitializeGpt();

    gpt_entry_t entry{{}, {}, kGptFirstUsableBlocks, kGptFirstUsableBlocks + 5, 0, {}};
    SetGptEntryName(GPT_DURABLE_BOOT_NAME, entry);
    block_device_.AddGptPartition(entry);

    ZirconBootOps zb_ops = GetZirconBootOps();
    AbrOps abr_ops = GetAbrOpsFromZirconBootOps(&zb_ops);
    EXPECT_EQ(AbrMarkSlotActive(&abr_ops, kAbrSlotIndexA), kAbrResultOk);

    EXPECT_EQ(InstallGblEfiBootControlProtocol(), EFI_SUCCESS);
    EXPECT_EQ(installed_boot_control_guid_, GBL_EFI_BOOT_CONTROL_PROTOCOL_GUID);
    EXPECT_NE(installed_boot_control_protocol_, nullptr);
    return cleanup;
  }

  auto SetupEfiStateWithoutGpt() {
    auto cleanup = SetupEfiGlobalState(stub_service_, image_device_);
    stub_service_.AddDevice(&image_device_);

    EXPECT_EQ(InstallGblEfiBootControlProtocol(), EFI_SUCCESS);
    EXPECT_EQ(installed_boot_control_guid_, GBL_EFI_BOOT_CONTROL_PROTOCOL_GUID);
    EXPECT_NE(installed_boot_control_protocol_, nullptr);
    return cleanup;
  }

  GblEfiBootControlProtocol* protocol() const { return installed_boot_control_protocol_; }

 protected:
  MockStubService stub_service_;
  Device image_device_;
  BlockDevice block_device_;
  GblEfiBootControlProtocol* installed_boot_control_protocol_ = nullptr;
  efi_guid installed_boot_control_guid_ = {};
};

TEST_F(GblEfiBootControlProtocolTest, InstallAndRevision) {
  auto cleanup = SetupEfiStateWithoutGpt();
  ASSERT_NE(protocol(), nullptr);
  EXPECT_EQ(protocol()->revision, GBL_EFI_BOOT_CONTROL_PROTOCOL_REVISION);
  EXPECT_EQ(protocol()->handle_loaded_os, nullptr);
}

TEST_F(GblEfiBootControlProtocolTest, GetSlotCount) {
  auto cleanup = SetupEfiStateWithoutGpt();
  EXPECT_EQ(protocol()->get_slot_count(protocol(), nullptr), EFI_INVALID_PARAMETER);

  uint8_t slot_count = 0;
  EXPECT_EQ(protocol()->get_slot_count(protocol(), &slot_count), EFI_SUCCESS);
  EXPECT_EQ(slot_count, 2u);
}

TEST_F(GblEfiBootControlProtocolTest, GetSlotInfoValidAndInvalid) {
  auto cleanup = SetupEfiStateWithGpt();

  EXPECT_EQ(protocol()->get_slot_info(protocol(), 0, nullptr), EFI_INVALID_PARAMETER);

  GblEfiSlotInfo info = {};
  EXPECT_EQ(protocol()->get_slot_info(protocol(), 2, &info), EFI_INVALID_PARAMETER);
  EXPECT_EQ(protocol()->get_slot_info(protocol(), 255, &info), EFI_INVALID_PARAMETER);

  ASSERT_EQ(protocol()->get_slot_info(protocol(), 0, &info), EFI_SUCCESS);
  EXPECT_EQ(info.suffix, static_cast<uint32_t>('a'));
  EXPECT_EQ(info.priority, 7u);
  EXPECT_EQ(info.remaining_tries, kAbrMaxTriesRemaining);
  EXPECT_EQ(info.successful, 0u);
  EXPECT_EQ(info.unbootable_reason,
            GblEfiUnbootableReason::GBL_EFI_UNBOOTABLE_REASON_UNKNOWN_REASON);

  ASSERT_EQ(protocol()->get_slot_info(protocol(), 1, &info), EFI_SUCCESS);
  EXPECT_EQ(info.suffix, static_cast<uint32_t>('b'));
  EXPECT_EQ(info.priority, 6u);
}

TEST_F(GblEfiBootControlProtocolTest, GetSlotInfoUnbootableAndSuccessful) {
  auto cleanup = SetupEfiStateWithGpt();

  ZirconBootOps zb_ops = GetZirconBootOps();
  AbrOps abr_ops = GetAbrOpsFromZirconBootOps(&zb_ops);
  ASSERT_EQ(AbrMarkSlotSuccessful(&abr_ops, kAbrSlotIndexA, false), kAbrResultOk);
  ASSERT_EQ(AbrMarkSlotUnbootable(&abr_ops, kAbrSlotIndexB, kAbrUnbootableReasonOsRequested),
            kAbrResultOk);

  GblEfiSlotInfo info_a = {};
  ASSERT_EQ(protocol()->get_slot_info(protocol(), 0, &info_a), EFI_SUCCESS);
  EXPECT_EQ(info_a.suffix, static_cast<uint32_t>('a'));
  EXPECT_EQ(info_a.successful, 1u);

  GblEfiSlotInfo info_b = {};
  ASSERT_EQ(protocol()->get_slot_info(protocol(), 1, &info_b), EFI_SUCCESS);
  EXPECT_EQ(info_b.suffix, static_cast<uint32_t>('b'));
  EXPECT_EQ(info_b.unbootable_reason,
            GblEfiUnbootableReason::GBL_EFI_UNBOOTABLE_REASON_USER_REQUESTED);

  ASSERT_EQ(AbrMarkSlotUnbootable(&abr_ops, kAbrSlotIndexB, kAbrUnbootableReasonNoMoreTries),
            kAbrResultOk);
  ASSERT_EQ(protocol()->get_slot_info(protocol(), 1, &info_b), EFI_SUCCESS);
  EXPECT_EQ(info_b.unbootable_reason,
            GblEfiUnbootableReason::GBL_EFI_UNBOOTABLE_REASON_NO_MORE_TRIES);

  ASSERT_EQ(
      AbrMarkSlotUnbootable(&abr_ops, kAbrSlotIndexB, kAbrUnbootableReasonVerificationFailure),
      kAbrResultOk);
  ASSERT_EQ(protocol()->get_slot_info(protocol(), 1, &info_b), EFI_SUCCESS);
  EXPECT_EQ(info_b.unbootable_reason,
            GblEfiUnbootableReason::GBL_EFI_UNBOOTABLE_REASON_VERIFICATION_FAILURE);
}

TEST_F(GblEfiBootControlProtocolTest, GetSlotInfoNoGptReturnsDeviceError) {
  auto cleanup = SetupEfiStateWithoutGpt();
  GblEfiSlotInfo info = {};
  EXPECT_EQ(protocol()->get_slot_info(protocol(), 0, &info), EFI_DEVICE_ERROR);
  EXPECT_EQ(protocol()->get_slot_info(protocol(), 1, &info), EFI_DEVICE_ERROR);
}

TEST_F(GblEfiBootControlProtocolTest, GetCurrentSlot) {
  auto cleanup = SetupEfiStateWithGpt();

  EXPECT_EQ(protocol()->get_current_slot(protocol(), nullptr), EFI_INVALID_PARAMETER);

  GblEfiSlotInfo info = {};
  ASSERT_EQ(protocol()->get_current_slot(protocol(), &info), EFI_SUCCESS);
  EXPECT_EQ(info.suffix, static_cast<uint32_t>('a'));
  EXPECT_EQ(info.priority, 7u);
}

TEST_F(GblEfiBootControlProtocolTest, GetCurrentSlotNoGptReturnsDeviceError) {
  auto cleanup = SetupEfiStateWithoutGpt();
  GblEfiSlotInfo info = {};
  EXPECT_EQ(protocol()->get_current_slot(protocol(), &info), EFI_DEVICE_ERROR);
}

TEST_F(GblEfiBootControlProtocolTest, SetActiveSlot) {
  auto cleanup = SetupEfiStateWithGpt();

  EXPECT_EQ(protocol()->set_active_slot(protocol(), 2), EFI_INVALID_PARAMETER);

  ASSERT_EQ(protocol()->set_active_slot(protocol(), 1), EFI_SUCCESS);

  GblEfiSlotInfo current = {};
  ASSERT_EQ(protocol()->get_current_slot(protocol(), &current), EFI_SUCCESS);
  EXPECT_EQ(current.suffix, static_cast<uint32_t>('b'));
  EXPECT_EQ(current.priority, 7u);

  GblEfiSlotInfo info_a = {};
  ASSERT_EQ(protocol()->get_slot_info(protocol(), 0, &info_a), EFI_SUCCESS);
  EXPECT_EQ(info_a.priority, 6u);

  ASSERT_EQ(protocol()->set_active_slot(protocol(), 0), EFI_SUCCESS);
  ASSERT_EQ(protocol()->get_current_slot(protocol(), &current), EFI_SUCCESS);
  EXPECT_EQ(current.suffix, static_cast<uint32_t>('a'));
}

TEST_F(GblEfiBootControlProtocolTest, SetActiveSlotNoGptReturnsDeviceError) {
  auto cleanup = SetupEfiStateWithoutGpt();
  EXPECT_EQ(protocol()->set_active_slot(protocol(), 0), EFI_DEVICE_ERROR);
  EXPECT_EQ(protocol()->set_active_slot(protocol(), 1), EFI_DEVICE_ERROR);
}

TEST_F(GblEfiBootControlProtocolTest, GetOneShotBootMode) {
  auto cleanup = SetupEfiStateWithoutGpt();

  EXPECT_EQ(protocol()->get_one_shot_boot_mode(protocol(), nullptr), EFI_INVALID_PARAMETER);

  GblEfiOneShotBootMode mode = GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_BOOTLOADER;
  g_reboot_mode = RebootMode::kNormal;
  ASSERT_EQ(protocol()->get_one_shot_boot_mode(protocol(), &mode), EFI_SUCCESS);
  EXPECT_EQ(mode, GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_NONE);

  g_reboot_mode = RebootMode::kBootloader;
  ASSERT_EQ(protocol()->get_one_shot_boot_mode(protocol(), &mode), EFI_SUCCESS);
  EXPECT_EQ(mode, GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_BOOTLOADER);

  g_reboot_mode = RebootMode::kRecovery;
  ASSERT_EQ(protocol()->get_one_shot_boot_mode(protocol(), &mode), EFI_SUCCESS);
  EXPECT_EQ(mode, GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_RECOVERY);
}

}  // namespace
}  // namespace gigaboot
