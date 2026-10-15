// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/boot-shim/reboot-reason.h"

#include <lib/boot-shim/boot-properties.h>
#include <lib/boot-shim/boot-shim.h>
#include <lib/fit/defer.h>
#include <lib/linux-boot-config/linux-boot-config.h>
#include <lib/zbitl/image.h>

#include <array>
#include <string>
#include <string_view>

#include <zxtest/zxtest.h>

namespace {

template <typename Zbi, typename Pred>
bool HasZbiItem(Zbi&& zbi, Pred&& pred) {
  auto cleanup = fit::defer([&zbi]() { zbi.ignore_error(); });
  return std::any_of(zbi.begin(), zbi.end(),
                     [pred](auto item) -> bool { return pred(*item.header, item.payload); });
}

TEST(RebootReasonItemTest, NoRebootReason) {
  constexpr std::string_view kCmdline = "   foo-bar=not-reboot-reason   ";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

TEST(RebootReasonItemTest, EmptyRebootReason) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

TEST(RebootReasonItemTest, UnknownReason) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=foo-bar";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

TEST(RebootReasonItemTest, Warm) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=warm";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WARM;
  }));
}

TEST(RebootReasonItemTest, RebootWarm) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=reboot,warm";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WARM;
  }));
}

TEST(RebootReasonItemTest, Hard) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=hard";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WARM;
  }));
}

TEST(RebootReasonItemTest, Cold) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=cold";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

TEST(RebootReasonItemTest, RebootCold) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=reboot,cold";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

TEST(RebootReasonItemTest, Watchdog) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=watchdog";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WATCHDOG;
  }));
}

TEST(RebootReasonItemTest, Brownout) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=reboot,uvlo";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_BROWNOUT;
  }));
}

TEST(RebootReasonItemTest, UserHardReset) {
  constexpr std::string_view kCmdline = "androidboot.bootreason=reboot,longkey,s2";

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(kCmdline),
                                               shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_USER_HARD_RESET;
  }));
}

TEST(RebootReasonItemTest, ParsingAtMultiplePositions) {
  constexpr std::array kCmdlines = {
      "androidboot.bootreason=watchdog",
      "androidboot.bootreason=watchdog ",
      " androidboot.bootreason=watchdog",
  };

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  for (auto cmdline : kCmdlines) {
    ASSERT_TRUE(image.clear().is_ok());

    boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
    shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(cmdline),
                                                 shim.shim_name());

    ASSERT_TRUE(shim.AppendItems(image).is_ok());

    ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
      EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
      auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
      return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
             (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
             *reason == ZBI_HW_REBOOT_REASON_WATCHDOG;
    }));
  }
}

TEST(RebootReasonItemTest, BootconfigReason) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = cold\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

TEST(RebootReasonItemTest, BootconfigPrecedenceOverCmdline) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = watchdog\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("androidboot.bootreason=warm", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WATCHDOG;
  }));
}

TEST(RebootReasonItemTest, BootconfigRebootCold) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = \"reboot,cold\"\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

TEST(RebootReasonItemTest, BootconfigArrayRebootMain) {
  constexpr std::string_view kBootconfigData =
      "androidboot.bootreason = \"reboot,cold\", \"main\"\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

TEST(RebootReasonItemTest, BootconfigArrayWatchdogApc) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = \"watchdog\", \"apc\"\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WATCHDOG;
  }));
}

TEST(RebootReasonItemTest, BootconfigEmptyReason) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = \"\"\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

TEST(RebootReasonItemTest, BootconfigUnknownReason) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason = \"foo-bar\"\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

TEST(RebootReasonItemTest, PmicBrownoutReasons) {
  constexpr std::array<std::string_view, 10> kBrownoutCmdlines = {
      "androidboot.bootreason=reboot,uvlo,pmic,main",
      "androidboot.bootreason=reboot,uvlo,pmic,sub",
      "androidboot.bootreason=reboot,uvlo,pmic,if",
      "androidboot.bootreason=reboot,ocp,pmic,main",
      "androidboot.bootreason=reboot,ocp,pmic,if",
      "androidboot.bootreason=reboot,ocp2,pmic,sub",
      "androidboot.bootreason=reboot,ocp3,pmic,if,usb",
      "androidboot.bootreason=reboot,sys_ldo_ok,pmic,main",
      "androidboot.bootreason=reboot,master_dc,reset",
      "androidboot.bootreason=reboot,auto_fship,pmic,if",
  };

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  for (auto cmdline : kBrownoutCmdlines) {
    ASSERT_TRUE(image.clear().is_ok());

    boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
    shim.Get<boot_shim::RebootReasonItem>().Init(boot_shim::BootProperties(cmdline),
                                                 shim.shim_name());

    ASSERT_TRUE(shim.AppendItems(image).is_ok());

    ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
      EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
      auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
      return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
             (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
             *reason == ZBI_HW_REBOOT_REASON_BROWNOUT;
    }));
  }
}

// The bootloader may write values into bootconfig unquoted, which bootconfig syntax parses as
// arrays. The full value must be reconstructed.
TEST(RebootReasonItemTest, BootconfigUnquotedRebootCold) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason:=reboot,cold\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

// Sub-reasons appended to a known reason refine, but do not change, it.
TEST(RebootReasonItemTest, BootconfigUnquotedWatchdogApc) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason:=watchdog,apc\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_WATCHDOG;
  }));
}

// An item prefix only matches on a ',' boundary.
TEST(RebootReasonItemTest, ItemPrefixRequiresSeparator) {
  constexpr std::string_view kBootconfigData = "androidboot.bootreason:=reboot,coldx\n";

  linux_boot_config::LinuxBootConfig bootconfig(kBootconfigData);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_FALSE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    return header.type == ZBI_TYPE_HW_REBOOT_REASON;
  }));
}

// Values longer than the buffer still match on their prefix.
TEST(RebootReasonItemTest, BootconfigLongValue) {
  std::string bootconfig_data = "androidboot.bootreason:=reboot,cold";
  for (int i = 0; i < 200; ++i) {
    bootconfig_data += ",abcdefgh";
  }
  bootconfig_data += "\n";

  linux_boot_config::LinuxBootConfig bootconfig(bootconfig_data);
  boot_shim::BootProperties props("", bootconfig);

  std::array<std::byte, 512> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::BootShim<boot_shim::RebootReasonItem> shim("test-shim", stdout);
  shim.Get<boot_shim::RebootReasonItem>().Init(props, shim.shim_name());

  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  ASSERT_TRUE(HasZbiItem(image, [](const zbi_header_t& header, zbitl::ByteView payload) {
    EXPECT_GE(payload.size_bytes(), sizeof(zbi_hw_reboot_reason_t));
    auto* reason = reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
    return header.type == ZBI_TYPE_HW_REBOOT_REASON &&
           (payload.size_bytes() >= sizeof(zbi_hw_reboot_reason_t)) &&
           *reason == ZBI_HW_REBOOT_REASON_COLD;
  }));
}

}  // namespace
