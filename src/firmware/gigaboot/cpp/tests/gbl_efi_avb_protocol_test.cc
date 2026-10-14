// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstdarg>

#include <efi/types.h>
#include <gbl/gbl_efi_protocols.h>
#include <gbl/uefi/protocols/gbl_efi_avb_protocol.h>
#include <gtest/gtest.h>

#include "mock_boot_service.h"

namespace gigaboot {
namespace {

class GblEfiAvbProtocolTest : public ::testing::Test {
 public:
  GblEfiAvbProtocolTest() : image_device_({"path", "image"}) {}

  static EFIAPI efi_status CaptureAvbProtocol(efi_handle* handle, ...) {
    auto* test = static_cast<GblEfiAvbProtocolTest*>(gEfiImageHandle);
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
      test->installed_avb_guid_ = *guid;
    }
    test->installed_avb_protocol_ = static_cast<GblEfiAvbProtocol*>(protocol);
    return EFI_SUCCESS;
  }

  void SetUp() override {
    installed_avb_protocol_ = nullptr;
    installed_avb_guid_ = {};
    gEfiImageHandle = this;
    stub_service_.services()->InstallMultipleProtocolInterfaces = CaptureAvbProtocol;
  }

  void TearDown() override {
    gEfiImageHandle = nullptr;
    installed_avb_protocol_ = nullptr;
  }

  auto SetupEfiState() {
    auto cleanup = SetupEfiGlobalState(stub_service_, image_device_);

    EXPECT_EQ(InstallGblEfiAvbProtocol(), EFI_SUCCESS);
    EXPECT_EQ(installed_avb_guid_, GBL_EFI_AVB_PROTOCOL_GUID);
    EXPECT_NE(installed_avb_protocol_, nullptr);
    return cleanup;
  }

  GblEfiAvbProtocol* protocol() const { return installed_avb_protocol_; }

 protected:
  MockStubService stub_service_;
  Device image_device_;
  GblEfiAvbProtocol* installed_avb_protocol_ = nullptr;
  efi_guid installed_avb_guid_ = {};
};

TEST_F(GblEfiAvbProtocolTest, InstallAndRevision) {
  auto cleanup = SetupEfiState();
  ASSERT_NE(protocol(), nullptr);
  EXPECT_EQ(protocol()->revision, GBL_EFI_AVB_PROTOCOL_REVISION);
}

TEST_F(GblEfiAvbProtocolTest, ReadPartitionAttributes) {
  auto cleanup = SetupEfiState();
  EXPECT_EQ(protocol()->read_partition_attributes(protocol(), nullptr, nullptr),
            EFI_INVALID_PARAMETER);

  size_t num_partitions = 5;
  EXPECT_EQ(protocol()->read_partition_attributes(protocol(), &num_partitions, nullptr),
            EFI_SUCCESS);
  EXPECT_EQ(num_partitions, 0u);
}

TEST_F(GblEfiAvbProtocolTest, ReadDeviceStatus) {
  auto cleanup = SetupEfiState();
  EXPECT_EQ(protocol()->read_device_status(protocol(), nullptr), EFI_INVALID_PARAMETER);

  GblEfiAvbDeviceStatus status_flags = 0;
  ASSERT_EQ(protocol()->read_device_status(protocol(), &status_flags), EFI_SUCCESS);
  EXPECT_EQ(status_flags, GBL_EFI_AVB_DEVICE_STATUS_UNLOCKED);
}

TEST_F(GblEfiAvbProtocolTest, ValidateVbmetaPublicKey) {
  auto cleanup = SetupEfiState();
  EXPECT_EQ(protocol()->validate_vbmeta_public_key(protocol(), 0, nullptr, 0, nullptr, nullptr),
            EFI_INVALID_PARAMETER);

  GblEfiAvbKeyValidationStatus validation_status =
      GblEfiAvbKeyValidationStatus::GBL_EFI_AVB_KEY_VALIDATION_STATUS_INVALID;
  EXPECT_EQ(protocol()->validate_vbmeta_public_key(protocol(), 0, nullptr, 0, nullptr,
                                                   &validation_status),
            EFI_UNSUPPORTED);
}

TEST_F(GblEfiAvbProtocolTest, RollbackIndex) {
  auto cleanup = SetupEfiState();
  EXPECT_EQ(protocol()->read_rollback_index(protocol(), 0, nullptr), EFI_INVALID_PARAMETER);

  uint64_t rollback_index = 42;
  ASSERT_EQ(protocol()->read_rollback_index(protocol(), 1, &rollback_index), EFI_SUCCESS);
  EXPECT_EQ(rollback_index, 0u);

  EXPECT_EQ(protocol()->write_rollback_index(protocol(), 1, 10), EFI_SUCCESS);
}

TEST_F(GblEfiAvbProtocolTest, PersistentValues) {
  auto cleanup = SetupEfiState();
  const EfiChar8 kName[] = "test_key";
  size_t value_size = 0;
  uint8_t value = 0;

  EXPECT_EQ(protocol()->read_persistent_value(protocol(), nullptr, &value_size, &value),
            EFI_INVALID_PARAMETER);
  EXPECT_EQ(protocol()->read_persistent_value(protocol(), kName, nullptr, &value),
            EFI_INVALID_PARAMETER);
  EXPECT_EQ(protocol()->read_persistent_value(protocol(), kName, &value_size, &value),
            EFI_UNSUPPORTED);

  EXPECT_EQ(protocol()->write_persistent_value(protocol(), nullptr, 1, &value),
            EFI_INVALID_PARAMETER);
  EXPECT_EQ(protocol()->write_persistent_value(protocol(), kName, 1, &value), EFI_UNSUPPORTED);
}

TEST_F(GblEfiAvbProtocolTest, HandleVerificationResultAndLockState) {
  auto cleanup = SetupEfiState();
  EXPECT_EQ(protocol()->handle_verification_result(protocol(), nullptr), EFI_INVALID_PARAMETER);

  GblEfiAvbVerificationResult result = {};
  EXPECT_EQ(protocol()->handle_verification_result(protocol(), &result), EFI_SUCCESS);

  EXPECT_EQ(
      protocol()->write_lock_state(protocol(), GblEfiAvbLockType::GBL_EFI_AVB_LOCK_TYPE_DEVICE,
                                   GblEfiAvbLockState::GBL_EFI_AVB_LOCK_STATE_LOCKED),
      EFI_UNSUPPORTED);
  EXPECT_EQ(protocol()->factory_data_reset(protocol()), EFI_UNSUPPORTED);
}

}  // namespace
}  // namespace gigaboot
