// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "gbl_efi_fastboot_protocol.h"

#include <zircon/assert.h>

#include <functional>
#include <span>
#include <string_view>

#include <phys/efi/main.h>

#include "utils.h"

#define GBL_EFI_FASTBOOT_PROTOCOL_GUID \
  {0xc67e48a0, 0x5eb8, 0x4127, {0xbe, 0x89, 0xdf, 0x2e, 0xd9, 0x3d, 0x8a, 0x9a}}

namespace {

// Contains information such as variable name and value.
constexpr struct Variable {
  const char* var_name;
  // For now we only consider constant variable.
  const char* var_impl;

  /// Gets the name as a string_view.
  std::string_view name() const { return std::string_view(var_name); }

  /// Gets the value as a string_view.
  std::string_view impl() const { return std::string_view(var_impl); }
} kVariables[] = {
    {"hw-revision", BOARD_NAME},
};

/// Gets the list of variables
std::span<const Variable> variables() { return std::span<const Variable>(kVariables); }

efi_status GetVar(struct GblEfiFastbootProtocol* self, const char* const* args, size_t num_args,
                  uint8_t* buf, size_t* bufsize) {
  const std::span<const char* const> args_span{args, num_args};
  if (args_span.empty() || !bufsize) {
    return EFI_INVALID_PARAMETER;
  }

  std::span<uint8_t> out{buf, *bufsize};
  for (size_t i = 0; i < variables().size(); i++) {
    const Variable& var = variables()[i];
    if (std::string_view(args_span[0]) != var.name()) {
      continue;
    }

    if (out.size() < var.impl().size() + 1) {
      return EFI_BUFFER_TOO_SMALL;
    }
    memcpy(out.data(), var.impl().data(), var.impl().size());
    *bufsize = var.impl().size();
    out.data()[*bufsize] = 0;
    return EFI_SUCCESS;
  }
  return EFI_NOT_FOUND;
}

efi_status GetVarAll(struct GblEfiFastbootProtocol* self, void* ctx, GetVarAllCallback cb) {
  for (size_t i = 0; i < variables().size(); i++) {
    std::array args{variables()[i].name().data()};
    cb(ctx, args.data(), args.size(), variables()[i].impl().data());
  }
  return EFI_SUCCESS;
}

EfiStatus CommandExec(struct GblEfiFastbootProtocol* self, size_t num_args, const char* const* args,
                      size_t download_data_used_len, uint8_t* download_data,
                      size_t download_data_full_size,
                      GblEfiFastbootCommandExecResult* implementation, FastbootMessageSender sender,
                      void* ctx) {
  *implementation =
      GblEfiFastbootCommandExecResult::GBL_EFI_FASTBOOT_COMMAND_EXEC_RESULT_DEFAULT_IMPL;
  return EFI_SUCCESS;
}

GblEfiFastbootProtocol protocol = {
    .revision = GBL_EFI_FASTBOOT_PROTOCOL_REVISION,
    .get_var = GetVar,
    .get_var_all = GetVarAll,
    .get_staged = NULL,
    .set_lock = NULL,
    .get_lock = NULL,
    .vendor_erase = NULL,
    .command_exec = CommandExec,
    .start_local_session = NULL,
    .update_local_session = NULL,
    .close_local_session = NULL,
};

efi_guid guid = GBL_EFI_FASTBOOT_PROTOCOL_GUID;

}  // namespace

namespace gigaboot {
efi_status InstallGblEfiFastbootProtocol() {
  efi_handle out_handle = NULL;
  return gEfiSystemTable->BootServices->InstallMultipleProtocolInterfaces(&out_handle, &guid,
                                                                          &protocol, NULL);
}
}  // namespace gigaboot
