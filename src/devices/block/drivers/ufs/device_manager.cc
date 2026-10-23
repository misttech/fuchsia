// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ufs/device_manager.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>

#include "src/devices/block/drivers/ufs/registers.h"
#include "src/devices/block/drivers/ufs/transfer_request_processor.h"
#include "src/devices/block/drivers/ufs/ufs.h"
#include "src/devices/block/drivers/ufs/upiu/upiu_transactions.h"
#include "transfer_request_processor.h"

namespace ufs {
zx::result<std::unique_ptr<DeviceManager>> DeviceManager::Create(
    Ufs &controller, TransferRequestProcessor &transfer_request_processor,
    InspectProperties &properties) {
  fbl::AllocChecker ac;
  auto device_manager = fbl::make_unique_checked<DeviceManager>(
      &ac, controller, transfer_request_processor, properties);
  if (!ac.check()) {
    fdf::error("Failed to allocate device manager.");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(device_manager));
}

zx::result<> DeviceManager::SendLinkStartUp() {
  if (zx::result<> result = controller_.Notify(NotifyEvent::kPreLinkStartup, 0);
      result.is_error()) {
    fdf::error("PreLinkStartup callback failed: {}", result);
    return result.take_error();
  }

  constexpr size_t kMaxLinkStartUpAttempts = 3;
  for (size_t attempt = 1; attempt <= kMaxLinkStartUpAttempts; ++attempt) {
    DmeLinkStartUpUicCommand link_startup_command(controller_);
    auto result = link_startup_command.SendCommand();
    if (result.is_ok()) {
      {
        std::lock_guard<std::mutex> lock(power_lock_);
        current_link_state_ = LinkState::kActive;
      }
      // Delivered here rather than from the UIC command's post-processing so that the callback
      // runs with uic_lock() released and may itself issue DME commands.
      return controller_.Notify(NotifyEvent::kPostLinkStartup, 0);
    }
    fdf::warn("UFS link startup attempt {}/{} failed: {}", attempt, kMaxLinkStartUpAttempts,
              result.status_string());
    if (attempt < kMaxLinkStartUpAttempts) {
      zx::nanosleep(zx::deadline_after(zx::msec(10)));
    }
  }

  fdf::error("Failed to startup UFS link after {} attempts", kMaxLinkStartUpAttempts);
  return zx::error(ZX_ERR_INTERNAL);
}

zx::result<> DeviceManager::DeviceInit() {
  zx::time device_init_start_time = zx::clock::get_monotonic();
  if (zx::result<> result = SetFlag(Flags::fDeviceInit); result.is_error()) {
    return result.take_error();
  }

  zx::time device_init_time_out = device_init_start_time + zx::usec(kDeviceInitTimeoutUs);
  while (true) {
    zx::result<uint8_t> flag = ReadFlag(Flags::fDeviceInit);
    if (flag.is_error()) {
      return flag.take_error();
    }

    if (!flag.value())
      break;

    if (zx::clock::get_monotonic() > device_init_time_out) {
      fdf::error("Wait for fDeviceInit timed out");
      return zx::error(ZX_ERR_TIMED_OUT);
    }
    zx::nanosleep(zx::deadline_after(kDeviceInitPollInterval));
  }
  return zx::ok();
}

zx::result<> DeviceManager::GetControllerDescriptor() {
  auto device_descriptor = ReadDescriptor<DeviceDescriptor>(DescriptorType::kDevice);
  if (device_descriptor.is_error()) {
    return device_descriptor.take_error();
  }
  device_descriptor_ = device_descriptor.value();

  // The field definitions for VersionReg and wSpecVersion are the same.
  // wSpecVersion use big-endian byte ordering.
  auto version = VersionReg::Get().FromValue(betoh16(device_descriptor_.wSpecVersion));
  fdf::info("UFS device version {}.{}{}", version.major_version_number(),
            version.minor_version_number(), version.version_suffix());

  fdf::info("{} enabled LUNs found", device_descriptor_.bNumberLU);

  auto geometry_descriptor = ReadDescriptor<GeometryDescriptor>(DescriptorType::kGeometry);
  if (geometry_descriptor.is_error()) {
    return geometry_descriptor.take_error();
  }
  geometry_descriptor_ = geometry_descriptor.value();

  // TODO(https://fxbug.dev/42075643): We need to functionalize the code to get max_lun_count_.
  if (geometry_descriptor_.bMaxNumberLU == 0) {
    max_lun_count_ = 8;
  } else if (geometry_descriptor_.bMaxNumberLU == 1) {
    max_lun_count_ = 32;
  } else {
    fdf::error("Invalid Geometry Descriptor bMaxNumberLU value={}",
               geometry_descriptor_.bMaxNumberLU);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // The kDeviceDensityUnit is defined in the spec as 512.
  // qTotalRawDeviceCapacity use big-endian byte ordering.
  constexpr uint32_t kDeviceDensityUnit = 512;
  fdf::info("UFS device total size is {} bytes",
            betoh64(geometry_descriptor_.qTotalRawDeviceCapacity) * kDeviceDensityUnit);

  return zx::ok();
}

zx::result<uint32_t> DeviceManager::ReadAttribute(Attributes attribute, uint8_t index) {
  ReadAttributeUpiu read_attribute_upiu(attribute, index);
  auto query_response = req_processor_.SendQueryRequestUpiu(read_attribute_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  return zx::ok(query_response->GetResponse<AttributeResponseUpiu>().GetAttribute());
}

zx::result<> DeviceManager::WriteAttribute(Attributes attribute, uint32_t value, uint8_t index) {
  WriteAttributeUpiu write_attribute_upiu(attribute, value, index);
  auto query_response = req_processor_.SendQueryRequestUpiu(write_attribute_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  return zx::ok();
}

template <typename DescriptorReturnType>
zx::result<DescriptorReturnType> DeviceManager::ReadDescriptor(DescriptorType descriptor,
                                                               uint8_t index) {
  ReadDescriptorUpiu read_descriptor_upiu(descriptor, index, sizeof(DescriptorReturnType));
  auto query_response = req_processor_.SendQueryRequestUpiu(read_descriptor_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  DescriptorReturnType data;
  CustomMemCpy(
      &data,
      &query_response->GetResponse<DescriptorResponseUpiu>().GetDescriptor<DescriptorReturnType>(),
      sizeof(data));
  return zx::ok(data);
}

zx::result<uint8_t> DeviceManager::ReadFlag(Flags type) {
  ReadFlagUpiu read_flag_upiu(type);
  auto query_response = req_processor_.SendQueryRequestUpiu(read_flag_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  return zx::ok(query_response->GetResponse<FlagResponseUpiu>().GetFlag());
}

zx::result<> DeviceManager::SetFlag(Flags type) {
  SetFlagUpiu set_flag_upiu(type);
  auto query_response = req_processor_.SendQueryRequestUpiu(set_flag_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  return zx::ok();
}

zx::result<> DeviceManager::ClearFlag(Flags type) {
  ClearFlagUpiu claer_flag_upiu(type);
  auto query_response = req_processor_.SendQueryRequestUpiu(claer_flag_upiu);
  if (query_response.is_error()) {
    return query_response.take_error();
  }
  return zx::ok();
}

zx::result<uint32_t> DeviceManager::DmeGet(uint16_t mib_attribute) {
  DmeGetUicCommand dme_get_command(controller_, mib_attribute, 0);
  auto value = dme_get_command.SendCommand();
  if (value.is_error()) {
    return value.take_error();
  }
  if (!value.value().has_value()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  return zx::ok(value.value().value());
}

zx::result<uint32_t> DeviceManager::DmePeerGet(uint16_t mib_attribute) {
  DmePeerGetUicCommand dme_peer_get_command(controller_, mib_attribute, 0);
  auto value = dme_peer_get_command.SendCommand();
  if (value.is_error()) {
    return value.take_error();
  }
  if (!value.value().has_value()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  return zx::ok(value.value().value());
}

zx::result<> DeviceManager::DmeSet(uint16_t mib_attribute, uint32_t value) {
  DmeSetUicCommand dme_set_command(controller_, mib_attribute, 0, AttrSetType::kVolatile, value);
  if (auto result = dme_set_command.SendCommand(); result.is_error()) {
    return result.take_error();
  }
  return zx::ok();
}

zx::result<uint32_t> DeviceManager::GetBootLunEnabled() {
  // Read bBootLunEn to confirm device interface is ok.
  zx::result<uint32_t> boot_lun_enabled = ReadAttribute(Attributes::bBootLunEn);
  if (boot_lun_enabled.is_error()) {
    return boot_lun_enabled.take_error();
  }
  fdf::debug("bBootLunEn 0x{:x}", boot_lun_enabled.value());
  return zx::ok(boot_lun_enabled.value());
}

zx::result<UnitDescriptor> DeviceManager::ReadUnitDescriptor(uint8_t lun) {
  return ReadDescriptor<UnitDescriptor>(DescriptorType::kUnit, lun);
}

zx::result<RpmbUnitDescriptor> DeviceManager::ReadRpmbUnitDescriptor() {
  return ReadDescriptor<RpmbUnitDescriptor>(DescriptorType::kUnit,
                                            static_cast<uint8_t>(WellKnownLuns::kRpmb));
}

zx::result<> DeviceManager::SetExceptionEventControl(ExceptionEventControl control) {
  if (exception_event_control_.value == control.value) {
    return zx::ok();
  }

  if (auto result = WriteAttribute(Attributes::wExceptionEventControl, control.value);
      result.is_error()) {
    return result.take_error();
  }
  exception_event_control_.value = control.value;

  return zx::ok();
}

zx::result<ExceptionEventStatus> DeviceManager::GetExceptionEventStatus() {
  auto ee_status_attribute = ReadAttribute(Attributes::wExceptionEventStatus);
  if (ee_status_attribute.is_error()) {
    return ee_status_attribute.take_error();
  }

  ExceptionEventStatus ee_status = {safemath::checked_cast<uint16_t>(ee_status_attribute.value())};
  return zx::ok(ee_status);
}

zx::result<> DeviceManager::PostExceptionEventsTask() {
  zx_status_t status = async::PostTask(controller_.exception_event_dispatcher().async_dispatcher(),
                                       [this] { HandleExceptionEvents(); });
  if (status != ZX_OK) {
    fdf::error("Failed to post Exception Event task: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok();
}

void DeviceManager::HandleExceptionEvents() {
  // An EVENT_ALERT can arrive on the very response (e.g. START STOP UNIT) that a suspend is
  // processing, and this task runs on its own dispatcher. While a power transition is in progress
  // the device is either about to be powered down or still being re-initialized, and SuspendPower()
  // / ResumePower() own the background-operation and exception-event state; skip the event
  // (ResumePower() re-enables background operations anyway).
  if (controller_.IsHardwarePoweredOff() || !IsResumed()) {
    return;
  }
  zx::result<ExceptionEventStatus> ee_status = GetExceptionEventStatus();
  if (ee_status.is_error()) {
    fdf::error("Failed to get Exception Event Status");
    return;
  }
  if (ee_status->urgent_bkops()) {
    if (auto result = HandleBackgroundOpEvent(); result.is_error()) {
      fdf::error("Failed to handle Background Operations Event");
    }
  }
  if (ee_status->too_high_temp() || ee_status->too_low_temp()) {
    // TODO(b/42075643): Implement temp exception handler.
    fdf::info("A temperature exception has occurred");
  }
}

zx::result<> DeviceManager::HandleBackgroundOpEvent() {
  zx::result<BackgroundOpStatus> bkop_status = GetBackgroundOpStatus();
  if (bkop_status.is_error()) {
    return bkop_status.take_error();
  }

  if (bkop_status.value() >= urgent_bkop_threshold_) {
    if (zx::result result = EnableBackgroundOp(); result.is_error()) {
      return result.take_error();
    }
  }
  return zx::ok();
}

zx::result<> DeviceManager::ConfigureWriteProtect(inspect::Node &wp_node) {
  zx::result<uint8_t> flag = ReadFlag(Flags::fPowerOnWPEn);
  if (flag.is_error()) {
    return flag.take_error();
  }
  is_power_on_write_protect_enabled_ = flag.value();
  properties_.is_power_on_write_protect_enabled =
      wp_node.CreateBool("is_power_on_write_protect_enabled", is_power_on_write_protect_enabled_);
  properties_.logical_lun_power_on_write_protect =
      wp_node.CreateBool("logical_lun_power_on_write_protect", logical_lun_power_on_write_protect_);
  return zx::ok();
}

void DeviceManager::SetLogicalLunPowerOnWriteProtect(bool value) {
  logical_lun_power_on_write_protect_ = value;
  properties_.logical_lun_power_on_write_protect.Set(value);
}

zx::result<> DeviceManager::ConfigureBackgroundOp(inspect::Node &bkop_node) {
  zx::result<uint8_t> flag = ReadFlag(Flags::fBackgroundOpsEn);
  if (flag.is_error()) {
    return flag.take_error();
  }
  is_background_op_enabled_ = flag.value();
  properties_.is_background_op_enabled =
      bkop_node.CreateBool("is_background_op_enabled", is_background_op_enabled_);

  // Currently we allow background operations in the active state. This may have a performance
  // penalty.
  // TODO(b/42075643): We should only perform background operations in the power suspended state.
  if (zx::result<> result = EnableBackgroundOp(); result.is_error()) {
    return result.take_error();
  }

  // For stable performance, set threshold of the Background Operation to `Required, not critical`.
  urgent_bkop_threshold_ = BackgroundOpStatus::kRequiredNotCritical;

  return zx::ok();
}

zx::result<> DeviceManager::EnableBackgroundOp() {
  if (is_background_op_enabled_) {
    return zx::ok();
  }

  if (zx::result<> result = SetFlag(Flags::fBackgroundOpsEn); result.is_error()) {
    return result.take_error();
  }
  is_background_op_enabled_ = true;
  properties_.is_background_op_enabled.Set(is_background_op_enabled_);

  // No need for urgent background operation exceptions.
  ExceptionEventControl control = {exception_event_control_.value};
  control.set_urgent_bkops_en(false);
  if (auto result = SetExceptionEventControl(control); result.is_error()) {
    fdf::warn("Failed to set exception event control: {}", result.status_string());
  }
  return zx::ok();
}

zx::result<> DeviceManager::DisableBackgroundOp() {
  if (!is_background_op_enabled_) {
    return zx::ok();
  }

  // Need urgent background operation exceptions.
  ExceptionEventControl control = {exception_event_control_.value};
  control.set_urgent_bkops_en(true);
  if (auto result = SetExceptionEventControl(control); result.is_error()) {
    fdf::warn("Failed to set exception event control: {}", result.status_string());
  }

  if (zx::result<> result = ClearFlag(Flags::fBackgroundOpsEn); result.is_error()) {
    return result.take_error();
  }
  is_background_op_enabled_ = false;
  properties_.is_background_op_enabled.Set(is_background_op_enabled_);

  return zx::ok();
}

zx::result<BackgroundOpStatus> DeviceManager::GetBackgroundOpStatus() {
  zx::result<uint32_t> bkop_status_attribute = ReadAttribute(Attributes::bBackgroundOpStatus);
  if (bkop_status_attribute.is_error()) {
    return bkop_status_attribute.take_error();
  }
  if (bkop_status_attribute.value() > BackgroundOpStatus::kCritical) {
    fdf::error("Invalid BackgroundOpStatus: {}", bkop_status_attribute.value());
    return zx::error(ZX_ERR_BAD_STATE);
  }
  BackgroundOpStatus background_op_status =
      static_cast<BackgroundOpStatus>(bkop_status_attribute.value());
  return zx::ok(background_op_status);
}

zx::result<> DeviceManager::ConfigureWriteBooster(inspect::Node &wb_node) {
  // Copy to access the unaligned value.
  const ExtendedUfsFeaturesSupport kExtendedUfsFeaturesSupport{
      betoh32(device_descriptor_.dExtendedUfsFeaturesSupport)};

  if (!kExtendedUfsFeaturesSupport.writebooster_support()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  properties_.is_write_booster_enabled =
      wb_node.CreateBool("is_write_booster_enabled", is_write_booster_enabled_);
  properties_.writebooster_buffer_flush_during_hibernate =
      wb_node.CreateBool("writebooster_buffer_flush_during_hibernate", false);
  properties_.writebooster_buffer_flush_enabled =
      wb_node.CreateBool("writebooster_buffer_flush_enabled", is_write_booster_flush_enabled_);

  if (zx::result<> result = EnableWriteBooster(); result.is_error()) {
    fdf::error("Failed to enable WriteBooster: {}", result);
    return result.take_error();
  }

  auto disable_write_booster = fit::defer([&] {
    if (is_write_booster_enabled_) {
      if (zx::result<> result = DisableWriteBooster(); result.is_error()) {
        fdf::error("Failed to disable WriteBooster: {}", result);
      } else {
        fdf::warn("WriteBooster is disabled");
      }
    }
  });

  // Get WriteBooster buffer parameters.
  write_booster_buffer_type_ =
      static_cast<WriteBoosterBufferType>(device_descriptor_.bWriteBoosterBufferType);
  properties_.write_booster_buffer_type = wb_node.CreateUint(
      "write_booster_buffer_type", static_cast<uint8_t>(write_booster_buffer_type_));

  user_space_configuration_option_ = static_cast<UserSpaceConfigurationOption>(
      device_descriptor_.bWriteBoosterBufferPreserveUserSpaceEn);
  properties_.user_space_configuration_option = wb_node.CreateUint(
      "user_space_configuration_option", static_cast<uint8_t>(user_space_configuration_option_));

  // Find the size of the write buffer.
  uint32_t alloc_units = 0;
  if (write_booster_buffer_type_ == WriteBoosterBufferType::kSharedBuffer) {
    alloc_units = betoh32(device_descriptor_.dNumSharedWriteBoosterBufferAllocUnits);
  } else if (write_booster_buffer_type_ == WriteBoosterBufferType::kLuDedicatedBuffer) {
    for (uint8_t lun = 0; lun < max_lun_count_; ++lun) {
      auto unit_descriptor = ReadDescriptor<UnitDescriptor>(DescriptorType::kUnit, lun);
      if (unit_descriptor.is_error()) {
        continue;
      }
      alloc_units = betoh32(unit_descriptor->dLUNumWriteBoosterBufferAllocUnits);
      if (alloc_units > 0) {
        // Found a dedicated buffer from LU.
        write_booster_dedicated_lu_ = lun;
        properties_.write_booster_dedicated_lu =
            wb_node.CreateUint("write_booster_dedicated_lu", write_booster_dedicated_lu_);
        break;
      }
    }
  } else {
    fdf::warn("Not supported WriteBooster buffer type: 0x{:x}",
              static_cast<uint8_t>(write_booster_buffer_type_));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  if (alloc_units == 0) {
    // Unable to enable WriteBooster due to lack of resources.
    fdf::warn("The WriteBooster buffer size is zero.");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t write_booster_buffer_size_in_bytes =
      alloc_units * static_cast<uint32_t>(geometry_descriptor_.bAllocationUnitSize) *
      (betoh32(geometry_descriptor_.dSegmentSize)) * kSectorSize;
  properties_.write_booster_buffer_size_in_bytes =
      wb_node.CreateUint("write_booster_buffer_size_in_bytes", write_booster_buffer_size_in_bytes);

  zx::result<bool> result = IsWriteBoosterBufferLifeTimeLeft();
  if (result.is_error()) {
    fdf::error("Failed to IsWriteBoosterBufferLifeTimeLeft(): {}", result);
    return result.take_error();
  }
  if (!result.value()) {
    // Unable to enable WriteBooster due to lack of resources.
    fdf::warn("Exceeded its maximum estimated WriteBooster Buffer life time");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  // TODO(https://fxbug.dev/42075643): Need to handle WRITEBOOSTER_FLUSH_NEEDED exception case.

  disable_write_booster.cancel();
  fdf::info("WriteBooster is enabled");
  return zx::ok();
}

zx::result<bool> DeviceManager::IsWriteBoosterBufferLifeTimeLeft() {
  uint8_t buffer_lun = write_booster_buffer_type_ == WriteBoosterBufferType::kLuDedicatedBuffer
                           ? write_booster_dedicated_lu_
                           : 0;
  zx::result<uint32_t> life_time = ReadAttribute(Attributes::bWBBufferLifeTimeEst, buffer_lun);
  if (life_time.is_error()) {
    return life_time.take_error();
  }
  if (life_time.value()) {
    if (life_time == kExceededWriteBoosterBufferLifeTime) {
      return zx::ok(false);
    }
  }

  return zx::ok(true);
}

zx::result<> DeviceManager::EnableWriteBooster() {
  // Enable WriteBooster.
  if (zx::result<> result = SetFlag(Flags::fWriteBoosterEn); result.is_error()) {
    return result.take_error();
  }
  is_write_booster_enabled_ = true;
  properties_.is_write_booster_enabled.Set(is_write_booster_enabled_);

  // Enable WriteBooster buffer flush during hibernate.
  if (zx::result<> result = SetFlag(Flags::fWBBufferFlushDuringHibernate); result.is_error()) {
    return result.take_error();
  }
  properties_.writebooster_buffer_flush_during_hibernate.Set(true);

  // Enable WriteBooster buffer flush.
  // TODO(https://fxbug.dev/42075643): For Samsung Exynos, ignore this flush behaviour due to the
  // quirk of not supporting manual flush.
  if (zx::result<> result = SetFlag(Flags::fWBBufferFlushEn); result.is_error()) {
    return result.take_error();
  }
  is_write_booster_flush_enabled_ = true;
  properties_.writebooster_buffer_flush_enabled.Set(is_write_booster_flush_enabled_);

  return zx::ok();
}

zx::result<> DeviceManager::DisableWriteBooster() {
  if (is_write_booster_flush_enabled_) {
    // Disable WriteBooster buffer flush.
    if (zx::result<> result = ClearFlag(Flags::fWBBufferFlushEn); result.is_error()) {
      return result.take_error();
    }
    is_write_booster_flush_enabled_ = false;
    properties_.writebooster_buffer_flush_enabled.Set(is_write_booster_flush_enabled_);
  }

  // Disable WriteBooster buffer flush during hibernate.
  if (zx::result<> result = ClearFlag(Flags::fWBBufferFlushDuringHibernate); result.is_error()) {
    return result.take_error();
  }
  properties_.writebooster_buffer_flush_during_hibernate.Set(false);

  // Disable WriteBooster.
  if (zx::result<> result = ClearFlag(Flags::fWriteBoosterEn); result.is_error()) {
    return result.take_error();
  }
  is_write_booster_enabled_ = false;
  properties_.is_write_booster_enabled.Set(is_write_booster_enabled_);

  return zx::ok();
}

zx::result<> DeviceManager::InitReferenceClock(inspect::Node &controller_node) {
  properties_.reference_clock = controller_node.CreateString("reference_clock", "");
  return ConfigureReferenceClock();
}

zx::result<> DeviceManager::ConfigureReferenceClock() {
  // Intel UFSHCI reference clock = 19.2MHz
  const AttributeReferenceClock reference_clock = controller_.intel_quirk()
                                                      ? AttributeReferenceClock::k19_2MHz
                                                      : AttributeReferenceClock::k38_4MHz;

  // Per JEDEC JESD220, bRefClkFreq is a persistent attribute retained across power cycles, and
  // redundant writes can incur flash/NVRAM write latency and wear. Only write it when the device
  // reports a different frequency. If it cannot be read, write it anyway unless the read timed out:
  // the write would then most likely time out as well.
  zx::result<uint32_t> current_reference_clock = ReadAttribute(Attributes::bRefClkFreq);
  if (current_reference_clock.is_error()) {
    if (current_reference_clock.status_value() == ZX_ERR_TIMED_OUT) {
      fdf::error("Timed out reading bRefClkFreq");
      return current_reference_clock.take_error();
    }
    fdf::warn("Failed to read bRefClkFreq, writing it unconditionally: {}",
              current_reference_clock);
  }
  if (current_reference_clock.is_error() ||
      current_reference_clock.value() != static_cast<uint32_t>(reference_clock)) {
    if (auto result = WriteAttribute(Attributes::bRefClkFreq, reference_clock); result.is_error()) {
      return result.take_error();
    }
  }

  std::string reference_clock_string;
  switch (reference_clock) {
    case AttributeReferenceClock::k19_2MHz:
      reference_clock_string = "19.2 MHz";
      break;
    case AttributeReferenceClock::k26MHz:
      reference_clock_string = "26 MHz";
      break;
    case AttributeReferenceClock::k38_4MHz:
      reference_clock_string = "38.4 MHz";
      break;
    case AttributeReferenceClock::kObsolete:
      reference_clock_string = "52 MHz (Obsolete))";
      break;
    default:
      return zx::error(ZX_ERR_INVALID_ARGS);
  }
  properties_.reference_clock.Set(reference_clock_string);

  return zx::ok();
}

zx::result<> DeviceManager::InitUniproAttributes(inspect::Node &unipro_node) {
  properties_.remote_version = unipro_node.CreateUint("remote_version", 0);
  properties_.local_version = unipro_node.CreateUint("local_version", 0);
  properties_.host_t_activate = unipro_node.CreateUint("host_t_activate", 0);
  properties_.device_t_activate = unipro_node.CreateUint("device_t_activate", 0);
  properties_.host_granularity = unipro_node.CreateUint("host_granularity", 0);
  properties_.device_granularity = unipro_node.CreateUint("device_granularity", 0);
  return ConfigureUniproAttributes();
}

zx::result<> DeviceManager::ConfigureUniproAttributes() {
  // UniPro Version
  // 7~15 = Above 2.0, 6 = 2.0, 5 = 1.8, 4 = 1.61, 3 = 1.6, 2 = 1.41, 1 = 1.40, 0 = Reserved
  zx::result<uint32_t> remote_version = DmeGet(PA_RemoteVerInfo);
  if (remote_version.is_error()) {
    return remote_version.take_error();
  }
  zx::result<uint32_t> local_version = DmeGet(PA_LocalVerInfo);
  if (local_version.is_error()) {
    return local_version.take_error();
  }

  // UniPro automatically sets timing information such as PA_TActivate through the
  // PACP_CAP_EXT1_ind command during Link Startup operation.
  zx::result<uint32_t> host_t_activate = DmeGet(PA_TActivate);
  if (host_t_activate.is_error()) {
    return host_t_activate.take_error();
  }

  if (controller_.intel_quirk()) {
    // Intel Lake-field UFSHCI has a quirk. We need to add 200us to the PEER's PA_TActivate.
    DmePeerSetUicCommand dme_peer_set_t_activate(
        controller_, PA_TActivate, 0, AttrSetType::kVolatile, host_t_activate.value() + 2);
    if (auto result = dme_peer_set_t_activate.SendCommand(); result.is_error()) {
      return result.take_error();
    }
  }

  zx::result<uint32_t> device_t_activate = DmePeerGet(PA_TActivate);
  if (device_t_activate.is_error()) {
    return device_t_activate.take_error();
  }
  // PA_Granularity = 100us (1=1us, 2=4us, 3=8us, 4=16us, 5=32us, 6=100us)
  zx::result<uint32_t> host_granularity = DmeGet(PA_Granularity);
  if (host_granularity.is_error()) {
    return host_granularity.take_error();
  }
  zx::result<uint32_t> device_granularity = DmePeerGet(PA_Granularity);
  if (device_granularity.is_error()) {
    return device_granularity.take_error();
  }

  properties_.remote_version.Set(remote_version.value());
  properties_.local_version.Set(local_version.value());
  properties_.host_t_activate.Set(host_t_activate.value());
  properties_.device_t_activate.Set(device_t_activate.value());
  properties_.host_granularity.Set(host_granularity.value());
  properties_.device_granularity.Set(device_granularity.value());

  return zx::ok();
}

zx::result<> DeviceManager::InitUicPowerMode(inspect::Node &unipro_node) {
  properties_.pa_active_tx_data_lanes = unipro_node.CreateUint("PA_ActiveTxDataLanes", 0);
  properties_.pa_active_rx_data_lanes = unipro_node.CreateUint("PA_ActiveRxDataLanes", 0);
  properties_.pa_max_rx_hs_gear = unipro_node.CreateUint("PA_MaxRxHSGear", 0);
  properties_.pa_tx_gear = unipro_node.CreateUint("PA_TxGear", 0);
  properties_.pa_rx_gear = unipro_node.CreateUint("PA_RxGear", 0);
  properties_.tx_termination = unipro_node.CreateBool("tx_termination", false);
  properties_.rx_termination = unipro_node.CreateBool("rx_termination", false);
  properties_.pa_hs_series = unipro_node.CreateUint("PA_HSSeries", 0);
  properties_.power_mode = unipro_node.CreateUint("power_mode", 0);
  return ConfigureUicPowerMode();
}

zx::result<> DeviceManager::ConfigureUicPowerMode() {
  if (zx::result<> result = controller_.Notify(NotifyEvent::kPrePowerModeChange, 0);
      result.is_error()) {
    return result.take_error();
  }

  // Query available TX/RX lanes and max HS-GEAR once at initial boot and reuse the cached
  // capability values across PowerDown resumes, as these are static link capabilities determined
  // during UniPro link startup.
  uint32_t tx_lanes = 0;
  uint32_t rx_lanes = 0;
  uint32_t max_rx_hs_gear = 0;
  if (cached_avail_tx_lanes_.has_value() && cached_avail_rx_lanes_.has_value() &&
      cached_max_rx_hs_gear_.has_value()) {
    tx_lanes = *cached_avail_tx_lanes_;
    rx_lanes = *cached_avail_rx_lanes_;
    max_rx_hs_gear = *cached_max_rx_hs_gear_;
  } else {
    zx::result<uint32_t> tx_lanes_res = DmeGet(PA_AvailTxDataLanes);
    if (tx_lanes_res.is_error()) {
      return tx_lanes_res.take_error();
    }
    zx::result<uint32_t> rx_lanes_res = DmeGet(PA_AvailRxDataLanes);
    if (rx_lanes_res.is_error()) {
      return rx_lanes_res.take_error();
    }
    zx::result<uint32_t> max_rx_hs_gear_res = DmeGet(PA_MaxRxHSGear);
    if (max_rx_hs_gear_res.is_error()) {
      return max_rx_hs_gear_res.take_error();
    }
    tx_lanes = tx_lanes_res.value();
    rx_lanes = rx_lanes_res.value();
    max_rx_hs_gear = max_rx_hs_gear_res.value();
    cached_avail_tx_lanes_ = tx_lanes;
    cached_avail_rx_lanes_ = rx_lanes;
    cached_max_rx_hs_gear_ = max_rx_hs_gear;
  }

  // Set data lanes.
  if (zx::result<> result = DmeSet(PA_ActiveTxDataLanes, tx_lanes); result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_ActiveRxDataLanes, rx_lanes); result.is_error()) {
    return result.take_error();
  }

  // Set HS-GEAR to max gear.
  if (zx::result<> result = DmeSet(PA_TxGear, max_rx_hs_gear); result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_RxGear, max_rx_hs_gear); result.is_error()) {
    return result.take_error();
  }

  // Set termination.
  // HS-MODE = ON / LS-MODE = OFF
  if (zx::result<> result = DmeSet(PA_TxTermination, true); result.is_error()) {
    return result.take_error();
  }

  // HS-MODE = ON / LS-MODE = OFF
  if (zx::result<> result = DmeSet(PA_RxTermination, true); result.is_error()) {
    return result.take_error();
  }

  // Set HSSeries (A = 1, B = 2)
  constexpr uint32_t kHsSeries = 2;
  if (zx::result<> result = DmeSet(PA_HSSeries, kHsSeries); result.is_error()) {
    return result.take_error();
  }

  // Set TX HS Adapt Type (INITIAL = 1 for HS-G4+, NO_ADAPT = 3 for HS-G1..G3), matching the gear
  // rule in ufshcd_dme_configure_adapt(). PA_TxHsAdaptType only exists from UniPro 1.8 (HS-G4);
  // for lower gears NO_ADAPT is the hardware default, so (unlike Linux) a rejected DME_SET is
  // tolerated there.
  constexpr uint32_t kPaInitialAdapt = 1;
  constexpr uint32_t kPaNoAdapt = 3;
  constexpr uint32_t kMinAdaptGear = 4;
  const bool needs_adapt = max_rx_hs_gear >= kMinAdaptGear;
  const uint32_t adapt_type = needs_adapt ? kPaInitialAdapt : kPaNoAdapt;
  if (zx::result<> result = DmeSet(PA_TxHsAdaptType, adapt_type); result.is_error()) {
    if (needs_adapt) {
      return result.take_error();
    }
    fdf::warn("Failed to set PA_TxHsAdaptType = NO_ADAPT for HS-G{}; continuing: {}",
              max_rx_hs_gear, result);
  }

  // Set Timeout values.
  if (zx::result<> result = DmeSet(PA_PWRModeUserData0, DL_FC0ProtectionTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_PWRModeUserData1, DL_TC0ReplayTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_PWRModeUserData2, DL_AFC0ReqTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_PWRModeUserData3, DL_FC0ProtectionTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_PWRModeUserData4, DL_TC0ReplayTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(PA_PWRModeUserData5, DL_AFC0ReqTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }

  if (zx::result<> result =
          DmeSet(DME_LocalFC0ProtectionTimeOutVal, DL_FC0ProtectionTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(DME_LocalTC0ReplayTimeOutVal, DL_TC0ReplayTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = DmeSet(DME_LocalAFC0ReqTimeOutVal, DL_AFC0ReqTimeOutVal_Default);
      result.is_error()) {
    return result.take_error();
  }

  // Set TX/RX PWRMode.
  // TX[3:0], RX[7:4]
  // Fast_Mode=1, Slow_Mode=2, FastAuto_Mode=4, SlowAuto_Mode=5
  constexpr uint32_t kFastMode = 1;
  constexpr uint32_t kRxBitShift = 4;
  constexpr uint32_t kPwrMode = kFastMode << kRxBitShift | kFastMode;
  if (zx::result<> result = DmeSet(PA_PWRMode, kPwrMode); result.is_error()) {
    return result.take_error();
  }

  // Wait for power mode changed.
  auto wait_for_completion = [&]() -> bool {
    return InterruptStatusReg::Get().ReadFrom(&controller_.GetMmio()).uic_power_mode_status();
  };
  fbl::String timeout_message = "Timeout waiting for Power Mode Change";
  if (zx_status_t status = controller_.WaitWithTimeout(
          wait_for_completion, zx::usec(kDeviceInitTimeoutUs), timeout_message);
      status != ZX_OK) {
    return zx::error(status);
  }
  // Clear 'Power Mode completion status'
  InterruptStatusReg::Get().FromValue(0).set_uic_power_mode_status(true).WriteTo(
      &controller_.GetMmio());

  HostControllerStatusReg::PowerModeStatus power_mode_status =
      HostControllerStatusReg::Get()
          .ReadFrom(&controller_.GetMmio())
          .uic_power_mode_change_request_status();
  if (power_mode_status != HostControllerStatusReg::PowerModeStatus::kPowerLocal) {
    fdf::error("Failed to change power mode: power_mode_status = 0x{:x}",
               static_cast<uint32_t>(power_mode_status));
    return zx::error(ZX_ERR_BAD_STATE);
  }

  if (zx::result<> result = controller_.Notify(NotifyEvent::kPostPowerModeChange, 0);
      result.is_error()) {
    return result.take_error();
  }

  if (controller_.intel_quirk()) {
    //  Intel Lake-field UFSHCI has a quirk. We need to wait 1250us and clear dme error.
    usleep(1250);
    // Test with dme_peer_get to make sure there are no errors.
    zx::result<uint32_t> device_granularity = DmePeerGet(PA_Granularity);
    if (device_granularity.is_error()) {
      return device_granularity.take_error();
    }
  }

  properties_.pa_active_tx_data_lanes.Set(tx_lanes);
  properties_.pa_active_rx_data_lanes.Set(rx_lanes);
  properties_.pa_max_rx_hs_gear.Set(max_rx_hs_gear);
  properties_.pa_tx_gear.Set(max_rx_hs_gear);
  properties_.pa_rx_gear.Set(max_rx_hs_gear);
  properties_.tx_termination.Set(true);
  properties_.rx_termination.Set(true);
  properties_.pa_hs_series.Set(kHsSeries);
  properties_.power_mode.Set(kPwrMode);

  return zx::ok();
}

zx::result<> DeviceManager::RestoreDeviceAndLinkConfig() {
  // exception_event_control_ caches the device's wExceptionEventControl attribute (which exception
  // events, e.g. urgent background operations, the device flags in response UPIUs) so that
  // SetExceptionEventControl() can skip writes that would not change it. The power cycle reset the
  // attribute to its default (0), so drop the cached value; otherwise a later
  // SetExceptionEventControl() that computes the same bits we last wrote would skip the write and
  // leave the device with those events disabled.
  exception_event_control_ = ExceptionEventControl{0};

  if (zx::result<> result = ConfigureReferenceClock(); result.is_error()) {
    return result.take_error();
  }

  if (!controller_.qemu_quirk()) {
    // The UniPro attributes that InitUniproAttributes() reads (UniPro versions, PA_TActivate and
    // PA_Granularity) are either fixed or re-established by the link startup that preceded this,
    // and are only reported through inspect, so they are not re-read here. The exception is the
    // Intel quirk, whose volatile DME_PEER_SET of PA_TActivate was undone by the power cycle.
    if (controller_.intel_quirk()) {
      if (zx::result<> result = ConfigureUniproAttributes(); result.is_error()) {
        return result.take_error();
      }
    }
    if (zx::result<> result = ConfigureUicPowerMode(); result.is_error()) {
      return result.take_error();
    }
    if (zx::result<> result = WriteAttribute(Attributes::bActiveIccLevel, kHighestActiveIcclevel);
        result.is_error()) {
      return result.take_error();
    }
  }

  if (is_write_booster_enabled_) {
    if (zx::result<> result = EnableWriteBooster(); result.is_error()) {
      fdf::warn("Failed to re-enable WriteBooster on resume: {}", result);
    }
  }

  // A PowerDown suspend removes VCC/VCCQ, which resets fPowerOnWPEn (JESD220). SuspendPower() does
  // not enter PowerDown while a power-on write-protected LU is present; only re-read the flag if
  // power-on write protect was previously enabled so the cached state and inspect stay consistent.
  if (is_power_on_write_protect_enabled_) {
    if (zx::result<uint8_t> flag = ReadFlag(Flags::fPowerOnWPEn); flag.is_ok()) {
      const bool enabled = flag.value() != 0;
      if (enabled != is_power_on_write_protect_enabled_) {
        fdf::warn("fPowerOnWPEn changed across PowerDown suspend: {} -> {}",
                  is_power_on_write_protect_enabled_, enabled);
        is_power_on_write_protect_enabled_ = enabled;
        properties_.is_power_on_write_protect_enabled.Set(enabled);
      }
    } else {
      fdf::warn("Failed to re-read fPowerOnWPEn on resume: {}", flag);
    }
  }

  return zx::ok();
}

zx::result<> DeviceManager::SetPowerCondition(scsi::PowerCondition target_power_condition) {
  if (current_power_condition_ == target_power_condition) {
    return zx::ok();
  }

  auto scsi_lun = Ufs::TranslateUfsLunToScsiLun(static_cast<uint8_t>(WellKnownLuns::kUfsDevice));
  if (scsi_lun.is_error()) {
    return scsi_lun.take_error();
  }

  // Send START STOP UNIT to change power mode. Like ufshcd_execute_start_stop(), retry when the
  // device answers with UNIT ATTENTION / NOT READY (ZX_ERR_UNAVAILABLE), e.g. the POWER ON unit
  // attention that follows a PowerDown suspend.
  zx_status_t status = ZX_ERR_INTERNAL;
  for (int retry = 0; retry < kMaxRetries; ++retry) {
    status = controller_.StartStopUnit(kPlaceholderTarget, scsi_lun.value(),
                                       /*immed*/ false, target_power_condition);
    if (status != ZX_ERR_UNAVAILABLE) {
      break;
    }
    // A UNIT ATTENTION is cleared by the report itself, so retry immediately once; only back off
    // if the device is still not ready ("becoming ready") after that.
    if (retry > 0) {
      zx::nanosleep(zx::deadline_after(kUnavailableRetryDelay));
    }
  }
  if (status != ZX_OK) {
    fdf::error("Failed to send START STOP UNIT SCSI command: {}", zx_status_get_string(status));
    return zx::error(status);
  }

  current_power_condition_ = target_power_condition;
  return zx::ok();
}

zx::result<> DeviceManager::SuspendPower() {
  // PowerDown removes VCC/VCCQ from the device, which clears fPowerOnWPEn (JESD220) and would
  // silently turn a power-on write-protected LU writable. Linux keeps VCC when
  // dev_info.is_lu_power_on_wp is set; we achieve the same by staying in Sleep (device Idle, link
  // Hibern8) instead, which only drops the link.
  const bool supports_power_down = controller_.SupportsPowerDownSuspend();
  bool power_down = supports_power_down && !IsLogicalLunPowerOnWriteProtect();
  if (supports_power_down && !power_down && !power_down_blocked_) {
    fdf::info("A power-on write-protected LU is present; using Sleep instead of PowerDown suspend");
    power_down_blocked_ = true;
  }
  // TODO(https://fxbug.dev/568465123): PowerDown collapses the host controller power domain, which
  // discards programmed inline encryption keys. Until key restore is wired through the inline
  // encryption provider on resume, stay in Sleep (link hibernate, controller powered) whenever keys
  // are live so that encrypted I/O continues to work after resume.
  if (power_down && controller_.HasProgrammedCryptoKeys()) {
    fdf::info("Inline encryption keys are programmed; using Sleep instead of PowerDown suspend");
    power_down = false;
  }
  const UfsPowerMode target_power_mode =
      power_down ? UfsPowerMode::kPowerDown : UfsPowerMode::kSleep;
  const scsi::PowerCondition target_power_condition =
      GetPowerModeInfo(target_power_mode).power_condition;
  const LinkState target_link_state = GetPowerModeInfo(target_power_mode).link_state;

  {
    std::lock_guard<std::mutex> lock(power_lock_);

    if (current_power_mode_ == target_power_mode &&
        current_power_condition_ == target_power_condition &&
        current_link_state_ == target_link_state) {
      return zx::ok();
    }

    if (current_power_mode_ != UfsPowerMode::kActive ||
        current_power_condition_ != scsi::PowerCondition::kActive ||
        current_link_state_ != LinkState::kActive) {
      return zx::error(ZX_ERR_BAD_STATE);
    }

    // From here on IsResumed() reports false, which keeps the I/O worker and the exception-event
    // handler quiet while the device is being reconfigured below.
    suspend_in_progress_ = true;
  }

  // Stop new I/O from reaching the controller and wait for the in-flight I/O to complete before
  // issuing START STOP UNIT / stopping the host controller (ufshcd blocks SCSI requests and waits
  // for outstanding ones for the same reason). The gate and the drain share the request
  // processor's slot lock, so once this returns nothing but admin UPIUs can be submitted until
  // ResumePower() re-opens it. This must happen outside power_lock_ because the I/O worker, which
  // completes the in-flight commands, may be blocked in IsResumed().
  zx::result<> result = req_processor_.BlockSlotAllocation(kSuspendIoDrainTimeout);

  std::lock_guard<std::mutex> lock(power_lock_);
  SuspendProgress progress;
  if (result.is_error()) {
    fdf::error("Refusing to suspend with in-flight I/O: {}", result);
  } else {
    result = SuspendPowerLocked(power_down, target_power_mode, progress);
  }

  // Finish the attempt in the same lock scope. If it failed before the new power state was recorded
  // the device is still Active, so undo the device-side preparation (background ops /
  // auto-hibernate off; ResumePower() would otherwise return early without re-enabling them) and
  // re-open the I/O path. Doing all of this before suspend_in_progress_ is cleared means nobody
  // observes IsResumed() == true until the device is back in its pre-suspend configuration. On
  // success the slot gate stays closed until ResumePower().
  if (!progress.suspended) {
    if (progress.auto_hibernate_disabled) {
      if (zx::result<> ah8 = controller_.ConfigureAutoHibernate(); ah8.is_error()) {
        fdf::warn("Failed to re-enable auto-hibernate after failed suspend: {}", ah8);
      }
    }
    if (progress.background_op_disabled) {
      if (zx::result<> bkops = EnableBackgroundOp(); bkops.is_error()) {
        fdf::warn("Failed to re-enable background operations after failed suspend: {}", bkops);
      }
    }
  }
  suspend_in_progress_ = false;
  if (!progress.suspended) {
    req_processor_.UnblockSlotAllocation();
    // Only posts to the I/O worker; it re-checks IsResumed() once this lock is released.
    controller_.TriggerIoWork();
  }
  return result;
}

// Device/link side of the suspend, run with power_lock_ held once the I/O path is closed. Reports
// through |progress| what has been done so that SuspendPower() can undo it if this fails before
// the new power state is recorded.
zx::result<> DeviceManager::SuspendPowerLocked(bool power_down, UfsPowerMode target_power_mode,
                                               SuspendProgress &progress) {
  const scsi::PowerCondition target_power_condition =
      GetPowerModeInfo(target_power_mode).power_condition;
  const LinkState target_link_state = GetPowerModeInfo(target_power_mode).link_state;

  // Disable background operations before the device leaves Active, as Linux does for both runtime
  // and system suspend (__ufshcd_wl_suspend() -> ufshcd_disable_auto_bkops(); the link may not be
  // turned off while auto-BKOPS is enabled since that would power the device off mid-operation).
  // Linux's only exception - keeping the device Active with VCC on so urgent BKOPS or a
  // WriteBooster flush can finish (UFSHCD_CAP_AUTO_BKOPS_SUSPEND / b_rpm_dev_flush_capable) -
  // applies to runtime suspend into Hibern8 only, which this driver does not do. For the same
  // reason a pending WriteBooster flush does not hold off a system suspend: the WriteBooster
  // buffer is non-volatile, so nothing is lost by powering the device down, and the flush simply
  // continues once the device is Active again.
  // TODO(b/42075643): On resume Linux only re-enables BKOPS when bBackgroundOpStatus is at least
  // "performance impact" (ufshcd_bkops_ctrl()) and otherwise relies on the URGENT_BKOPS exception
  // event; ResumePower() re-enables unconditionally, matching our Active-state policy in
  // ConfigureBackgroundOp().
  if (zx::result<> result = DisableBackgroundOp(); result.is_error()) {
    return result.take_error();
  }
  progress.background_op_disabled = true;

  if (zx::result<> result = controller_.Notify(NotifyEvent::kPreSuspend, 0); result.is_error()) {
    return result.take_error();
  }

  if (zx::result<> result = controller_.DisableAutoHibernate(); result.is_error()) {
    return result.take_error();
  }
  progress.auto_hibernate_disabled = true;

  if (!power_down) {
    if (zx::result<> result = controller_.Notify(NotifyEvent::kPrePowerModeChange, 0);
        result.is_error()) {
      return result.take_error();
    }
  }

  if (zx::result<> result = SetPowerCondition(target_power_condition); result.is_error()) {
    return result.take_error();
  }

  DmeHibernateEnterCommand dme_hibernate_enter_command(controller_);
  if (auto result = dme_hibernate_enter_command.SendCommand(); result.is_error()) {
    // TODO(b/42075643): Link has a problem and needs to perform error recovery.
    // The device already left Active; bring it back (ufshcd's set_dev_active) so the state we
    // leave behind matches what the I/O path assumes when it is re-opened. If that fails too, the
    // recorded state (Active mode/link, non-Active condition) is repaired by the next
    // ResumePower(), which re-issues START STOP UNIT without touching the link.
    if (zx::result<> active = SetPowerCondition(scsi::PowerCondition::kActive); active.is_error()) {
      fdf::error("Failed to restore Active power condition after hibernate-enter failure: {}",
                 active);
    }
    return result.take_error();
  }

  zx_status_t power_down_status = ZX_OK;
  if (!power_down) {
    if (zx::result<> result = controller_.Notify(NotifyEvent::kPostPowerModeChange, 0);
        result.is_error()) {
      return result.take_error();
    }
  } else {
    // Block new admin UPIUs (RPMB, fuchsia.hardware.ufs, exception-event worker) for the rest of
    // the sequence; TransferRequestProcessor::SendRequestUpiu() checks IsHardwarePoweredOff()
    // under this lock, so once it is set below nothing can reach the registers.
    std::lock_guard<std::mutex> admin_lock(
        controller_.GetTransferRequestProcessor().admin_slot_lock());

    // Stop the host controller and mark the register space as powered off (under uic_lock(), so
    // that any UIC command / key-slot write already in progress completes first).
    power_down_status = controller_.StopHostControllerForPowerDown();
    if (zx::result<> result = controller_.Notify(NotifyEvent::kPostSuspend, 0); result.is_error()) {
      fdf::error("PHY power-down failed after stopping the host controller: {}", result);
      if (power_down_status == ZX_OK) {
        power_down_status = result.status_value();
      }
    }
  }

  // Once the host controller has been stopped there is no way back short of a full
  // re-initialization, so a PowerDown failure is still recorded as PowerDown (and reported to Power
  // Broker); the next "on" then takes the ReinitAfterPowerDown() path.
  current_link_state_ = target_link_state;
  current_power_mode_ = target_power_mode;
  current_power_condition_ = target_power_condition;
  properties_.power_suspended.Set(true);
  progress.suspended = true;

  if (power_down_status != ZX_OK) {
    return zx::error(power_down_status);
  }
  fdf::info("Power suspended.");
  return zx::ok();
}

zx::result<> DeviceManager::ResumePower() {
  const UfsPowerMode target_power_mode = UfsPowerMode::kActive;
  const scsi::PowerCondition target_power_condition =
      GetPowerModeInfo(target_power_mode).power_condition;
  const LinkState target_link_state = GetPowerModeInfo(target_power_mode).link_state;

  bool from_power_down = false;
  bool device_only = false;
  {
    std::lock_guard<std::mutex> lock(power_lock_);

    if (current_power_mode_ == target_power_mode &&
        current_power_condition_ == target_power_condition &&
        current_link_state_ == target_link_state) {
      return zx::ok();
    }

    from_power_down =
        (current_power_mode_ == UfsPowerMode::kPowerDown && current_link_state_ == LinkState::kOff);
    const bool from_sleep = (current_power_mode_ == UfsPowerMode::kSleep &&
                             current_power_condition_ == scsi::PowerCondition::kIdle &&
                             current_link_state_ == LinkState::kHibernate);
    // Left behind by SuspendPower() when hibernate-enter failed and the device could not be
    // brought back to Active either: the link is still up, only START STOP UNIT needs re-issuing.
    device_only = (current_power_mode_ == UfsPowerMode::kActive &&
                   current_link_state_ == LinkState::kActive &&
                   current_power_condition_ != target_power_condition);
    if (!from_power_down && !from_sleep && !device_only) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
  }

  if (from_power_down) {
    // Mirror ufshcd_reset_and_restore(): a failed link startup / device init after PowerDown is
    // retried with a full PHY + device power cycle in between, rather than leaving storage
    // unusable until reboot.
    zx::result<> reinit_result = zx::ok();
    for (int attempt = 1; attempt <= kMaxPowerDownResumeAttempts; ++attempt) {
      if (zx::result<> result = controller_.Notify(NotifyEvent::kPreResume, 0); result.is_error()) {
        fdf::error("PHY resume failed (attempt {}/{}): {}", attempt, kMaxPowerDownResumeAttempts,
                   result);
        reinit_result = result;
      } else {
        // The UFSHCI power domains/clocks are back; registers may be accessed again.
        controller_.SetHardwarePoweredOff(false);
        // Execute ReinitAfterPowerDown() outside power_lock_ because SendLinkStartUp() acquires
        // power_lock_ and admin UPIUs trigger ProcessIo() on the IO worker thread.
        // current_power_mode_ remains kPowerDown until reinit completes, so IsResumed() stays
        // false.
        reinit_result = controller_.ReinitAfterPowerDown();
        if (reinit_result.is_ok()) {
          // The device comes out of a power cycle in the Active power condition, but issue START
          // STOP UNIT explicitly (as ufshcd does) so that the device state is known rather than
          // assumed.
          std::lock_guard<std::mutex> lock(power_lock_);
          reinit_result = SetPowerCondition(target_power_condition);
        }
        if (reinit_result.is_ok()) {
          break;
        }
        fdf::error("Re-initialization after PowerDown failed (attempt {}/{}): {}", attempt,
                   kMaxPowerDownResumeAttempts, reinit_result);
        // Stop the host controller (as SuspendPower() does, under admin_slot_lock() so that no
        // admin UPIU slips in between the IsHardwarePoweredOff() check and the doorbell write) and
        // make sure nothing touches it while the PHY is powered down again.
        std::lock_guard<std::mutex> admin_lock(
            controller_.GetTransferRequestProcessor().admin_slot_lock());
        controller_.StopHostControllerForPowerDown();
      }
      // Power the PHY and device back down so that their state matches the PowerDown state we
      // keep recording (whether we retry or give up). Errors are logged by the PHY driver.
      if (zx::result<> result = controller_.Notify(NotifyEvent::kPostSuspend, 0);
          result.is_error()) {
        fdf::warn("PHY power-down after failed resume attempt failed: {}", result);
      }
      // SendLinkStartUp() records LinkState::kActive as soon as DME_LINKSTARTUP succeeds. The link
      // is down again now that the PHY is off, so restore the PowerDown state; otherwise the next
      // ResumePower() would not recognise the PowerDown state and fail with ZX_ERR_BAD_STATE.
      std::lock_guard<std::mutex> lock(power_lock_);
      current_link_state_ = LinkState::kOff;
      current_power_mode_ = UfsPowerMode::kPowerDown;
      current_power_condition_ = GetPowerModeInfo(UfsPowerMode::kPowerDown).power_condition;
    }
    if (reinit_result.is_error()) {
      // current_* describe PowerDown again; the next "on" retries from kPreResume.
      return reinit_result.take_error();
    }
  }

  std::lock_guard<std::mutex> lock(power_lock_);
  const bool link_in_hibernate = !from_power_down && !device_only;
  if (link_in_hibernate) {
    if (zx::result<> result = controller_.Notify(NotifyEvent::kPrePowerModeChange, 0);
        result.is_error()) {
      return result.take_error();
    }

    DmeHibernateExitCommand dme_hibernate_exit_command(controller_);
    if (auto result = dme_hibernate_exit_command.SendCommand(); result.is_error()) {
      // TODO(https://fxbug.dev/42075643): Link has a problem and needs to perform error recovery.
      return result.take_error();
    }
  }

  // (The PowerDown path already issued START STOP UNIT inside its retry loop.)
  if (!from_power_down) {
    if (zx::result<> result = SetPowerCondition(target_power_condition); result.is_error()) {
      return result.take_error();
    }
  }

  current_link_state_ = target_link_state;
  current_power_mode_ = target_power_mode;
  current_power_condition_ = target_power_condition;
  properties_.power_suspended.Set(false);

  if (link_in_hibernate) {
    if (zx::result<> result = controller_.Notify(NotifyEvent::kPostPowerModeChange, 0);
        result.is_error()) {
      return result.take_error();
    }
  }

  // From here on the device is Active and usable for I/O, so re-open the slot gate closed by
  // SuspendPower() (the caller posts TriggerIoWork() once we return); the remaining steps are
  // optimisations (platform hook, background ops, auto-hibernate) whose failure should not be
  // reported as a failed resume.
  req_processor_.UnblockSlotAllocation();

  if (zx::result<> result = controller_.Notify(NotifyEvent::kPostResume, 0); result.is_error()) {
    fdf::warn("PostResume callback failed: {}", result);
  }

  // TODO(b/42075643): We should only perform background operations in the power suspended state.
  if (zx::result<> result = EnableBackgroundOp(); result.is_error()) {
    fdf::warn("Failed to re-enable background operations after resume: {}", result);
  }

  if (zx::result<> result = controller_.ConfigureAutoHibernate(); result.is_error()) {
    fdf::warn("Failed to re-enable auto-hibernate after resume: {}", result);
  }

  fdf::info("Power resumed.");
  return zx::ok();
}

zx::result<> DeviceManager::InitUfsPowerMode(inspect::Node &controller_node,
                                             inspect::Node &attributes_node) {
  // NOTE: ReadAttribute and WriteAttribute issue synchronous query UPIU commands across the bus.
  // We MUST execute bus I/O outside power_lock_ to avoid deadlocking with concurrent IO/dispatcher
  // threads (e.g. ProcessIo) that acquire power_lock_ while checking power state.
  // Thread safety and serialization for bus I/O are guaranteed by admin_slot_lock_ in
  // TransferRequestProcessor, so executing attribute queries outside power_lock_ carries no risk
  // of conflicting I/O.
  zx::result<uint32_t> power_mode = ReadAttribute(Attributes::bCurrentPowerMode);
  if (power_mode.is_error()) {
    return power_mode.take_error();
  }
  UfsPowerMode target_power_mode = static_cast<UfsPowerMode>(power_mode.value());
  if (target_power_mode != UfsPowerMode::kActive) {
    fdf::error("Initial power mode is not active: 0x{:x}", static_cast<uint8_t>(target_power_mode));
    return zx::error(ZX_ERR_BAD_STATE);
  }
  fdf::debug("bCurrentPowerMode 0x{:x}", power_mode.value());

  // Write active ICC level outside power_lock_ for the same anti-deadlock reason.
  // TODO(https://fxbug.dev/42075643): Calculate and set the maximum ICC level. Currently, this
  // value is temporarily set to 0x0F, which is the highest active ICC level.
  if (auto result = WriteAttribute(Attributes::bActiveIccLevel, kHighestActiveIcclevel);
      result.is_error()) {
    return result.take_error();
  }

  // Acquire power_lock_ only for the immediate, non-blocking in-memory state update.
  std::lock_guard<std::mutex> lock(power_lock_);
  current_power_mode_ = target_power_mode;
  current_power_condition_ = GetPowerModeInfo(current_power_mode_).power_condition;
  current_link_state_ = GetPowerModeInfo(current_power_mode_).link_state;

  // TODO(https://fxbug.dev/42075643): Enable auto hibernate

  properties_.b_current_power_mode =
      attributes_node.CreateUint("bCurrentPowerMode", static_cast<uint8_t>(current_power_mode_));
  properties_.b_active_icc_level =
      attributes_node.CreateUint("bActiveICCLevel", kHighestActiveIcclevel);
  properties_.power_condition =
      controller_node.CreateUint("PowerCondition", static_cast<uint8_t>(current_power_condition_));
  properties_.link_state =
      controller_node.CreateUint("LinkState", static_cast<uint8_t>(current_link_state_));

  return zx::ok();
}

}  // namespace ufs
