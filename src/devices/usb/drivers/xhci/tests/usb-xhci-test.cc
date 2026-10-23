// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.hci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb/cpp/fidl.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/completion.h>
#include <lib/zx/bti.h>
#include <lib/zx/vmar.h>

#include <list>

#include <fake-dma-buffer/fake-dma-buffer.h>
#include <gtest/gtest.h>

#include "src/devices/usb/drivers/xhci/tests/test-env.h"
#include "src/lib/testing/predicates/status.h"

namespace usb_xhci {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

struct FakeTRB;
struct FakePhysAddr {
  uint64_t magic;
  FakeTRB* value;
};

struct FakeTRB : TRB {
  FakeTRB() {
    phys_addr_ = std::make_unique<FakePhysAddr>();
    phys_addr_->magic = kMagicValue;
    phys_addr_->value = this;
  }

  zx_paddr_t phys() { return reinterpret_cast<zx_paddr_t>(phys_addr_.get()); }

  static bool is_valid_paddr(zx_paddr_t phys) {
    return *reinterpret_cast<uint64_t*>(phys) == kMagicValue;
  }

  static FakeTRB* get(zx_paddr_t phys) {
    if (!is_valid_paddr(phys)) {
      return nullptr;
    }
    FakePhysAddr* addr = reinterpret_cast<FakePhysAddr*>(phys);
    return addr->value;
  }

  ~FakeTRB() {
    // Cast to volatile to prevent the compiler
    // from optimizing out this zero operation.
    *reinterpret_cast<volatile uint64_t*>(phys_addr_.get()) = 0;
  }

  static std::unique_ptr<FakeTRB> FromTRB(TRB* trb) {
    return std::unique_ptr<FakeTRB>(static_cast<FakeTRB*>(trb));
  }

  // Magic value to use for determining if a physical address is valid or not.
  // ASAN builds should also trigger an error if we try dereferencing something
  // that isn't valid. This value is a fallback for cases where ASAN isn't being used.
  static constexpr uint64_t kMagicValue = 0x12345678901ABCDEU;
  std::unique_ptr<FakePhysAddr> phys_addr_;
  std::vector<TRB> contig;
  zx_paddr_t next = 0;
  zx_paddr_t prev = 0;
};

class FakeDevice {
 public:
  FakeDevice() {
    constexpr auto kHcsParams2 = 2 * sizeof(uint32_t);
    constexpr auto kHccParams1 = 4 * sizeof(uint32_t);
    constexpr auto kXecp = 320 * sizeof(uint32_t);
    constexpr auto kOffset = 0 * sizeof(uint32_t);
    constexpr auto kHcsParams1 = 1 * sizeof(uint32_t);
    constexpr auto kOffset1 = 5 * sizeof(uint32_t);
    constexpr auto kOffset2 = 6 * sizeof(uint32_t);
    constexpr auto kUsbCmd = 7 * sizeof(uint32_t);
    constexpr auto kUsbSts = 8 * sizeof(uint32_t);
    constexpr auto kUsbPageSize = 9 * sizeof(uint32_t);
    constexpr auto kConfig = 14 * sizeof(uint32_t);
    constexpr auto kCrCr = 13 * sizeof(uint32_t);
    constexpr auto kDcbaa = 19 * sizeof(uint32_t);
    constexpr auto kDoorbellBase = 1024 * sizeof(uint32_t);
    constexpr auto kImodi = 457 * sizeof(uint32_t);

    constexpr uint8_t kCapLength = 0x1c;

    region_.emplace(sizeof(uint32_t), 2048);

    region()[kHcsParams2].SetReadCallback([]() {
      auto params = HCSPARAMS2::Get().FromValue(0);
      params.set_ERST_MAX(4);
      params.set_MAX_SCRATCHPAD_BUFFERS_LOW(1);
      return params.reg_value();
    });

    region()[kHccParams1].SetReadCallback([]() {
      auto params = HCCPARAMS1::Get().FromValue(0);
      params.set_AC64(true);
      params.set_CSZ(true);
      params.set_xECP(320);
      return params.reg_value();
    });

    region()[kXecp].SetReadCallback([=]() {
      auto xecp = (XECP::Get(HCCPARAMS1::Get().FromValue(
                                 static_cast<uint32_t>(region()[4 * sizeof(uint32_t)].Read())))
                       .FromValue(0));
      xecp.set_NEXT(0);
      xecp.set_ID(XECP::UsbLegacySupport);
      if (driver_owned_controller_) {
        xecp.set_reg_value(xecp.reg_value() | 1 << 24);
      } else {
        xecp.set_reg_value(xecp.reg_value() | 1 << 16);
      }
      return xecp.reg_value();
    });

    region()[kXecp].SetWriteCallback([=](uint64_t value) {
      if (value & (1 << 24)) {
        driver_owned_controller_ = true;
      }
    });
    region()[kOffset].SetReadCallback([=]() { return kCapLength; });

    region()[kHcsParams1].SetReadCallback([=]() {
      HCSPARAMS1 parms = HCSPARAMS1::Get().FromValue(0);
      parms.set_MaxIntrs(1);
      parms.set_MaxPorts(4);
      parms.set_MaxSlots(32);
      return parms.reg_value();
    });

    region()[kOffset1].SetReadCallback([=]() { return 0x1000; });

    region()[kOffset2].SetReadCallback([=]() { return 0x700; });

    region()[kUsbCmd].SetReadCallback([=]() {
      USBCMD cmd = USBCMD::Get(static_cast<uint8_t>(region()[0].Read())).FromValue(0);
      cmd.set_ENABLE(controller_enabled_);
      cmd.set_EWE(event_wrap_enable_);
      cmd.set_HSEE(host_system_error_enable_);
      cmd.set_INTE(irq_enable_);
      cmd.set_RESET(0);
      return cmd.reg_value();
    });

    region()[kUsbCmd].SetWriteCallback([=](uint64_t value) {
      USBCMD cmd = USBCMD::Get(static_cast<uint8_t>(region()[0].Read()))
                       .FromValue(static_cast<uint32_t>(value));
      if (cmd.RESET()) {
        controller_was_reset_ = true;
      }
      controller_enabled_ = cmd.ENABLE();
      event_wrap_enable_ = cmd.EWE();
      host_system_error_enable_ = cmd.HSEE();
      irq_enable_ = cmd.INTE();
    });

    region()[kUsbSts].SetReadCallback([=]() {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(!controller_enabled_);
      return sts.reg_value();
    });

    region()[kUsbPageSize].SetReadCallback([=]() {
      USB_PAGESIZE size = USB_PAGESIZE::Get(kCapLength).FromValue(0);
      size.set_PageSize(1);
      return size.reg_value();
    });

    region()[kConfig].SetReadCallback([=]() {
      CONFIG config = CONFIG::Get(kCapLength).FromValue(0);
      config.set_MaxSlotsEn(slots_enabled_);
      return config.reg_value();
    });

    region()[kConfig].SetWriteCallback([=](uint64_t value) {
      CONFIG config = CONFIG::Get(kCapLength).FromValue(static_cast<uint32_t>(value));
      slots_enabled_ = config.MaxSlotsEn();
    });

    region()[kCrCr].SetWriteCallback([=](uint64_t value) {
      CRCR cr = CRCR::Get(kCapLength).FromValue(value);
      crcr_ = reinterpret_cast<zx_paddr_t>(cr.PTR());
    });

    region()[kDcbaa].SetReadCallback([=]() { return dcbaa_; });
    region()[kDcbaa].SetWriteCallback([=](uint64_t value) {
      auto val = DCBAAP::Get(kCapLength).FromValue(value);
      dcbaa_ = val.PTR();
    });
    doorbell_callback_ = [](uint8_t doorbell, uint8_t target) {};
    for (size_t i = 0; i < 32; i++) {
      region()[kDoorbellBase + i * sizeof(uint32_t)].SetWriteCallback([=](uint64_t value) {
        auto buffer = mmio();
        auto bell = DOORBELL::Get(DoorbellOffset::Get().ReadFrom(&buffer), 0)
                        .FromValue(static_cast<uint32_t>(value));
        doorbell_callback_(static_cast<uint8_t>(i), static_cast<uint8_t>(bell.Target()));
      });
    }
    region()[kImodi].SetWriteCallback([=](uint64_t value) {
      auto buffer = mmio();
      auto imodi = IMODI::Get(RuntimeRegisterOffset::Get().ReadFrom(&buffer), 0)
                       .FromValue(static_cast<uint32_t>(value));
      imodi_ = static_cast<uint16_t>(imodi.MODI());
    });

    // Control register
  }

  fdf::MmioBuffer mmio() { return region_->GetMmioBuffer(); }
  fake_mmio::FakeMmioRegRegion& region() { return *region_; }

