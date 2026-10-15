// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/boot-shim/reboot-reason.h"

#include <array>

namespace boot_shim {
namespace {

// See https://source.android.com/docs/core/architecture/bootloader/boot-reason
constexpr std::string_view kBootArgKey = "androidboot.bootreason";

// The maximum property value size enforced by the bootloader.
constexpr size_t kMaxRebootReasonSize = 512;

// Maps `androidboot.bootreason` values that start with `prefix` to a ZBI reboot reason.
//
// A boot reason is a comma-separated list of items (e.g. "reboot,ocp,pmic"), and `prefix` can match
// it in one of two ways:
//
//  * As an item prefix (the default), `prefix` must end on an item boundary: the value either
//    equals `prefix` or continues with ',' and more items. "reboot,longkey" matches
//    "reboot,longkey" and "reboot,longkey,s2", but not "reboot,longkeys".
//  * As a partial prefix, the last item of `prefix` may also be the start of a longer item.
//    "reboot,ocp" matches "reboot,ocp,pmic" and also "reboot,ocp2,pmic".
struct RebootReasonMap {
  std::string_view prefix;
  zbi_hw_reboot_reason_t value;
  // If true, `prefix` is a partial prefix; otherwise it is an item prefix.
  bool is_partial = false;
};

constexpr auto kRebootReasons = std::to_array<RebootReasonMap>({

    // Generally indicates the hardware has its state reset and ramoops/crashlog should retain
    // persistent
    // content.
    {.prefix = "warm", .value = ZBI_HW_REBOOT_REASON_WARM},
    {.prefix = "reboot,warm", .value = ZBI_HW_REBOOT_REASON_WARM},

    // Generally indicates the memory and the devices retain some state, and the ramoops/crashlog
    // backing
    // store contains persistent content.
    {.prefix = "hard", .value = ZBI_HW_REBOOT_REASON_WARM},

    // Generally indicates a full reset of all devices, including memory.
    {.prefix = "cold", .value = ZBI_HW_REBOOT_REASON_COLD},
    {.prefix = "reboot,cold", .value = ZBI_HW_REBOOT_REASON_COLD},

    {.prefix = "watchdog", .value = ZBI_HW_REBOOT_REASON_WATCHDOG},
    {.prefix = "reboot,uvlo", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    {.prefix = "reboot,ocp", .value = ZBI_HW_REBOOT_REASON_BROWNOUT, .is_partial = true},
    {.prefix = "reboot,sys_ldo_ok,pmic", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    {.prefix = "reboot,smpl_timeout,pmic", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    {.prefix = "reboot,master_dc,reset", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    // The PMIC cut power and entered ship mode because the battery was depleted.
    {.prefix = "reboot,auto_fship,pmic", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    {.prefix = "reboot,longkey", .value = ZBI_HW_REBOOT_REASON_USER_HARD_RESET},
});

}  // namespace

void RebootReasonItem::Init(const BootProperties& properties, const char* shim_name, FILE* log) {
  // Bootloaders may write the boot reason into bootconfig unquoted, which bootconfig syntax parses
  // as an array, so join the elements to recover the full boot reason.
  std::array<char, kMaxRebootReasonSize> buffer;
  auto prop = properties.JoinProperty(kBootArgKey, buffer);

  // No reboot reason.
  if (prop.is_error()) {
    fprintf(log, "%s: ERROR %.*s was missing, no reboot reason.\n", shim_name,
            static_cast<int>(kBootArgKey.size()), kBootArgKey.data());
    return;
  }

  std::string_view reboot_reason = prop.value();
  if (reboot_reason.empty()) {
    fprintf(log, "%s: ERROR %.*s was empty, no reboot reason.\n", shim_name,
            static_cast<int>(kBootArgKey.size()), kBootArgKey.data());
    return;
  }

  for (const auto& [prefix, value, is_partial] : kRebootReasons) {
    // Values may carry sub-reasons after a known reason (e.g. "reboot,uvlo,pmic,sub" or
    // "watchdog,apc"), so an item prefix also matches when followed by a ','.
    if (is_partial ? reboot_reason.starts_with(prefix)
                   : reboot_reason == prefix || (reboot_reason.starts_with(prefix) &&
                                                 reboot_reason[prefix.size()] == ',')) {
      fprintf(log, "%s: INFO %.*s was <%.*s>.\n", shim_name, static_cast<int>(kBootArgKey.size()),
              kBootArgKey.data(), static_cast<int>(reboot_reason.size()), reboot_reason.data());
      set_payload(value);
      return;
    }
  }

  fprintf(log, "%s: ERROR %.*s was <%.*s>, no known reboot reason.\n", shim_name,
          static_cast<int>(kBootArgKey.size()), kBootArgKey.data(),
          static_cast<int>(reboot_reason.size()), reboot_reason.data());
}

}  // namespace boot_shim
