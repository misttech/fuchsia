// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/efi/testing/stub_boot_services.h>

#include <array>
#include <cstdarg>
#include <cstring>
#include <map>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <efi/types.h>
#include <gbl/gbl_efi_protocols.h>
#include <gbl/uefi/protocols/gbl_efi_fastboot_protocol.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <phys/efi/main.h>

namespace gigaboot {
namespace {

using ::efi::MatchGuid;
using ::efi::MockBootServices;
using ::testing::_;
using ::testing::NiceMock;
using ::testing::Return;
using ::testing::Test;

constexpr efi_guid kGblEfiFastbootProtocolGuid = {
    0xc67e48a0, 0x5eb8, 0x4127, {0xbe, 0x89, 0xdf, 0x2e, 0xd9, 0x3d, 0x8a, 0x9a}};

class GblEfiFastbootProtocolTest : public Test {
 public:
  static EFIAPI efi_status FakeInstallMultipleProtocolInterfaces(efi_handle* handle, ...) {
    auto* test = static_cast<GblEfiFastbootProtocolTest*>(gEfiImageHandle);
    if (!handle || !test) {
      return EFI_INVALID_PARAMETER;
    }

#if defined(__x86_64__)
    __builtin_ms_va_list ap;
    __builtin_ms_va_start(ap, handle);
#else
    va_list ap;
    va_start(ap, handle);
#endif
    while (const efi_guid* guid = va_arg(ap, const efi_guid*)) {
      void* protocol = va_arg(ap, void*);
      test->installed_protocols_[*guid] = protocol;
    }
#if defined(__x86_64__)
    __builtin_ms_va_end(ap);
#else
    va_end(ap);
#endif

    if (*handle == nullptr) {
      *handle = reinterpret_cast<efi_handle>(0x1);
    }
    return EFI_SUCCESS;
  }

  void SetUp() override {
    installed_protocols_.clear();
    gEfiImageHandle = this;

    mock_boot_services_.services()->InstallMultipleProtocolInterfaces =
        &FakeInstallMultipleProtocolInterfaces;

    ON_CALL(mock_boot_services_, LocateProtocol(_, _, _))
        .WillByDefault(
            [this](const efi_guid* protocol, void* registration, void** intf) -> efi_status {
              if (!protocol || !intf) {
                return EFI_INVALID_PARAMETER;
              }
              auto it = installed_protocols_.find(*protocol);
              if (it == installed_protocols_.end()) {
                return EFI_NOT_FOUND;
              }
              *intf = it->second;
              return EFI_SUCCESS;
            });

    system_table_ = efi_system_table{
        .BootServices = mock_boot_services_.services(),
    };
    gEfiSystemTable = &system_table_;

    ASSERT_EQ(InstallGblEfiFastbootProtocol(), EFI_SUCCESS);
    ASSERT_EQ(gEfiSystemTable->BootServices->LocateProtocol(&kGblEfiFastbootProtocolGuid, nullptr,
                                                            reinterpret_cast<void**>(&protocol_)),
              EFI_SUCCESS);
    ASSERT_NE(protocol_, nullptr);
  }

  void TearDown() override {
    protocol_ = nullptr;
    gEfiSystemTable = nullptr;
    gEfiImageHandle = nullptr;
    installed_protocols_.clear();
  }

 protected:
  NiceMock<MockBootServices> mock_boot_services_;
  efi_system_table system_table_ = {};
  std::map<efi_guid, void*> installed_protocols_;
  GblEfiFastbootProtocol* protocol_ = nullptr;
};

TEST_F(GblEfiFastbootProtocolTest, InstallAndLocateProtocol) {
  EXPECT_EQ(protocol_->revision, GBL_EFI_FASTBOOT_PROTOCOL_REVISION);
  EXPECT_NE(protocol_->get_var, nullptr);
  EXPECT_NE(protocol_->get_var_all, nullptr);
  EXPECT_EQ(protocol_->get_staged, nullptr);
  EXPECT_NE(protocol_->command_exec, nullptr);
  EXPECT_EQ(protocol_->get_partition_type, nullptr);
}

TEST_F(GblEfiFastbootProtocolTest, GetVarHwRevisionSuccess) {
  constexpr const char* kVarName = "hw-revision";
  std::array<const EfiChar8*, 1> args = {reinterpret_cast<const EfiChar8*>(kVarName)};

  std::array<EfiChar8, 32> buffer;
  buffer.fill(0xff);
  size_t buffer_size = buffer.size();

  ASSERT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), &buffer_size, buffer.data()),
            EFI_SUCCESS);
  EXPECT_EQ(buffer_size, std::strlen(BOARD_NAME));
  EXPECT_EQ(std::string_view(reinterpret_cast<const char*>(buffer.data()), buffer_size),
            std::string_view(BOARD_NAME));
  EXPECT_EQ(buffer[buffer_size], 0);
}