  void set_irq_signaller(zx::unowned_interrupt signaller) { irq_signaller_ = std::move(signaller); }

  void SetDoorbellCallback(fit::function<void(uint8_t, uint8_t)> callback) {
    doorbell_callback_ = std::move(callback);
  }

  FakeTRB* crcr() { return FakeTRB::get(crcr_); }

 private:
  std::optional<fake_mmio::FakeMmioRegRegion> region_;
  zx::unowned_interrupt irq_signaller_;
  bool driver_owned_controller_ = false;
  bool controller_enabled_ = false;
  bool controller_was_reset_ = false;
  bool event_wrap_enable_ = false;
  bool irq_enable_ = false;
  bool host_system_error_enable_ = false;
  uint32_t slots_enabled_ = 0;
  zx_paddr_t crcr_ = 0;
  zx_paddr_t dcbaa_ = 0;
  uint16_t imodi_;
  fit::function<void(uint8_t, uint8_t)> doorbell_callback_;
};

struct FakeUsbDevice {
  uint32_t device_id;
  uint32_t hub_id;
  usb_speed_t speed;
  bool fake_root_hub;
};

class FakeUsbBus : public ddk::UsbBusInterfaceProtocol<FakeUsbBus> {
 public:
  FakeUsbBus() = default;

  void reset() { sync_completion_reset(&completion_); }

  void wait() { sync_completion_wait(&completion_, ZX_TIME_INFINITE); }

  void reset_remove() { sync_completion_reset(&remove_completion_); }

  void wait_for_remove() { sync_completion_wait(&remove_completion_, ZX_TIME_INFINITE); }

  usb_bus_interface_protocol_t* proto() {
    proto_.ctx = this;
    proto_.ops = &usb_bus_interface_protocol_ops_;
    return &proto_;
  }

  zx_status_t UsbBusInterfaceAddDevice(uint32_t device_id, uint32_t hub_id, usb_speed_t speed) {
    FakeUsbDevice fake_device;
    fake_device.device_id = device_id;
    fake_device.hub_id = hub_id;
    fake_device.speed = speed;
    fake_device.fake_root_hub = device_id >= 32;
    devices_[device_id] = fake_device;
    sync_completion_signal(&completion_);
    return ZX_OK;
  }

  zx_status_t UsbBusInterfaceRemoveDevice(uint32_t device_id) {
    devices_.erase(device_id);
    sync_completion_signal(&remove_completion_);
    return ZX_OK;
  }

  zx_status_t UsbBusInterfaceResetPort(uint32_t hub_id, uint32_t port, bool enumerating) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbBusInterfaceReinitializeDevice(uint32_t device_id) { return ZX_ERR_NOT_SUPPORTED; }
  const std::map<uint32_t, FakeUsbDevice>& devices() { return devices_; }

 private:
  std::map<uint32_t, FakeUsbDevice> devices_;
  sync_completion_t completion_;
  sync_completion_t remove_completion_;
  usb_bus_interface_protocol_t proto_;
};

class XhciTestEnvironment : fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    device_server_.Initialize(component::kDefaultInstance);
    EXPECT_OK(
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs));

    fdf_fake::FakePDev::Config config;
    config.mmios[0] = fake_device_.mmio();
    config.irqs[0] = {};
    EXPECT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &config.irqs[0]));
    fake_device_.set_irq_signaller(config.irqs[0].borrow());
    config.use_fake_bti = true;
    pdev_server_.SetConfig(std::move(config));
    return to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_server_.GetInstanceHandler(fdf::Dispatcher::GetCurrent()->async_dispatcher()), "pdev");
  }

  FakeDevice& fake_device() { return fake_device_; }

 private:
  compat::DeviceServer device_server_;
  fdf_fake::FakePDev pdev_server_;
  FakeDevice fake_device_;
};

class XhciTestConfig final {
 public:
  using DriverType = usb_xhci::UsbXhci;
  using EnvironmentType = XhciTestEnvironment;
};

using TestRequest = usb::CallbackRequest<sizeof(max_align_t)>;
class XhciHarness : public ::testing::Test {
 public:
  FakeTRB* CreateTRB() {
    auto it = trbs_.insert(trbs_.end(), std::make_unique<FakeTRB>());
    (*it)->control = 0;
    (*it)->ptr = 0;
    (*it)->status = 0;
    return it->get();
  }

  FakeTRB* CreateTRBs(size_t count) {
    auto it = trbs_.insert(trbs_.end(), std::make_unique<FakeTRB>());
    (*it)->control = 0;
    (*it)->ptr = 0;
    (*it)->status = 0;
    (*it)->contig.resize(count);
    return it->get();
  }

  size_t GetMaxDeviceCount() { return driver_test().driver()->UsbHciGetMaxDeviceCount(); }

  void RequestQueue(TestRequest request) {
    static const usb_protocol_ops_t ops = {
        .request_queue =
            [](void* ctx, usb_request_t* usb_request,
               const usb_request_complete_callback_t* complete_cb) {
              auto* driver = static_cast<UsbXhci*>(ctx);
              driver->UsbHciRequestQueue(usb_request, complete_cb);
            },
    };
    usb_protocol_t proto = {
        .ops = &ops,
        .ctx = driver_test().driver(),
    };
    ddk::UsbProtocolClient client(&proto);
    TestRequest::Queue(std::move(request), client);
  }

  template <typename Callback>
  zx_status_t AllocateRequest(std::optional<TestRequest>* request, uint32_t device_id,
                              uint64_t data_size, uint8_t endpoint, Callback callback) {
    zx_status_t result =
        TestRequest::Alloc(request, data_size, endpoint,
                           driver_test().driver()->UsbHciGetRequestSize(), std::move(callback));
    if (result != ZX_OK) {
      return result;
    }
    void* virt;
    (*request)->Mmap(&virt);
    static_assert(sizeof(uint64_t) == sizeof(void*));
    size_t phys_count =
        fbl::round_up(data_size, zx_system_get_page_size()) / zx_system_get_page_size();
    (*request)->request()->phys_count = phys_count;
    // Need to use malloc for compatibility with the C ABI (which will eventually call free)
    (*request)->request()->phys_list =
        static_cast<zx_paddr_t*>(malloc(sizeof(zx_paddr_t) * phys_count));
    for (size_t i = 0; i < phys_count; i++) {
      auto trb = CreateTRB();
      trb->ptr = reinterpret_cast<uint64_t>(virt) + (zx_system_get_page_size() * i);
      (*request)->request()->phys_list[i] = trb->phys();
    }
    (*request)->request()->header.device_id = device_id;
    return ZX_OK;
  }

  uint8_t AllocateSlot() {
    if (slot_freelist_.empty()) {
      slot_id_++;
      return slot_id_;
    }
    uint8_t retval = slot_freelist_.back();
    slot_freelist_.pop_back();
    return retval;
  }

  FakeUsbDevice ConnectDevice(uint8_t port, usb_speed_t speed) {
    std::optional<HubInfo> hub;
    uint8_t slot = AllocateSlot();
    driver_test().driver()->GetPortState(port).is_connected = true;
    driver_test().driver()->GetPortState(port).link_active = true;
    driver_test().driver()->GetPortState(port).slot_id = slot;
    driver_test().driver()->SetDeviceInformation(slot, slot, hub);
    driver_test().driver()->AddressDeviceCommand(slot, port, hub, true);
    bus_.reset();
    driver_test().driver()->DeviceOnline(slot, port, speed);
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
    return bus_.devices().find(slot - 1)->second;
  }

  void EnableEndpoint(uint32_t device_id, uint8_t ep_num, bool is_in_endpoint) {
    usb_endpoint_descriptor_t ep_desc = {};
    ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
    ep_desc.b_endpoint_address = ep_num | (is_in_endpoint ? 0x80 : 0);
    driver_test().driver()->UsbHciEnableEndpoint(device_id, &ep_desc, nullptr);
  }

  zx_status_t CompleteCommand(TRB* trb, CommandCompletionEvent* event) {
    std::unique_ptr<TRBContext> context;
    zx_status_t status = driver_test().driver()->GetCommandRing()->CompleteTRB(trb, &context);
    if (status != ZX_OK) {
      return status;
    }
    context->completer->complete_ok(event);
    return ZX_OK;
  }

  void SetDoorbellListener(fit::function<void(uint8_t, uint8_t)> listener) {
    driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
      env.fake_device().SetDoorbellCallback(std::move(listener));
    });
  }

  FakeTRB* crcr_next() {
    return FakeTRB::get(driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
        [](XhciTestEnvironment& env) { return env.fake_device().crcr()->next; }));
  }

  ~XhciHarness() { trbs_.clear(); }

  fdf_testing::ForegroundDriverTest<XhciTestConfig>& driver_test() { return driver_test_; }

 protected:
  FakeUsbBus bus_;

 private:
  fdf_testing::ForegroundDriverTest<XhciTestConfig> driver_test_;
  std::vector<uint8_t> slot_freelist_;
  uint8_t slot_id_ = 0;
  std::list<std::unique_ptr<FakeTRB>> trbs_;
};

