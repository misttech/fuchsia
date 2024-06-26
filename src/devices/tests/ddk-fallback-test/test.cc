// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/platform-defs.h>
#include <lib/driver-integration-test/fixture.h>
#include <lib/fdio/directory.h>
#include <zircon/syscalls.h>

#include <zxtest/zxtest.h>

using driver_integration_test::IsolatedDevmgr;

class FallbackTest : public zxtest::Test {
 public:
  ~FallbackTest() override = default;
  // Set up and launch the devmgr.
  void LaunchDevmgr(IsolatedDevmgr::Args args) {
    board_test::DeviceEntry dev = {};
    strlcpy(dev.name, kPlatformDeviceName, sizeof(dev.name));
    dev.vid = PDEV_VID_TEST;
    dev.pid = PDEV_PID_FALLBACK_TEST;
    dev.did = 0;
    args.device_list.push_back(dev);

    zx_status_t status = IsolatedDevmgr::Create(&args, &devmgr_);
    ASSERT_OK(status);
  }

  // Check that the correct driver was bound. `fallback` indicates if we expect the fallback or
  // not-fallback driver to have bound.
  void CheckDriverBound(bool fallback) {
    fbl::String path = fbl::StringPrintf("sys/platform/%s/ddk-%s-test", kPlatformDeviceName,
                                         fallback ? "fallback" : "not-fallback");
    zx::result channel =
        device_watcher::RecursiveWaitForFile(devmgr_.devfs_root().get(), path.c_str());
    ASSERT_OK(channel.status_value());

    chan_ = std::move(channel.value());
    ASSERT_NE(chan_.get(), ZX_HANDLE_INVALID);
  }

 protected:
  zx::channel chan_;
  IsolatedDevmgr devmgr_;

 private:
  static constexpr char kPlatformDeviceName[board_test::kNameLengthMax] = "ddk-test";
};

TEST_F(FallbackTest, TestNotFallbackTakesPriority) {
  IsolatedDevmgr::Args args;
  ASSERT_NO_FATAL_FAILURE(LaunchDevmgr(std::move(args)));
  ASSERT_NO_FATAL_FAILURE(CheckDriverBound(false));
}

TEST_F(FallbackTest, TestFallbackBoundWhenAlone) {
  IsolatedDevmgr::Args args;
  args.driver_disable.push_back("fuchsia-boot:///dtr#meta/ddk-not-fallback-test.cm");
  ASSERT_NO_FATAL_FAILURE(LaunchDevmgr(std::move(args)));
  ASSERT_NO_FATAL_FAILURE(CheckDriverBound(true));
}
