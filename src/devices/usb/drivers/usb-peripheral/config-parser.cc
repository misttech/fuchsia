// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/config-parser.h"

#include <lib/driver/logging/cpp/logger.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstdint>
#include <map>
#include <string>
#include <vector>

namespace usb_peripheral {

namespace {

struct FunctionDefinition {
  uint8_t tag_mask;
  fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor descriptor;
  std::string_view description;
};

const std::map<std::string_view, FunctionDefinition> all_functions = {
    {"cdc",
     {
         .tag_mask = kCdcMask,
         .descriptor = kCDCFunctionDescriptor,
         .description = kCDCProductDescription,
     }},
    {"ums",
     {
         .tag_mask = kUmsMask,
         .descriptor = kUMSFunctionDescriptor,
         .description = kUMSProductDescription,
     }},
    {"rndis",
     {
         .tag_mask = kRndisMask,
         .descriptor = kRNDISFunctionDescriptor,
         .description = kRNDISProductDescription,
     }},
    {"adb",
     {
         .tag_mask = kAdbMask,
         .descriptor = kADBFunctionDescriptor,
         .description = kADBProductDescription,
     }},
    {"fastboot",
     {
         .tag_mask = kFastbootMask,
         .descriptor = kFastbootFunctionDescriptor,
         .description = kFastbootProductDescription,
     }},
    {"test",
     {
         .tag_mask = kTestMask,
         .descriptor = kTestFunctionDescriptor,
         .description = kTestProductDescription,
     }},
    {"vsock_bridge",
     {
         .tag_mask = kVsockBridgeMask,
         .descriptor = kFfxFunctionDescriptor,
         .description = kVsockBridgeProductDescription,
     }},
};

}  // namespace

zx_status_t PeripheralConfigParser::AddFunctions(const std::vector<std::string>& functions) {
  if (functions.empty()) {
    fdf::info("No functions found");
    return ZX_OK;
  }

  // resolve and then sort the functions by their product ids so that they are added and combined in
  // a predictable order.
  std::vector<FunctionDefinition> function_defs;
  for (const auto& function : functions) {
    auto function_def = all_functions.find(function);
    if (function_def != all_functions.end()) {
      if (!kPidLookup.contains(function_def->second.tag_mask)) {
        fdf::error("No supported USB PID for function {}", function);
        return ZX_ERR_INVALID_ARGS;
      }
      function_defs.push_back(function_def->second);
    } else {
      fdf::error("Function not supported: {}", function);
      return ZX_ERR_INVALID_ARGS;
    }
  }

  std::ranges::sort(function_defs, [](FunctionDefinition& left, FunctionDefinition& right) {
    return kPidLookup.at(left.tag_mask) < kPidLookup.at(right.tag_mask);
  });

  zx_status_t status = ZX_OK;
  for (const auto& function : function_defs) {
    function_configs_.push_back(function.descriptor);
    status = SetCompositeProductDescription(function.tag_mask, function.description);

    if (status != ZX_OK) {
      fdf::error("Failed to set product description for function: {}", function.description);
      return status;
    }
  }

  return ZX_OK;
}

zx_status_t PeripheralConfigParser::SetCompositeProductDescription(uint8_t tag_mask,
                                                                   const std::string_view& desc) {
  if (tag_ & tag_mask) {
    fdf::error("Duplicate function for {}", desc);
    return ZX_ERR_WRONG_TYPE;
  }

  tag_ |= tag_mask;

  if (!kPidLookup.contains(tag_)) {
    fdf::error("No matching pid for this combination of functions: {:#x}", tag_);
    return ZX_ERR_WRONG_TYPE;
  }

  pid_ = kPidLookup.at(tag_);

  if (product_desc_.empty()) {
    product_desc_ += desc;
  } else {
    product_desc_ += kCompositeDeviceConnector;
    product_desc_ += desc;
  }

  return ZX_OK;
}

}  // namespace usb_peripheral