class XhciMmioHarness : public XhciHarness {
 public:
  void SetUp() override {
    ASSERT_TRUE(driver_test()
                    .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                      xhci_config::Config fake_config;
                      fake_config.enable_suspend() = false;
                      args.config(fake_config.ToVmo());
                    })
                    .is_ok());
    ASSERT_OK(driver_test().driver()->TestInit(this));
    driver_test().driver()->UsbHciSetBusInterface(bus_.proto());
  }

  void TearDown() override { ASSERT_TRUE(driver_test().StopDriver().is_ok()); }
};

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBs() {
  fbl::AutoLock al(&mutex_);
  return std::move(pending_trbs_);
}

zx::result<> UsbXhci::TestInit(void* test_harness) {
  test_harness_ = test_harness;
  return Init(ddk_fake::CreateBufferFactory());
}

void EventRing::ScheduleTask(fpromise::promise<void, zx_status_t> promise) {
  SchedulePromiseWithXhciExecutorPolicy(executor_, hci_, std::move(promise));
}

void EventRing::RunUntilIdle() { executor_.run_until_idle(); }

void EventRing::ClearPendingPortStatusChanges() { executor_.run_until_idle(); }

zx_status_t TransferRing::AllocateTRB(TRB** trb, State* state) {
  fbl::AutoLock _(&mutex_);
  if (state) {
    state->pcs = pcs_;
    state->trbs = trbs_;
  }
  auto new_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  new_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = new_trb->phys();
  trbs_ = new_trb;
  trbs_->ptr = 0;
  trbs_->status = pcs_;
  *trb = trbs_;
  return ZX_OK;
}

zx::result<ContiguousTRBInfo> TransferRing::AllocateContiguous(size_t count) {
  fbl::AutoLock _(&mutex_);
  auto new_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRBs(count);
  new_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = new_trb->phys();
  trbs_ = new_trb->contig.data();
  trbs_->ptr = 0;
  trbs_->status = pcs_;
  ContiguousTRBInfo info;
  info.trbs = cpp20::span(trbs_, count);
  return zx::ok(info);
}

constexpr auto kPeekPtr = 0x13823990000;

zx::result<CRCR> TransferRing::TransferRing::PeekCommandRingControlRegister(uint8_t cap_length) {
  fbl::AutoLock l(&mutex_);
  CRCR cr;
  cr.set_RCS(pcs_);
  cr.set_PTR(kPeekPtr);
  return zx::ok(cr);
}

zx_status_t TransferRing::CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
  fbl::AutoLock _(&mutex_);
  if (pending_trbs_.is_empty()) {
    return ZX_ERR_CANCELED;
  }
  dequeue_trb_ = trb;
  *context = pending_trbs_.pop_front();
  if (trb != (*context)->trb) {
    return ZX_ERR_IO;
  }
  return ZX_OK;
}

void TransferRing::CommitTransaction(const State& start) {}

