// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.gpu.magma/cpp/wire.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <lib/fidl/cpp/wire/arena.h>
#include <lib/fit/thread_safety.h>
#include <lib/magma/platform/platform_handle.h>
#include <lib/magma/platform/platform_logger.h>
#include <lib/magma/platform/zircon/zircon_platform_status.h>
#include <lib/magma/util/short_macros.h>
#include <lib/magma_service/sys_driver/dfv1/magma_device_impl.h>
#include <lib/magma_service/sys_driver/magma_system_device.h>
#include <zircon/process.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <memory>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <ddktl/protocol/empty-protocol.h>

#include "lib/ddk/binding_driver.h"

#if MAGMA_TEST_DRIVER
zx_status_t magma_indriver_test(zx_device_t* device);
#endif

namespace {

class GpuDevice;

using DdkDeviceType =
    ddk::Device<GpuDevice, msd::MagmaDeviceImpl, ddk::Unbindable, ddk::Initializable>;

msd::DeviceHandle* ZxDeviceToDeviceHandle(zx_device_t* device) {
  return reinterpret_cast<msd::DeviceHandle*>(device);
}

class GpuDevice : public DdkDeviceType, public ddk::EmptyProtocol<ZX_PROTOCOL_GPU> {
 public:
  explicit GpuDevice(zx_device_t* parent_device) : DdkDeviceType(parent_device) {}

  void DdkInit(ddk::InitTxn txn);
  void DdkUnbind(ddk::UnbindTxn txn);
  void DdkRelease();

  zx_status_t Init();

 private:
  zx_status_t MagmaStart() FIT_REQUIRES(magma_mutex());
};

zx_status_t GpuDevice::MagmaStart() {
  set_magma_system_device(msd::MagmaSystemDevice::Create(
      magma_driver(), magma_driver()->CreateDevice(ZxDeviceToDeviceHandle(parent()))));
  if (!magma_system_device())
    return DRET_MSG(ZX_ERR_NO_RESOURCES, "Failed to create device");
  InitSystemDevice();
  return ZX_OK;
}

void GpuDevice::DdkInit(ddk::InitTxn txn) {
  set_zx_device(zxdev());
  txn.Reply(InitChildDevices());
}

void GpuDevice::DdkUnbind(ddk::UnbindTxn txn) {
  // This will tear down client connections and cause them to return errors.
  MagmaStop();
  txn.Reply();
}

void GpuDevice::DdkRelease() {
  MAGMA_LOG(INFO, "Starting device_release");

  delete this;
  MAGMA_LOG(INFO, "Finished device_release");
}

zx_status_t GpuDevice::Init() {
  std::lock_guard<std::mutex> lock(magma_mutex());
  set_magma_driver(msd::Driver::Create());
#if MAGMA_TEST_DRIVER
  DLOG("running magma indriver test");
  set_unit_test_status(magma_indriver_test(parent()));
#endif

  zx_status_t status = MagmaStart();
  if (status != ZX_OK)
    return status;

  status = DdkAdd(ddk::DeviceAddArgs("magma_gpu").set_flags(DEVICE_ADD_NON_BINDABLE));
  if (status != ZX_OK)
    return DRET_MSG(status, "device_add failed");
  return ZX_OK;
}

static zx_status_t driver_bind(void* context, zx_device_t* parent) {
  MAGMA_LOG(INFO, "driver_bind: binding\n");
  auto gpu = std::make_unique<GpuDevice>(parent);
  if (!gpu)
    return ZX_ERR_NO_MEMORY;

  zx_status_t status = gpu->Init();
  if (status != ZX_OK) {
    return status;
  }
  // DdkAdd in Init took ownership of device.
  [[maybe_unused]] GpuDevice* ptr = gpu.release();
  return ZX_OK;
}

}  // namespace

zx_driver_ops_t msd_driver_ops = []() constexpr {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = driver_bind;
  return ops;
}();

ZIRCON_DRIVER(magma_pdev_gpu, msd_driver_ops, "zircon", "0.1");
