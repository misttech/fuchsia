// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/config-parser.h"

#include <lib/ddk/debug.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <cstdint>
#include <string>
#include <vector>

namespace usb_peripheral {

zx_status_t PeripheralConfigParser::AddFunctions(const std::vector<std::string>& functions) {
  if (functions.empty()) {
    zxlogf(INFO, "No functions found");
    return ZX_OK;
  }

  zx_status_t status = ZX_OK;
  for (const auto& function : functions) {
    if (function == "cdc") {
      function_configs_.push_back(kCDCFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_CDC_PID);

    } else if (function == "ums") {
      function_configs_.push_back(kUMSFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_UMS_PID);

    } else if (function == "rndis") {
      function_configs_.push_back(kRNDISFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_RNDIS_PID);

    } else if (function == "adb") {
      function_configs_.push_back(kADBFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_ADB_PID);

    } else if (function == "overnet") {
      function_configs_.push_back(kOvernetFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_OVERNET_PID);

    } else if (function == "fastboot") {
      function_configs_.push_back(kFastbootFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_FASTBOOT_PID);

    } else if (function == "test") {
      function_configs_.push_back(kTestFunctionDescriptor);
      status = SetCompositeProductDescription(GOOGLE_USB_FUNCTION_TEST_PID);

    } else {
      zxlogf(ERROR, "Function not supported: %s", function.c_str());
      return ZX_ERR_INVALID_ARGS;
    }

    if (status != ZX_OK) {
      zxlogf(ERROR, "Failed to set product description for %s", function.c_str());
      return status;
    }
  }

  return ZX_OK;
}

zx_status_t PeripheralConfigParser::SetCompositeProductDescription(uint16_t pid) {
  if (pid_ == 0) {
    switch (pid) {
      case GOOGLE_USB_CDC_PID:
        product_desc_ = kCDCProductDescription;
        break;
      case GOOGLE_USB_RNDIS_PID:
        product_desc_ = kRNDISProductDescription;
        break;
      case GOOGLE_USB_UMS_PID:
        product_desc_ = kUMSProductDescription;
        break;
      case GOOGLE_USB_ADB_PID:
        product_desc_ = kADBProductDescription;
        break;
      case GOOGLE_USB_OVERNET_PID:
        product_desc_ = kOvernetProductDescription;
        break;
      case GOOGLE_USB_FASTBOOT_PID:
        product_desc_ = kFastbootProductDescription;
        break;
      case GOOGLE_USB_FUNCTION_TEST_PID:
        product_desc_ = kTestProductDescription;
        break;
      default:
        zxlogf(ERROR, "Invalid pid: %d", pid);
        return ZX_ERR_WRONG_TYPE;
    }
    pid_ = pid;
  } else {
    product_desc_ += kCompositeDeviceConnector;
    if (pid_ == GOOGLE_USB_CDC_PID && pid == GOOGLE_USB_FUNCTION_TEST_PID) {
      pid_ = GOOGLE_USB_CDC_AND_FUNCTION_TEST_PID;
      product_desc_ += kTestProductDescription;
    } else if (pid_ == GOOGLE_USB_CDC_PID && pid == GOOGLE_USB_ADB_PID) {
      pid_ = GOOGLE_USB_CDC_AND_ADB_PID;
      product_desc_ += kADBProductDescription;
    } else if (pid_ == GOOGLE_USB_CDC_PID && pid == GOOGLE_USB_OVERNET_PID) {
      pid_ = GOOGLE_USB_CDC_AND_OVERNET_PID;
      product_desc_ += kOvernetProductDescription;
    } else if (pid_ == GOOGLE_USB_CDC_PID && pid == GOOGLE_USB_FASTBOOT_PID) {
      pid_ = GOOGLE_USB_CDC_AND_FASTBOOT_PID;
      product_desc_ += kFastbootProductDescription;
    } else {
      zxlogf(ERROR, "No matching pid for this combination: %d + %d", pid_, pid);
      return ZX_ERR_WRONG_TYPE;
    }
  }
  return ZX_OK;
}

}  // namespace usb_peripheral