zx_status_t TransferRing::AssignContext(TRB* trb, std::unique_ptr<TRBContext> context, TRB* first,
                                        TRB* setup) {
  fbl::AutoLock _(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  context->trb = trb;
  pending_trbs_.push_back(std::move(context));
  return ZX_OK;
}

zx_status_t TransferRing::Init(size_t page_size, const zx::bti& bti, EventRing* ring, bool is_32bit,
                               fdf::MmioBuffer* mmio, UsbXhci* hci) {
  fbl::AutoLock _(&mutex_);
  if (trbs_ != nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  page_size_ = page_size;
  bti_ = &bti;
  ring_ = ring;
  is_32_bit_ = is_32bit;
  mmio_ = mmio;
  isochronous_ = false;
  token_++;
  stalled_ = false;
  hci_ = hci;
  trbs_ = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  static_assert(sizeof(uint64_t) == sizeof(this));
  trbs_->ptr = reinterpret_cast<uint64_t>(this);
  trbs_->status = pcs_;
  trb_start_phys_ = static_cast<FakeTRB*>(trbs_)->phys();
  return ZX_OK;
}

CRCR TransferRing::TransferRing::phys(uint8_t cap_length) __TA_NO_THREAD_SAFETY_ANALYSIS {
  CRCR cr = CRCR::Get(cap_length).FromValue(trb_start_phys_);
  assert(trb_start_phys_);
  cr.set_RCS(pcs_);
  return cr;
}

TransferRing::State TransferRing::SaveState() {
  fbl::AutoLock _(&mutex_);
  State state;
  state.pcs = pcs_;
  state.trbs = trbs_;
  return state;
}

void TransferRing::Restore(const State& state) {
  fbl::AutoLock _(&mutex_);
  trbs_ = state.trbs;
  pcs_ = state.pcs;
}

zx_status_t TransferRing::AddTRB(const TRB& trb, std::unique_ptr<TRBContext> context) {
  fbl::AutoLock _(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  FakeTRB* alloc_trb;
  alloc_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  alloc_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = alloc_trb->phys();
  trbs_ = alloc_trb;
  alloc_trb->control = trb.control;
  alloc_trb->ptr = trb.ptr;
  alloc_trb->status = trb.status;
  context->token = token_;
  context->trb = alloc_trb;
  pending_trbs_.push_back(std::move(context));
  return ZX_OK;
}

zx_status_t TransferRing::Deinit() {
  fbl::AutoLock _(&mutex_);
  if (!trbs_) {
    return ZX_ERR_BAD_STATE;
  }
  trbs_ = nullptr;
  dequeue_trb_ = nullptr;
  pcs_ = true;
  return ZX_OK;
}

zx_status_t TransferRing::DeinitIfActive() __TA_NO_THREAD_SAFETY_ANALYSIS {
  if (trbs_) {
    return Deinit();
  }
  return ZX_OK;
}

zx_paddr_t TransferRing::VirtToPhys(TRB* trb) {
  auto phys = static_cast<FakeTRB*>(trb)->phys();
  ZX_ASSERT(FakeTRB::is_valid_paddr(phys));
  return phys;
}

zx_status_t EventRingSegmentTable::Init(size_t page_size, const zx::bti& bti, bool is_32bit,
                                        uint32_t erst_max, ERSTSZ erst_size,
                                        const dma_buffer::BufferFactory& factory,
                                        fdf::MmioBuffer* mmio) {
  erst_size_ = erst_size;
  bti_ = &bti;
  page_size_ = page_size;
  is_32bit_ = is_32bit;
  mmio_.emplace(mmio->View(0));
  zx_status_t status = factory.CreatePaged(bti, zx_system_get_page_size(),
                                           dma_buffer::CacheOptions::kDisabled, &erst_);
  if (status != ZX_OK) {
    return status;
  }

  count_ = page_size / sizeof(ERSTEntry);
  if (count_ > erst_max) {
    count_ = erst_max;
  }
  entries_ = static_cast<ERSTEntry*>(erst_->virt());
  return ZX_OK;
}

zx_status_t EventRing::Init(size_t page_size, const zx::bti& bti, fdf::MmioBuffer* buffer,
                            bool is_32bit, uint32_t erst_max, ERSTSZ erst_size, ERDP erdp_reg,
                            IMAN iman_reg, uint8_t cap_length, HCSPARAMS1 hcs_params_1,
                            CommandRing* command_ring, DoorbellOffset doorbell_offset, UsbXhci* hci,
                            HCCPARAMS1 hcc_params_1, uint64_t* dcbaa, uint16_t interrupter,
                            inspect::Node* interrupter_node) {
  fbl::AutoLock _(&segment_mutex_);
  erdp_reg_ = erdp_reg;
  hcs_params_1_ = hcs_params_1;
  mmio_ = buffer;
  bti_ = &bti;
  page_size_ = page_size;
  is_32bit_ = is_32bit;
  mmio_ = buffer;
  iman_reg_ = iman_reg;
  cap_length_ = cap_length;
  command_ring_ = command_ring;
  doorbell_offset_ = doorbell_offset;
  hci_ = hci;
  hcc_params_1_ = hcc_params_1;
  dcbaa_ = dcbaa;
  static_assert(sizeof(zx_paddr_t) == sizeof(this));
  erdp_phys_ = reinterpret_cast<zx_paddr_t>(this);
  interrupter_ = interrupter;
  return segments_.Init(page_size, bti, is_32bit, erst_max, erst_size, hci->buffer_factory(),
                        mmio_);
}

size_t EventRing::GetPressure() { return 0; }

zx_status_t Interrupter::Init(uint16_t interrupter, size_t page_size, fdf::MmioBuffer* buffer,
                              const RuntimeRegisterOffset& offset, uint32_t erst_max,
                              DoorbellOffset doorbell_offset, UsbXhci* hci, HCCPARAMS1 hcc_params_1,
                              uint64_t* dcbaa) {
  interrupter_ = interrupter;
  hci_ = hci;
  return ZX_OK;
}

zx_status_t Interrupter::Start(const RuntimeRegisterOffset& offset,
                               fdf::MmioView interrupter_regs) {
  return ZX_OK;
}

zx_status_t Interrupter::StartIrqThread() { return ZX_OK; }

fpromise::promise<void, zx_status_t> Interrupter::Timeout(zx::time deadline) {
  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

// Enumerates a device as specified in xHCI section 4.3 starting from step 4
// This method should be called once the physical port of a device has been
// initialized.
fpromise::promise<void, zx_status_t> EnumerateDevice(UsbXhci* hci, uint8_t port,
                                                     std::optional<HubInfo> hub_info) {
  fpromise::bridge<void, zx_status_t> bridge;
  return bridge.consumer.promise();
}

struct alignas(4096) FakeVMO {
  size_t size;
  uint32_t alignment_log2;
  bool enable_cache;
  zx::vmo backing_storage;
  void* virt;
};

TEST_F(XhciMmioHarness, QueueControlRequest) {
  ConnectDevice(1, USB_SPEED_HIGH);
  bool rang = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 1 && target == 1) {
      rang = true;
    }
  });

  std::optional<TestRequest> request;
  bool invoked = false;
  AllocateRequest(&request, 0, zx_system_get_page_size() * 2, 0, [&](TestRequest request) {
    invoked = true;
    void** parameters;
    request.Mmap(reinterpret_cast<void**>(&parameters));
    EXPECT_EQ(*parameters, parameters);
  });
  request->request()->setup.bm_request_type = kStandardDeviceIn;
  request->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  request->request()->setup.w_value = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice);
  ASSERT_LE(zx_system_get_page_size() * 2, static_cast<uint32_t>(UINT16_MAX));
  request->request()->setup.w_length = static_cast<uint16_t>(zx_system_get_page_size() * 2);
  RequestQueue(std::move(*request));
  ASSERT_TRUE(rang);
  // Find slot context pointer in address device command
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  ASSERT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  auto control = static_cast<unsigned char*>(reinterpret_cast<FakeVMO*>(cr->ptr)->virt);
  auto endpoint_context = reinterpret_cast<EndpointContext*>(control + (64 * 2));
  auto ring_phys =
      static_cast<zx_paddr_t>(static_cast<uint64_t>(endpoint_context->dequeue_pointer_a) |
                              (static_cast<uint64_t>(endpoint_context->dequeue_pointer_b) << 32)) &
      (~1);
  auto trb = FakeTRB::get(ring_phys);
  auto initial_trb = trb;
  // Setup
  trb = FakeTRB::get(trb->next);
  auto setup_trb = static_cast<struct Setup*>(static_cast<TRB*>(trb));
  ASSERT_EQ(setup_trb->length(), 8UL);
  ASSERT_EQ(setup_trb->IDT(), 1UL);
  ASSERT_EQ(setup_trb->TRT(), Setup::IN);
  // Data
  trb = FakeTRB::get(trb->next);
  auto data_trb = static_cast<ControlData*>(static_cast<TRB*>(trb));
  ASSERT_EQ(data_trb->DIRECTION(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 1UL);
  ASSERT_TRUE(data_trb->ISP());
  ASSERT_TRUE(data_trb->NO_SNOOP());
  void** virt = reinterpret_cast<void**>(FakeTRB::get(static_cast<zx_paddr_t>(data_trb->ptr))->ptr);
  *virt = virt;
  // Normal
  trb = FakeTRB::get(trb->next);
  auto normal_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(normal_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(normal_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(normal_trb->SIZE(), 0UL);
  ASSERT_TRUE(normal_trb->ISP());
  ASSERT_TRUE(normal_trb->NO_SNOOP());
  // Status
  trb = FakeTRB::get(trb->next);
  auto status_trb = static_cast<Status*>(static_cast<TRB*>(trb));
  ASSERT_EQ(status_trb->DIRECTION(), 0UL);
  ASSERT_EQ(status_trb->INTERRUPTER(), 0UL);
  ASSERT_TRUE(status_trb->IOC());
  // Interrupt on completion
  TransferRing* ring = reinterpret_cast<TransferRing*>(initial_trb->ptr);
  std::unique_ptr<TRBContext> context;
  ring->CompleteTRB(trb, &context);
  std::get<Request>(*context->request).Complete(ZX_OK, sizeof(void*));
  ASSERT_TRUE(invoked);
}

TEST_F(XhciMmioHarness, QueueNormalRequest) {
  ConnectDevice(1, USB_SPEED_FULL);
  EnableEndpoint(0, 1, true);
  bool rang = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 1 && target == 3) {
      rang = true;
    }
  });

  std::optional<TestRequest> request;
  bool invoked = false;
  AllocateRequest(&request, 0, zx_system_get_page_size() * 2, 1 | 0x80, [&](TestRequest request) {
    invoked = true;
    void** parameters;
    request.Mmap(reinterpret_cast<void**>(&parameters));
    EXPECT_EQ(*parameters, parameters);
  });

  RequestQueue(std::move(*request));
  ASSERT_TRUE(rang);
  // Find slot context pointer in address device command
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  ASSERT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  [[maybe_unused]] auto control =
      static_cast<unsigned char*>(reinterpret_cast<FakeVMO*>(cr->ptr)->virt);
  auto endpoint_context = reinterpret_cast<EndpointContext*>(control + (64 * 4));
  auto ring_phys =
      static_cast<zx_paddr_t>(static_cast<uint64_t>(endpoint_context->dequeue_pointer_a) |
                              (static_cast<uint64_t>(endpoint_context->dequeue_pointer_b) << 32)) &
      (~1);

  auto trb_start = FakeTRB::get(ring_phys);

  // Data (page 0)
  auto trb = FakeTRB::get(trb_start->next)->contig.data();
  auto data_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(Control::FromTRB(data_trb).Type(), Control::Normal);
  ASSERT_EQ(data_trb->IOC(), 0UL);
  ASSERT_EQ(data_trb->ISP(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 1UL);
  ASSERT_TRUE(data_trb->NO_SNOOP());
  void** virt = reinterpret_cast<void**>(FakeTRB::get(static_cast<zx_paddr_t>(data_trb->ptr))->ptr);
  *virt = virt;

  // Data (page 1, contiguous)
  trb++;
  data_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(data_trb->IOC(), 1UL);
  ASSERT_EQ(data_trb->ISP(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 0UL);
  ASSERT_TRUE(data_trb->NO_SNOOP());

  // Interrupt on completion
  TransferRing* ring = reinterpret_cast<TransferRing*>(trb_start->ptr);
  std::unique_ptr<TRBContext> context;
  ring->CompleteTRB(trb, &context);
  std::get<Request>(*context->request).Complete(ZX_OK, sizeof(void*));
  ASSERT_TRUE(invoked);
}

TEST_F(XhciMmioHarness, CancelAllOnDisabledEndpoint) {
  ConnectDevice(1, USB_SPEED_HIGH);
  zx_status_t cancel_status;
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));
  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::StopEndpointCommand: {
          auto cancel_command = reinterpret_cast<StopEndpoint*>(cr);
          EXPECT_EQ(cancel_command->ENDPOINT(), 2UL);
          EXPECT_EQ(cancel_command->SLOT(), 1UL);
          got_stop_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &event));
        } break;
      }
    }
  });
  cancel_status = driver_test().driver()->UsbHciCancelAll(0, 1);
  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_EQ(cancel_status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, ResetEndpointTestSuccessCase) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  uint64_t paddr;
  {
    auto& state = driver_test().driver()->GetDeviceState()[0];
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    auto& transfer_ring = state->GetEndpoint(0).transfer_ring();
    transfer_ring.set_stall(true);
    paddr = transfer_ring.PeekCommandRingControlRegister(0).value().reg_value();
  }

  zx_status_t reset_status;
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));
  cr = FakeTRB::get(cr->next);
  control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &event));

  bool got_reset_endpoint = false;
  bool got_set_tr_dequeue_ptr = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::ResetEndpointCommand: {
          auto reset_command = reinterpret_cast<ResetEndpoint*>(cr);
          EXPECT_EQ(reset_command->ENDPOINT(), 2UL);
          EXPECT_EQ(reset_command->SLOT(), 1UL);
          got_reset_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &event));
        } break;
        case Control::SetTrDequeuePointerCommand: {
          // ResetEndpoint should be sent prior to SetTrDequeuePointer
          EXPECT_TRUE(got_reset_endpoint);
          auto set_cmd = reinterpret_cast<SetTRDequeuePointer*>(cr);
          EXPECT_EQ(set_cmd->ENDPOINT(), 2UL);
          EXPECT_EQ(set_cmd->ptr, paddr);
          EXPECT_OK(CompleteCommand(cr, &event));
          got_set_tr_dequeue_ptr = true;
        } break;
      }
    }
  });

  reset_status = driver_test().driver()->UsbHciResetEndpoint(0, 1);

  EXPECT_TRUE(got_reset_endpoint);
  EXPECT_TRUE(got_set_tr_dequeue_ptr);
  EXPECT_OK(reset_status);
}