TEST_F(GblEfiFastbootProtocolTest, GetVarInvalidParameter) {
  constexpr const char* kVarName = "hw-revision";
  std::array<const EfiChar8*, 1> args = {reinterpret_cast<const EfiChar8*>(kVarName)};
  std::array<EfiChar8, 32> buffer{};
  size_t buffer_size = buffer.size();

  // Empty args (num_args == 0).
  EXPECT_EQ(protocol_->get_var(protocol_, 0, args.data(), &buffer_size, buffer.data()),
            EFI_INVALID_PARAMETER);

  // Null buffer_size pointer.
  EXPECT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), nullptr, buffer.data()),
            EFI_INVALID_PARAMETER);
}

TEST_F(GblEfiFastbootProtocolTest, GetVarBufferTooSmall) {
  constexpr const char* kVarName = "hw-revision";
  std::array<const EfiChar8*, 1> args = {reinterpret_cast<const EfiChar8*>(kVarName)};
  std::array<EfiChar8, 32> buffer{};

  // Zero-sized buffer.
  size_t buffer_size = 0;
  EXPECT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), &buffer_size, buffer.data()),
            EFI_BUFFER_TOO_SMALL);

  // Buffer size equal to strlen(BOARD_NAME) (missing space for null terminator).
  buffer_size = std::strlen(BOARD_NAME);
  EXPECT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), &buffer_size, buffer.data()),
            EFI_BUFFER_TOO_SMALL);

  // Exact minimum buffer size (strlen(BOARD_NAME) + 1) succeeds.
  buffer_size = std::strlen(BOARD_NAME) + 1;
  EXPECT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), &buffer_size, buffer.data()),
            EFI_SUCCESS);
  EXPECT_EQ(buffer_size, std::strlen(BOARD_NAME));
}

TEST_F(GblEfiFastbootProtocolTest, GetVarNotFound) {
  constexpr const char* kVarName = "nonexistent-variable";
  std::array<const EfiChar8*, 1> args = {reinterpret_cast<const EfiChar8*>(kVarName)};
  std::array<EfiChar8, 32> buffer{};
  size_t buffer_size = buffer.size();

  EXPECT_EQ(protocol_->get_var(protocol_, args.size(), args.data(), &buffer_size, buffer.data()),
            EFI_NOT_FOUND);
}

TEST_F(GblEfiFastbootProtocolTest, GetVarAll) {
  struct VarEntry {
    std::vector<std::string> args;
    std::string val;
  };
  std::vector<VarEntry> entries;

  auto callback = [](void* context, size_t num_args, const EfiChar8* const* args,
                     const EfiChar8* val) {
    auto* out = static_cast<std::vector<VarEntry>*>(context);
    VarEntry entry;
    for (size_t i = 0; i < num_args; ++i) {
      entry.args.emplace_back(reinterpret_cast<const char*>(args[i]));
    }
    entry.val = reinterpret_cast<const char*>(val);
    out->push_back(std::move(entry));
  };

  ASSERT_EQ(protocol_->get_var_all(protocol_, &entries, callback), EFI_SUCCESS);
  ASSERT_EQ(entries.size(), 1u);
  ASSERT_EQ(entries[0].args.size(), 1u);
  EXPECT_EQ(entries[0].args[0], "hw-revision");
  EXPECT_EQ(entries[0].val, BOARD_NAME);
}

TEST_F(GblEfiFastbootProtocolTest, CommandExecDefaultImpl) {
  GblEfiFastbootCommandExecResult implementation =
      GblEfiFastbootCommandExecResult::GBL_EFI_FASTBOOT_COMMAND_EXEC_RESULT_PROHIBITED;

  ASSERT_EQ(protocol_->command_exec(protocol_, 0, nullptr, 0, 0, nullptr, &implementation, nullptr,
                                    nullptr),
            EFI_SUCCESS);
  EXPECT_EQ(implementation,
            GblEfiFastbootCommandExecResult::GBL_EFI_FASTBOOT_COMMAND_EXEC_RESULT_DEFAULT_IMPL);
}

}  // namespace
}  // namespace gigaboot