TEST_F(XhciMmioHarness, ResetEndpointFailsIfNotStalled) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  {
    auto& state = driver_test().driver()->GetDeviceState()[0];
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    state->GetEndpoint(0).transfer_ring().set_stall(false);
  }
  ASSERT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 1), ZX_ERR_INVALID_ARGS);
}

// ResetEndpoint propagates TR dequeue pointer failure to the caller (doorbell completions are
// synchronous in this harness).
TEST_F(XhciMmioHarness, ResetEndpointSetDequeuePointerFails) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  {
    auto& state = driver_test().driver()->GetDeviceState()[0];
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    state->GetEndpoint(0).transfer_ring().set_stall(true);
  }

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent fail_event;
  fail_event.set_CompletionCode(CommandCompletionEvent::ContextStateError);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  bool got_reset_endpoint = false;
  bool got_set_tr_dequeue_ptr = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell != 0) {
      return;
    }
    cr = FakeTRB::get(cr->next);
    switch (Control::FromTRB(cr).Type()) {
      case Control::ResetEndpointCommand:
        got_reset_endpoint = true;
        EXPECT_OK(CompleteCommand(cr, &success_event));
        break;
      case Control::SetTrDequeuePointerCommand:
        EXPECT_TRUE(got_reset_endpoint);
        got_set_tr_dequeue_ptr = true;
        EXPECT_OK(CompleteCommand(cr, &fail_event));
        break;
    }
  });

  zx_status_t reset_status = driver_test().driver()->UsbHciResetEndpoint(0, 1);

  EXPECT_TRUE(got_reset_endpoint);
  EXPECT_TRUE(got_set_tr_dequeue_ptr);
  EXPECT_EQ(reset_status, ZX_ERR_IO);
}

// CancelAll propagates StopEndpoint command failure to the caller.
TEST_F(XhciMmioHarness, CancelAllStopEndpointFails) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent fail_event;
  fail_event.set_CompletionCode(CommandCompletionEvent::ContextStateError);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell != 0) {
      return;
    }
    cr = FakeTRB::get(cr->next);
    if (Control::FromTRB(cr).Type() == Control::StopEndpointCommand) {
      got_stop_endpoint = true;
      EXPECT_OK(CompleteCommand(cr, &fail_event));
    }
  });

  zx_status_t cancel_status = driver_test().driver()->UsbHciCancelAll(0, 1);

  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_EQ(cancel_status, ZX_ERR_IO);
}

TEST_F(XhciMmioHarness, GetMaxDeviceCount) { ASSERT_EQ(GetMaxDeviceCount(), 34UL); }

namespace {

// NOLINTNEXTLINE(performance-unnecessary-value-param)
TEST_F(XhciMmioHarness, StopWithHCHaltedTimeout) {
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  auto poll_count = std::make_shared<std::atomic<size_t>>(0);
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([=] {
      poll_count->fetch_add(1, std::memory_order_relaxed);
      // Simulate MMIO unmapping or unhalted controller where HCHalted remains 0.
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(0);
      return sts.reg_value();
    });
  });

  libsync::Completion stop_done;
  driver_test().driver()->Stop(fdf::StopCompleter([&](zx::result<> result) {
    EXPECT_OK(result);
    stop_done.Signal();
  }));
  stop_done.Wait();
  EXPECT_GT(poll_count->load(std::memory_order_relaxed), 1u);
}

TEST_F(XhciMmioHarness, RunSynchronouslyDoesNotAbortPendingCommandsWhenRunning) {
  // Queue a command TRB on the command ring via ConnectDevice.
  ConnectDevice(1, USB_SPEED_HIGH);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);

  // Schedule a promise via RunSynchronously that takes 300ms to resolve while the controller is
  // healthy and running (!IsHaltedOrStopping()).
  fpromise::bridge<void, zx_status_t> slow_bridge;
  std::thread completer_thread([completer = std::move(slow_bridge.completer)] mutable {
    zx::nanosleep(zx::deadline_after(zx::msec(300)));
    completer.complete_ok();
  });

  zx_status_t sync_status =
      driver_test().driver()->RunSynchronously(0, slow_bridge.consumer.promise().box());
  completer_thread.join();

  // RunSynchronously must wait for the promise to complete with ZX_OK and must NOT abort pending
  // commands on the command ring after 250ms while the controller is still running.
  EXPECT_OK(sync_status);

  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));

  // Also verify that if the controller halts while RunSynchronously is waiting on a non-command
  // promise, early return does not cause stack-use-after-return when the promise later resolves.
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([=] {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(1);
      return sts.reg_value();
    });
  });

  fpromise::bridge<void, zx_status_t> halted_bridge;
  EXPECT_EQ(driver_test().driver()->RunSynchronously(0, halted_bridge.consumer.promise().box()),
            ZX_ERR_IO_NOT_PRESENT);
  // Resolving the promise after RunSynchronously has returned must safely update heap-shared state
  // without touching a popped stack frame.
  halted_bridge.completer.complete_ok();
  driver_test().driver()->RunUntilIdle(0);
}

TEST_F(XhciMmioHarness, DeviceOnlineUsesOneBasedPortIndex) {
  // GetPortState() uses 1-based port indexing matching the xHCI specification.
  std::optional<HubInfo> hub;
  driver_test().driver()->SetDeviceInformation(1, 1, hub);
  driver_test().driver()->GetPortState(1).is_USB3 = true;
  driver_test().driver()->GetPortState(2).is_USB3 = false;

  bus_.reset();
  EXPECT_OK(driver_test().driver()->DeviceOnline(1, 1, USB_SPEED_SUPER));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
  ASSERT_EQ(bus_.devices().count(0), 1u);
  EXPECT_EQ(bus_.devices().at(0).hub_id, GetMaxDeviceCount() - 1);

  // Also verify port 2 (USB 2.0) routes to the USB 2.0 root hub (GetMaxDeviceCount()).
  driver_test().driver()->SetDeviceInformation(2, 2, hub);
  bus_.reset();
  EXPECT_OK(driver_test().driver()->DeviceOnline(2, 2, USB_SPEED_HIGH));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
  ASSERT_EQ(bus_.devices().count(1), 1u);
  EXPECT_EQ(bus_.devices().at(1).hub_id, GetMaxDeviceCount() - 2);
}

TEST_F(XhciMmioHarness, GetPortStateUsesOneBasedPortIndex) {
  driver_test().driver()->GetPortState(1).slot_id = 42;
  driver_test().driver()->GetPortState(2).slot_id = 84;

  EXPECT_EQ(driver_test().driver()->GetPortState(1).slot_id, 42);
  EXPECT_EQ(driver_test().driver()->GetPortState(2).slot_id, 84);
}

TEST_F(XhciMmioHarness, DeviceOfflineWithBoundFidlEndpointAndPendingRequest) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, true);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  zx::result hci_client_end =
      driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
  ASSERT_OK(hci_client_end);
  auto [ep_client_end, ep_server_end] =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_client(std::move(*hci_client_end));
    auto connect_res = hci_client->ConnectToEndpoint(0, 1U | 0x80U, std::move(ep_server_end));
    ASSERT_TRUE(connect_res.ok());
    ASSERT_TRUE(connect_res->is_ok());
  }));

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(std::move(ep_client_end));

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    std::vector<fuchsia_hardware_usb_endpoint::VmoInfo> vmo_info;
    vmo_info.emplace_back(
        fuchsia_hardware_usb_endpoint::VmoInfo().id(1).size(zx_system_get_page_size()));
    auto reg_res = ep_client->RegisterVmos({std::move(vmo_info)});
    ASSERT_TRUE(reg_res.is_ok());

    std::vector<fuchsia_hardware_usb_request::Request> requests;
    requests.emplace_back()
        .defer_completion(false)
        .information(fuchsia_hardware_usb_request::RequestInfo::WithBulk(
            fuchsia_hardware_usb_request::BulkRequestInfo()))
        .data()
        .emplace()
        .emplace_back()
        .offset(0)
        .size(zx_system_get_page_size())
        .buffer(fuchsia_hardware_usb_request::Buffer::WithVmoId(1));
    auto queue_res = ep_client->QueueRequests(std::move(requests));
    ASSERT_TRUE(queue_res.is_ok());
  }));

  bus_.reset_remove();
  driver_test().driver()->ScheduleTask(0, driver_test().driver()->DeviceOffline(1));
  driver_test().driver()->RunUntilIdle();
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait_for_remove(); }));
  EXPECT_FALSE(driver_test().driver()->GetDeviceState()[0]);

  // Verify FIDL calls and channel unbind after DeviceOffline dropped device_state_[0] do not
  // trigger a Use-After-Free or null pointer dereference.
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    auto cancel_res = ep_client->CancelAll();
    ASSERT_TRUE(cancel_res.is_error());
    ASSERT_TRUE(cancel_res.error_value().is_domain_error());
    EXPECT_EQ(cancel_res.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);
    ep_client = {};
  }));
  driver_test().runtime().RunUntilIdle();
}

struct __PACKED TestFullConfigDescriptor {
  usb_configuration_descriptor_t config;
  usb_interface_descriptor_t interface;
  usb_endpoint_descriptor_t ep_in;
  usb_endpoint_descriptor_t ep_out;
};

constexpr TestFullConfigDescriptor kTestFullConfigDesc = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = htole16(sizeof(TestFullConfigDescriptor)),
            .b_num_interfaces = 1,
            .b_configuration_value = 1,
            .i_configuration = 0,
            .bm_attributes = 0xC0,
            .b_max_power = 50,
        },
    .interface =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = USB_CLASS_VENDOR,
            .b_interface_sub_class = 0,
            .b_interface_protocol = 0,
            .i_interface = 0,
        },
    .ep_in =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x81,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = htole16(512),
            .b_interval = 0,
        },
    .ep_out =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x02,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = htole16(512),
            .b_interval = 0,
        },
};

constexpr usb_device_descriptor_t kTestDeviceDesc = {
    .b_length = sizeof(usb_device_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice),
    .bcd_usb = htole16(0x0200),
    .b_device_class = 0,
    .b_device_sub_class = 0,
    .b_device_protocol = 0,
    .b_max_packet_size0 = 64,
    .id_vendor = htole16(0x18d1),
    .id_product = htole16(0x5026),
    .bcd_device = htole16(0x0100),
    .i_manufacturer = 0,
    .i_product = 0,
    .i_serial_number = 0,
    .b_num_configurations = 1,
};

const usb_hci_protocol_ops_t kTestXhciBanjoOps = {
    .request_queue =
        [](void* ctx, usb_request_t* usb_request,
           const usb_request_complete_callback_t* complete_cb) {
          static_cast<UsbXhci*>(ctx)->UsbHciRequestQueue(usb_request, complete_cb);
        },
    .set_bus_interface =
        [](void* ctx, const usb_bus_interface_protocol_t* bus_intf) {
          static_cast<UsbXhci*>(ctx)->UsbHciSetBusInterface(bus_intf);
        },
    .get_max_device_count =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetMaxDeviceCount(); },
    .enable_endpoint =
        [](void* ctx, uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
           const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
          return static_cast<UsbXhci*>(ctx)->UsbHciEnableEndpoint(device_id, ep_desc, ss_com_desc,
                                                                  enable);
        },
    .get_current_frame =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetCurrentFrame(); },
    .configure_hub =
        [](void* ctx, uint32_t device_id, usb_speed_t speed, const usb_hub_descriptor_t* desc,
           bool multi_tt) {
          return static_cast<UsbXhci*>(ctx)->UsbHciConfigureHub(device_id, speed, desc, multi_tt);
        },
    .hub_device_added =
        [](void* ctx, uint32_t device_id, uint32_t port, usb_speed_t speed) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceAdded(device_id, port, speed);
        },
    .hub_device_removed =
        [](void* ctx, uint32_t device_id, uint32_t port) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceRemoved(device_id, port);
        },
    .hub_device_reset =
        [](void* ctx, uint32_t device_id, uint32_t port) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceReset(device_id, port);
        },
    .reset_endpoint =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciResetEndpoint(device_id, ep_address);
        },
    .reset_device =
        [](void* ctx, uint32_t hub_address, uint32_t device_id) {
          return static_cast<UsbXhci*>(ctx)->UsbHciResetDevice(hub_address, device_id);
        },
    .get_max_transfer_size =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciGetMaxTransferSize(device_id, ep_address);
        },
    .cancel_all =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciCancelAll(device_id, ep_address);
        },
    .get_request_size =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetRequestSize(); },
};

// Implements the usb-bus UsbDevice role (EP0 control enumeration + fuchsia.hardware.usb.Usb server
// forwarding EnableEndpoint via UsbHci Banjo and ConnectToEndpoint via UsbHci FIDL).
class TestUsbBusDevice : public fidl::Server<fuchsia_hardware_usb::Usb> {
 public:
  TestUsbBusDevice(ddk::UsbHciProtocolClient hci_banjo,
                   fidl::ClientEnd<fuchsia_hardware_usb_hci::UsbHci> hci_fidl, uint32_t device_id,
                   usb_speed_t speed)
      : hci_banjo_(hci_banjo),
        hci_fidl_(std::move(hci_fidl)),
        device_id_(device_id),
        speed_(speed),
        parent_req_size_(hci_banjo_.GetRequestSize()) {}

  zx_status_t Init() {
    size_t actual = 0;
    zx_status_t status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice)) << 8u),
        0, nullptr, 0, &device_desc_, sizeof(device_desc_), &actual);
    if (status != ZX_OK || actual != sizeof(device_desc_)) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    usb_configuration_descriptor_t cfg_hdr = {};
    status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration))
            << 8u),
        0, nullptr, 0, &cfg_hdr, sizeof(cfg_hdr), &actual);
    if (status != ZX_OK || actual != sizeof(cfg_hdr)) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    config_desc_.resize(letoh16(cfg_hdr.w_total_length));
    status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration))
            << 8u),
        0, nullptr, 0, config_desc_.data(), config_desc_.size(), &actual);
    if (status != ZX_OK || actual != config_desc_.size()) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    return Control(kStandardDeviceOut,
                   fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
                   cfg_hdr.b_configuration_value, 0, nullptr, 0, nullptr, 0, nullptr);
  }

  void Bind(async_dispatcher_t* dispatcher, fidl::ServerEnd<fuchsia_hardware_usb::Usb> server_end) {
    bindings_.AddBinding(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
  }

  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override {
    auto res = hci_fidl_->ConnectToEndpoint(device_id_, request.ep_addr(), std::move(request.ep()));
    if (!res.ok()) {
      completer.Reply(fit::as_error(res.status()));
      return;
    }
    if (res->is_error()) {
      completer.Reply(fit::as_error(res->error_value()));
      return;
    }
    completer.Reply(fit::ok());
  }

  void GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) override {
    completer.Reply({{.speed = static_cast<fdescriptor::UsbSpeed>(speed_)}});
  }
  void GetDeviceId(GetDeviceIdCompleter::Sync& completer) override {
    completer.Reply({{.device_id = device_id_}});
  }
  void GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) override {
    fdescriptor::UsbDeviceDescriptor desc{{
        .b_length = device_desc_.b_length,
        .b_descriptor_type = device_desc_.b_descriptor_type,
        .bcd_usb = le16toh(device_desc_.bcd_usb),
        .b_device_class = device_desc_.b_device_class,
        .b_device_sub_class = device_desc_.b_device_sub_class,
        .b_device_protocol = device_desc_.b_device_protocol,
        .b_max_packet_size0 = device_desc_.b_max_packet_size0,
        .id_vendor = le16toh(device_desc_.id_vendor),
        .id_product = le16toh(device_desc_.id_product),
        .bcd_device = le16toh(device_desc_.bcd_device),
        .i_manufacturer = device_desc_.i_manufacturer,
        .i_product = device_desc_.i_product,
        .i_serial_number = device_desc_.i_serial_number,
        .b_num_configurations = device_desc_.b_num_configurations,
    }};
    completer.Reply({{.descriptor = desc}});
  }
  void GetConfigurationDescriptor(GetConfigurationDescriptorRequest& request,
                                  GetConfigurationDescriptorCompleter::Sync& completer) override {
    completer.Reply(fit::ok(
        fuchsia_hardware_usb::UsbGetConfigurationDescriptorResponse{{.descriptor = config_desc_}}));
  }
  void GetConfiguration(GetConfigurationCompleter::Sync& completer) override {
    completer.Reply({{.configuration = 1}});
  }
  void SetConfiguration(SetConfigurationRequest& request,
                        SetConfigurationCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }
  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }
  void ControlTransfer(ControlTransferRequest& request,
                       ControlTransferCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }
  void EnableEndpoint(EnableEndpointRequest& request,
                      EnableEndpointCompleter::Sync& completer) override {
    const usb_endpoint_descriptor_t ep_desc = {
        .b_length = request.ep_desc().b_length(),
        .b_descriptor_type = request.ep_desc().b_descriptor_type(),
        .b_endpoint_address = request.ep_desc().b_endpoint_address(),
        .bm_attributes = request.ep_desc().bm_attributes(),
        .w_max_packet_size = htole16(request.ep_desc().w_max_packet_size()),
        .b_interval = request.ep_desc().b_interval(),
    };
    zx_status_t status = hci_banjo_.EnableEndpoint(device_id_, &ep_desc, nullptr, request.enable());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void ResetEndpoint(ResetEndpointRequest& request,
                     ResetEndpointCompleter::Sync& completer) override {
    zx_status_t status = hci_banjo_.ResetEndpoint(device_id_, request.ep_address());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb::Usb> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  zx_status_t Control(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                      const void* write_buf, size_t write_size, void* read_buf, size_t read_size,
                      size_t* out_actual) {
    const bool is_out = usb_request_is_out(request_type);
    const size_t length = is_out ? write_size : read_size;
    std::optional<usb::Request<void>> req;
    zx_status_t status = usb::Request<void>::Alloc(&req, length, 0, parent_req_size_);
    if (status != ZX_OK || !req.has_value()) {
      return status != ZX_OK ? status : ZX_ERR_INTERNAL;
    }
    usb::Request<void>& req_val = *req;
    req_val.request()->setup.bm_request_type = request_type;
    req_val.request()->setup.b_request = request;
    req_val.request()->setup.w_value = value;
    req_val.request()->setup.w_index = index;
    req_val.request()->setup.w_length = static_cast<uint16_t>(length);
    req_val.request()->header.device_id = device_id_;
    req_val.request()->header.ep_address = 0;
    req_val.request()->header.length = length;
    if (is_out && length > 0) {
      ZX_ASSERT(req_val.CopyTo(write_buf, length, 0) == static_cast<ssize_t>(length));
    }
    sync_completion_t completion;
    usb_request_complete_callback_t complete_cb = {
        .callback =
            [](void* ctx, usb_request_t*) {
              sync_completion_signal(static_cast<sync_completion_t*>(ctx));
            },
        .ctx = &completion,
    };
    hci_banjo_.RequestQueue(req_val.request(), &complete_cb);
    sync_completion_wait(&completion, ZX_TIME_INFINITE);
    status = req_val.request()->response.status;
    if (status == ZX_OK && !is_out) {
      const size_t actual = req_val.request()->response.actual;
      if (length > 0 && actual > 0) {
        auto copy_res = req_val.CachedCopyFrom(read_buf, actual, 0);
        ZX_ASSERT(copy_res.is_ok() && copy_res.value() == actual);
      }
      if (out_actual) {
        *out_actual = actual;
      }
    }
    req_val.Release();
    return status;
  }

  ddk::UsbHciProtocolClient hci_banjo_;
  fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_fidl_;
  uint32_t device_id_;
  usb_speed_t speed_;
  size_t parent_req_size_;
  usb_device_descriptor_t device_desc_ = {};
  std::vector<uint8_t> config_desc_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb::Usb> bindings_;
};

// Multi-driver integration test covering UsbXhci <-> UsbBus/UsbDevice <-> FIDL Usb/Endpoint
// across repeated M1/M4 attach and M2 unplug cycles with in-flight FIDL and Banjo transfers.
TEST_F(XhciMmioHarness, UsbBusAndUsbDeviceMultiCycleHotplugWithInFlightFidlEndpoint) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
      return;
    }
    // Auto-complete EP0 control requests queued during device enumeration (GET_DESCRIPTOR and
    // SET_CONFIGURATION) so enumeration succeeds end-to-end.
    if (target == 1) {
      auto state = xhci->GetDeviceState()[doorbell - 1];
      if (!state) {
        return;
      }
      fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> pending_trbs;
      Endpoint* ep0 = nullptr;
      {
        fbl::AutoLock _(&state->transaction_lock());
        ep0 = &state->GetEndpoint();
        pending_trbs = ep0->transfer_ring().TakePendingTRBs();
      }
      for (auto& trb : pending_trbs) {
        if (!trb.request.has_value()) {
          continue;
        }
        size_t actual = 0;
        if (std::holds_alternative<Request>(*trb.request)) {
          auto& req = std::get<Request>(*trb.request);
          const usb_setup_t& setup = req.request()->setup;
          if (setup.b_request == fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor)) {
            const uint8_t desc_type =
                static_cast<uint8_t>(static_cast<uint32_t>(setup.w_value) >> 8u);
            if (desc_type == fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice)) {
              actual = std::min<size_t>(setup.w_length, sizeof(kTestDeviceDesc));
              EXPECT_EQ(static_cast<size_t>(req.CopyTo(&kTestDeviceDesc, actual, 0)), actual);
            } else if (desc_type ==
                       fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration)) {
              actual = std::min<size_t>(setup.w_length, sizeof(kTestFullConfigDesc));
              EXPECT_EQ(static_cast<size_t>(req.CopyTo(&kTestFullConfigDesc, actual, 0)), actual);
            }
          }
        }
        ep0->RequestComplete(ZX_OK, actual, std::move(*trb.request));
      }
    }
  });

  usb_hci_protocol_t hci_proto = {
      .ops = &kTestXhciBanjoOps,
      .ctx = xhci,
  };
  ddk::UsbHciProtocolClient hci_banjo_client(&hci_proto);

  auto usb_bus_dispatcher = driver_test().runtime().StartBackgroundDispatcher();

  for (size_t cycle = 0; cycle < 2; ++cycle) {
    SCOPED_TRACE(::testing::Message() << "Hotplug cycle " << cycle);

    // 1. Simulate physical USB device attach on port 1.
    FakeUsbDevice fake_dev = ConnectDevice(1, USB_SPEED_HIGH);
    const uint32_t device_id = fake_dev.device_id;
    const uint8_t slot_id = static_cast<uint8_t>(device_id + 1);

    zx::result hci_fidl_client =
        driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
    ASSERT_OK(hci_fidl_client);

    // 2. Enumerate the attached device over EP0 and bind its fuchsia.hardware.usb.Usb server.
    std::unique_ptr<TestUsbBusDevice> bus_device;
    auto [usb_client_end, usb_server_end] = fidl::Endpoints<fuchsia_hardware_usb::Usb>::Create();
    EXPECT_OK(fdf::RunOnDispatcherSync(usb_bus_dispatcher->async_dispatcher(), [&] {
      bus_device = std::make_unique<TestUsbBusDevice>(hci_banjo_client, std::move(*hci_fidl_client),
                                                      device_id, fake_dev.speed);
      ASSERT_OK(bus_device->Init());
      bus_device->Bind(usb_bus_dispatcher->async_dispatcher(), std::move(usb_server_end));
    }));

    // 3. Connect to Bulk IN endpoint 0x81 through the fuchsia.hardware.usb.Usb server (matching
    // starnix/modules/usbfs), which enables the endpoint in UsbXhci and binds the FIDL Endpoint
    // server in xhci-endpoint.cc.
    auto [ep_client_end, ep_server_end] =
        fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
    fidl::SyncClient<fuchsia_hardware_usb::Usb> usb_client(std::move(usb_client_end));

    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      auto desc_res = usb_client->GetDeviceDescriptor();
      ASSERT_TRUE(desc_res.is_ok());

      fdescriptor::UsbEndpointDescriptor ep_in_desc{{
          .b_length = sizeof(usb_endpoint_descriptor_t),
          .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint),
          .b_endpoint_address = 0x81,
          .bm_attributes = std::to_underlying(fdescriptor::EndpointType::kBulk),
          .w_max_packet_size = 512,
          .b_interval = 0,
      }};
      fdescriptor::UsbSsEpCompDescriptor ss_comp{};
      auto enable_res = usb_client->EnableEndpoint(
          {{.ep_desc = ep_in_desc, .ss_com_desc = ss_comp, .enable = true}});
      ASSERT_TRUE(enable_res.is_ok());

      auto connect_res =
          usb_client->ConnectToEndpoint({{.ep_addr = 0x81, .ep = std::move(ep_server_end)}});
      ASSERT_TRUE(connect_res.is_ok());
    }));

    fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(std::move(ep_client_end));

    // 4. Register VMOs and queue an in-flight FIDL bulk request on 0x81 (pins BTI PMT in xhci).
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      std::vector<fuchsia_hardware_usb_endpoint::VmoInfo> vmo_info;
      vmo_info.emplace_back(
          fuchsia_hardware_usb_endpoint::VmoInfo().id(1).size(zx_system_get_page_size()));
      auto reg_res = ep_client->RegisterVmos({std::move(vmo_info)});
      ASSERT_TRUE(reg_res.is_ok());

      std::vector<fuchsia_hardware_usb_request::Request> requests;
      requests.emplace_back()
          .defer_completion(false)
          .information(fuchsia_hardware_usb_request::RequestInfo::WithBulk(
              fuchsia_hardware_usb_request::BulkRequestInfo()))
          .data()
          .emplace()
          .emplace_back()
          .offset(0)
          .size(zx_system_get_page_size())
          .buffer(fuchsia_hardware_usb_request::Buffer::WithVmoId(1));
      auto queue_res = ep_client->QueueRequests(std::move(requests));
      ASSERT_TRUE(queue_res.is_ok());
    }));

    // 5. Also enable Bulk OUT endpoint 0x02 and queue an in-flight Banjo usb::Request with a VMO
    // (which UsbXhci pins via PhysMap(hci_->bti())) so both FIDL and Banjo requests are pending on
    // transfer rings when DeviceOffline runs.
    EnableEndpoint(device_id, 2, false);
    driver_test().driver()->RunUntilIdle();

    libsync::Completion banjo_req_completed;
    zx_status_t banjo_completion_status = ZX_OK;
    std::optional<usb::Request<void>> banjo_req;
    ASSERT_OK(usb::Request<void>::Alloc(&banjo_req, zx_system_get_page_size(), 0x02,
                                        driver_test().driver()->UsbHciGetRequestSize()));
    ASSERT_TRUE(banjo_req.has_value());
    usb::Request<void>& banjo_req_val = *banjo_req;
    banjo_req_val.request()->header.device_id = device_id;
    banjo_req_val.request()->header.ep_address = 0x02;
    banjo_req_val.request()->header.length = zx_system_get_page_size();
    usb_request_complete_callback_t banjo_cb = {
        .callback =
            [](void* ctx, usb_request_t* req) {
              auto* state_pair = static_cast<std::pair<zx_status_t*, libsync::Completion*>*>(ctx);
              *state_pair->first = req->response.status;
              usb::Request<void> owned_req(req, sizeof(usb_request_t));
              state_pair->second->Signal();
            },
        .ctx = nullptr,
    };
    std::pair<zx_status_t*, libsync::Completion*> cb_ctx(&banjo_completion_status,
                                                         &banjo_req_completed);
    banjo_cb.ctx = &cb_ctx;
    driver_test().driver()->UsbHciRequestQueue(banjo_req_val.take(), &banjo_cb);

    // 6. Simulate physical cable unplug / role transition while both FIDL and Banjo transfers are
    // in flight and the FIDL Endpoint channel is still bound.
    bus_.reset_remove();
    driver_test().driver()->ScheduleTask(0, driver_test().driver()->DeviceOffline(slot_id));
    driver_test().driver()->RunUntilIdle();
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait_for_remove(); }));
    EXPECT_FALSE(driver_test().driver()->GetDeviceState()[device_id]);

    // Both the in-flight Banjo request and the in-flight FIDL request must have been unmapped and
    // completed with ZX_ERR_IO_NOT_PRESENT by UsbXhci::DeviceOffline.
    EXPECT_OK(banjo_req_completed.Wait(zx::sec(5)));
    EXPECT_EQ(banjo_completion_status, ZX_ERR_IO_NOT_PRESENT);

    // 7. Tear down the bus device node and close the FIDL Endpoint channel AFTER
    // device_state_[device_id] has already been dropped by UsbXhci::DeviceOffline.
    EXPECT_OK(fdf::RunOnDispatcherSync(usb_bus_dispatcher->async_dispatcher(),
                                       [&] { bus_device.reset(); }));

    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      auto cancel_res = ep_client->CancelAll();
      ASSERT_TRUE(cancel_res.is_error());
      ASSERT_TRUE(cancel_res.error_value().is_domain_error());
      EXPECT_EQ(cancel_res.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);
      ep_client = {};
      usb_client = {};
    }));
    driver_test().runtime().RunUntilIdle();
  }
}

TEST_F(XhciMmioHarness, HubDeviceRemovedWhenBusInvalidDoesNotAssert) {
  // 1. Connect a simulated USB hub on root hub port 1.
  FakeUsbDevice hub_dev = ConnectDevice(1, USB_SPEED_HIGH);
  const uint32_t hub_id = hub_dev.device_id;
  auto hub_state = driver_test().driver()->GetDeviceState()[hub_id];
  ASSERT_TRUE(hub_state);

  // 2. Connect a child device on root hub port 2 (to allocate device state and slot).
  FakeUsbDevice child_dev = ConnectDevice(2, USB_SPEED_FULL);
  const uint32_t child_device_id = child_dev.device_id;
  auto child_state = driver_test().driver()->GetDeviceState()[child_device_id];
  ASSERT_TRUE(child_state);

  // 3. Configure the hub state so child_device_id is attached downstream of hub port 1.
  HubInfo hub_info = {};
  hub_info.port_to_device[0] = static_cast<uint8_t>(child_device_id);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    hub_state->GetHubLocked() = hub_info;
  }

  // 4. Simulate bus unbind prior to xhci shutdown, clearing the bus interface
  // (matching DFv2 bottom-up unbind where usb-bus clears the bus interface).
  driver_test().driver()->UsbHciSetBusInterface(nullptr);

  // 5. Invoke UsbHciHubDeviceRemoved for the child device on hub port 1.
  // Without the fix, ZX_ASSERT(bus_.is_valid()) fails fatally because bus_ was cleared.
  // With the fix, UsbHciHubDeviceRemoved returns ZX_OK safely.
  EXPECT_OK(driver_test().driver()->UsbHciHubDeviceRemoved(hub_id, 1));
}

TEST_F(XhciMmioHarness, IrqCountClampedToPDevDeviceInfo) {
  // Verify that UsbXhci clamps irq_count_ to the PDev IRQ count instead of allocating
  // excess interrupters beyond what the platform device provides.
  EXPECT_EQ(driver_test().driver()->irq_count(), 1u);
}

TEST_F(XhciMmioHarness, ConnectToEndpointBoundsCheck) {
  zx::result hci_client_end =
      driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
  ASSERT_OK(hci_client_end);

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_client(std::move(*hci_client_end));

    // 1. Invalid device_id (out of bounds).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(1000, 1U | 0x80U, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 2. Invalid endpoint address: ep 0 (EP0 is control, not connected via ConnectToEndpoint).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 3. Invalid endpoint address: 0x80 (ep 0 IN).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0x80, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 4. Invalid endpoint address: 0x10 (out of USB endpoint index range).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0x10, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }
  }));
}

TEST_F(XhciMmioHarness, RequestQueueBoundsCheck) {
  ConnectDevice(1, USB_SPEED_HIGH);

  // 1. Invalid device_id
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 1, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 1000;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }

  // 2. Invalid ep_address (0x80)
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 0x80, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 0;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }

  // 3. Invalid ep_address (0x10)
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 0x10, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 0;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }
}

TEST_F(XhciMmioHarness, ResetEndpointBoundsCheck) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(1000, 1), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 0x80), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 0x10), ZX_ERR_INVALID_ARGS);
}

TEST_F(XhciMmioHarness, CancelAllBoundsAndEP0) {
  ConnectDevice(1, USB_SPEED_HIGH);

  // 1. Invalid device_id
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(1000, 1), ZX_ERR_INVALID_ARGS);

  // 2. Invalid ep_address (0x80)
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(0, 0x80), ZX_ERR_INVALID_ARGS);

  // 3. Invalid ep_address (0x10)
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(0, 0x10), ZX_ERR_INVALID_ARGS);

  // 4. CancelAll on EP0 (ep_address == 0)
  // Complete AddressDeviceCommand
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  // Now execute CancelAll on EP0 (ep_address == 0).
  // This sends StopEndpoint for DCI 1 (ENDPOINT == 1).
  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::StopEndpointCommand: {
          auto stop = reinterpret_cast<StopEndpoint*>(cr);
          EXPECT_EQ(stop->ENDPOINT(), 1u);
          EXPECT_EQ(stop->SLOT(), 1u);
          got_stop_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &success_event));
        } break;
      }
    }
  });

  zx_status_t status = driver_test().driver()->UsbHciCancelAll(0, 0);
  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_OK(status);
}

}  // namespace

}  // namespace usb_xhci
