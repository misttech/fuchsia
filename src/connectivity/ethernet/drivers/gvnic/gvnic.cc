// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/gvnic/gvnic.h"

#include <lib/async/cpp/task.h>
#include <lib/ddk/binding_driver.h>
#include <lib/fit/defer.h>

#include <atomic>
#include <limits>

#include "src/connectivity/ethernet/drivers/gvnic/abi.h"

#define DIV_ROUND_UP(a, b) (((a) + (b) - 1) / (b))
#define ROUND_UP(a, b) (DIV_ROUND_UP(a, b) * b)

#define SET_LOCAL_REG_AND_WRITE_TO_MMIO(f, v)                                       \
  do {                                                                              \
    regs_.f = (v);                                                                  \
    reg_mmio_->Write<decltype(regs_.f)::wrapped_type>(regs_.f.GetBE(),              \
                                                      offsetof(GvnicRegisters, f)); \
  } while (0)

#define READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(f) \
  (regs_.f.SetBE(reg_mmio_->Read<decltype(regs_.f)::wrapped_type>(offsetof(GvnicRegisters, f))))

#define POLL_AT_MOST_N_TIMES_UNTIL_CONDITION_IS_MET(n, cond, msg) \
  do {                                                            \
    uint32_t loop_limit = n;                                      \
    while (unlikely(!(cond))) {                                   \
      ZX_ASSERT_MSG(loop_limit--, "Polling timed out for " msg);  \
      zx_nanosleep(zx_deadline_after(ZX_USEC(10)));               \
      zxlogf(DEBUG, "Polling for " msg);                          \
    }                                                             \
  } while (0)

#define CACHELINE_SIZE (64)
#define GVNIC_PACKET_BUFFER_SIZE_ALIGNMENT (2048)

namespace gvnic {

zx_status_t Gvnic::Bind(void* ctx, zx_device_t* dev) {
  zx_status_t status;

  auto driver = std::make_unique<Gvnic>(dev);
  status = driver->Bind();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't bind: %s", zx_status_get_string(status));
    return status;
  }

  // The DriverFramework now owns driver.
  [[maybe_unused]] auto ptr = driver.release();

  zxlogf(INFO, "Succeess.");

  return ZX_OK;
}

zx_status_t Gvnic::Bind() {
  zx_status_t status;

  status = SetUpPci();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't set up PCI: %s", zx_status_get_string(status));
    return status;
  }
  status = MapBars();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't map bars: %s", zx_status_get_string(status));
    return status;
  }
  ResetCard(true);
  static constexpr const char* const version_string = "Fuchsia gvnic driver " GVNIC_VERSION;
  status = WriteVersion(version_string, strlen(version_string));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't write version: %s", zx_status_get_string(status));
    return status;
  }
  status = CreateAdminQueue();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create admin queue: %s", zx_status_get_string(status));
    return status;
  }
  status = EnableCard();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't enable card: %s", zx_status_get_string(status));
    return status;
  }
  status = DescribeDevice();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't describe device: %s", zx_status_get_string(status));
    return status;
  }
  status = ReportLinkSpeed();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't report link speed: %s", zx_status_get_string(status));
    return status;
  }
  status = ConfigureDeviceResources();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't configure device resources: %s", zx_status_get_string(status));
    return status;
  }
  status = RegisterPageList(tx_page_list_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't register tx page list: %s", zx_status_get_string(status));
    return status;
  }
  status = RegisterPageList(rx_page_list_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't register rx page list: %s", zx_status_get_string(status));
    return status;
  }
  status = CreateTXQueue();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create tx queue: %s", zx_status_get_string(status));
    return status;
  }
  status = CreateRXQueue();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create rx queue: %s", zx_status_get_string(status));
    return status;
  }
  status = SetUpInterrupts();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't set up interrupts: %s", zx_status_get_string(status));
    return status;
  }
  status = StartRXThread();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't start rx thread: %s", zx_status_get_string(status));
    return status;
  }
  status = AddDevice();
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't add device: %s", zx_status_get_string(status));
    return status;
  }
  is_bound.Set(true);

  return ZX_OK;
}

zx_status_t Gvnic::AddDevice() {
  zx::result dispatcher =
      fdf::UnsynchronizedDispatcher::Create({}, "netdev-dispatcher", [this](fdf_dispatcher_t*) {
        netdevice_dispatcher_.close();
        netdevice_dispatcher_shutdown_.Signal();
      });
  if (dispatcher.is_error()) {
    zxlogf(ERROR, "Failed to create netdevice dispatcher: %s", dispatcher.status_string());
    return dispatcher.status_value();
  }
  netdevice_dispatcher_ = std::move(dispatcher.value());

  // If this method fails at some point we must shut down the dispatcher.
  auto cleanup = fit::defer([this] {
    if (netdevice_dispatcher_.get()) {
      netdevice_dispatcher_.ShutdownAsync();
      netdevice_dispatcher_shutdown_.Wait();
    }
  });

  auto protocol = [this](fdf::ServerEnd<netdev::NetworkDeviceImpl> server_end) mutable {
    fdf::BindServer(netdevice_dispatcher_.get(), std::move(server_end), this);
  };

  // Register the callback to handler.
  netdev::Service::InstanceHandler handler({.network_device_impl = std::move(protocol)});

  if (!outgoing_dir_.has_value()) {
    outgoing_dir_.emplace(fdf::OutgoingDirectory::Create(fdf::Dispatcher::GetCurrent()->get()));
  }

  auto status = outgoing_dir_->AddService<netdev::Service>(std::move(handler));
  if (status.is_error()) {
    zxlogf(ERROR, "Failed to add service to outgoing directory: %s\n", status.status_string());
    return status.error_value();
  }

  auto [client, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();

  auto result = outgoing_dir_->Serve(std::move(server));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to serve outgoing directory: %s\n", result.status_string());
    return result.error_value();
  }

  std::array offers{netdev::Service::Name};

  ddk::DeviceAddArgs args = ddk::DeviceAddArgs("gvnic")
                                .set_inspect_vmo(inspect_.DuplicateVmo())
                                .set_fidl_service_offers(offers)
                                .set_outgoing_dir(client.TakeChannel())
                                .set_proto_id(ZX_PROTOCOL_NETWORK_DEVICE_IMPL);

  if (zx_status_t status = DdkAdd(args); status != ZX_OK) {
    zxlogf(ERROR, "Couldn't add device: %s", zx_status_get_string(status));
    return status;
  }

  // Now that the device has been successfully added the cleanup isn't needed anymore.
  cleanup.cancel();

  return ZX_OK;
}

zx_status_t Gvnic::SetUpPci() {
  zx_status_t status;

  pci_ = ddk::Pci::FromFragment(parent());
  if (!pci_.is_valid()) {
    zxlogf(ERROR, "Couldn't create pci object from parent fragment");
    return ZX_ERR_INTERNAL;
  }
  status = pci_.GetBti(0, &bti_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't get bti from pci: %s", zx_status_get_string(status));
    return status;
  }
  status = pci_.SetBusMastering(true);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't set pci bus mastering: %s", zx_status_get_string(status));
    return status;
  }
  buffer_factory_ = dma_buffer::CreateBufferFactory();
  return ZX_OK;
}

zx_status_t Gvnic::SetUpInterrupts() {
  zx_status_t status;

  fuchsia_hardware_pci::InterruptMode mode;
  status = pci_.ConfigureInterruptMode(kNumMsixVectors, &mode);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't configure interrupt mode: %s", zx_status_get_string(status));
    return status;
  }

  status = pci_.MapInterrupt(tx_irq_index_, &tx_interrupt_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't map tx interrupt: %s", zx_status_get_string(status));
    return status;
  }

  status = pci_.MapInterrupt(rx_irq_index_, &rx_interrupt_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't map rx interrupt: %s", zx_status_get_string(status));
    return status;
  }
  return ZX_OK;
}

zx_status_t Gvnic::MapBars() {
  zx_status_t status;

  status = pci_.MapMmio(GVNIC_REGISTER_BAR, ZX_CACHE_POLICY_UNCACHED_DEVICE, &reg_mmio_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't map mmio for the regiter bar: %s", zx_status_get_string(status));
    return status;
  }
  status = pci_.MapMmio(GVNIC_DOORBELL_BAR, ZX_CACHE_POLICY_UNCACHED_DEVICE, &doorbell_mmio_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't map mmio for the doorbell bar: %s", zx_status_get_string(status));
    return status;
  }
  // Initialize the local copies of regs_.
  memset(&regs_, 0, sizeof(regs_));
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dev_status);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(drv_status);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(max_tx_queues);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(max_rx_queues);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_pfn);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_counter);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dma_mask);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_base_address_hi);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_base_address_lo);
  READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_length);
  return ZX_OK;
}

void Gvnic::ResetCard(bool use_new_reset_sequence) {
  if (use_new_reset_sequence) {
    // A driver can cause a device reset by writing GVE_RESET to the DRV_STATUS register. Drivers
    // should then poll the DEV_STATUS register until the GVE_IS_RESET bit is set to confirm the AQ
    // was released and the device reset is complete.
    if (!(READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dev_status) &
          GVNIC_DEVICE_STATUS_DEVICE_IS_RESET)) {
      SET_LOCAL_REG_AND_WRITE_TO_MMIO(drv_status, GVNIC_DRIVER_STATUS_RESET);
      // In practice, this "polling" succeeds instantly.
      POLL_AT_MOST_N_TIMES_UNTIL_CONDITION_IS_MET(
          10000,
          READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dev_status) &
              GVNIC_DEVICE_STATUS_DEVICE_IS_RESET,
          "reset completion (new)");
    }
  } else {
    // To be compatible with older driver versions, writing 0x0 to the ADMIN_QUEUE_PFN register will
    // also cause a device reset but all future driver versions should use the DRV_STATUS register.
    // Older drivers using the ADMIN_QUEUE_PFN register should then poll the ADMIN_QUEUE_PFN
    // register until it reads back 0 to confirm the AQ was released and the device reset is
    // complete.
    SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_pfn, 0);
    // In practice, this "polling" succeeds instantly.
    POLL_AT_MOST_N_TIMES_UNTIL_CONDITION_IS_MET(
        10000, READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_pfn) == 0,
        "reset completion (old)");
  }
  // Now that we know the device is not using the admin queue (or any other physical addresses) we
  // can release any quarentined pages.
  bti_.release_quarantine();
}

zx_status_t Gvnic::WriteVersion(const char* ver, uint32_t len) {
  // DRIVER_VERSION Register:
  // Drivers should identify themselves by writing a human readable version string to this register
  // one byte at a time, followed by a newline.
  while (len--) {
    SET_LOCAL_REG_AND_WRITE_TO_MMIO(driver_version, *ver++);
  }
  // Terminate by writing the newline.
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(driver_version, '\n');
  return ZX_OK;
}

zx_status_t Gvnic::CreateAdminQueue() {
  zx_status_t status;

  ZX_ASSERT_MSG(!admin_queue_, "Admin Queue alredy allocated.");
  status = buffer_factory_->CreateContiguous(bti_, zx_system_get_page_size(), 0,
                                             dma_buffer::CacheOptions::kEnabled, &admin_queue_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create admin queue: %s", zx_status_get_string(status));
    return status;
  }
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_length, GVNIC_ADMINQ_SIZE);
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_base_address_hi,
                                  static_cast<uint32_t>(admin_queue_->phys() >> 32));
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_base_address_lo,
                                  static_cast<uint32_t>(admin_queue_->phys() & 0xffffffff));
  ZX_ASSERT_MSG(!scratch_page_, "Scratch page alredy allocated.");
  status = buffer_factory_->CreateContiguous(bti_, zx_system_get_page_size(), 0,
                                             dma_buffer::CacheOptions::kEnabled, &scratch_page_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create scratch page: %s", zx_status_get_string(status));
    return status;
  }
  return ZX_OK;
}

zx_status_t Gvnic::EnableCard() {
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(drv_status, GVNIC_DRIVER_STATUS_RUN);
  if (READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dev_status) &
      GVNIC_DEVICE_STATUS_DEVICE_IS_RESET) {
    const uint64_t pfn = admin_queue_->phys() / zx_system_get_page_size();
    SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_pfn, static_cast<uint32_t>(pfn));
  }
  return ZX_OK;
}

zx_status_t Gvnic::DescribeDevice() {
  ZX_ASSERT_MSG(scratch_page_, "Scratch page not allocated.");
  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_DESCRIBE_DEVICE;
  ent->status = 0x0;  // Device will set this when the command completes.
  ent->describe_device.device_descriptor_addr = scratch_page_->phys();
  ent->describe_device.device_descriptor_version = 1;
  ent->describe_device.available_length = static_cast<uint32_t>(scratch_page_->size());
  ZX_ASSERT_MSG(ent->describe_device.available_length == scratch_page_->size(),
                "length did not fit in uint32_t");
  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Describe device command status is not 'passed' (0x%x)", ent->status.val());
    return ZX_ERR_INTERNAL;
  }
  auto volatile dev_descr_in_scratch =
      reinterpret_cast<GvnicDeviceDescriptor*>(scratch_page_->virt());
  dev_descr_ = *dev_descr_in_scratch;

  const uint64_t max_options =
      (scratch_page_->size() - sizeof(dev_descr_in_scratch[0])) / sizeof(GvnicDeviceOption);
  if (dev_descr_.num_device_options >= max_options) {
    zxlogf(WARNING,
           "# of device options (%hu) is >= max_options (%lu). Options might be incomplete.",
           dev_descr_.num_device_options.val(), max_options);
  }
  dev_opts_.reset(new GvnicDeviceOption[dev_descr_.num_device_options]);
  auto options_in_scratch = reinterpret_cast<GvnicDeviceOption*>(&dev_descr_in_scratch[1]);

  for (int o = 0; o < dev_descr_.num_device_options; o++) {
    dev_opts_[o] = options_in_scratch[o];
  }

  const uint32_t full_rounded_mtu =
      ROUND_UP(dev_descr_.mtu + ETH_HLEN + GVNIC_RX_PADDING, GVNIC_PACKET_BUFFER_SIZE_ALIGNMENT);
  ZX_ASSERT_MSG(full_rounded_mtu <= std::numeric_limits<uint16_t>::max(),
                "full_rounded_mtu did not fit in uint16_t");
  rounded_mtu_ = static_cast<uint16_t>(full_rounded_mtu);

  return ZX_OK;
}

zx_status_t Gvnic::ReportLinkSpeed() {
  ZX_ASSERT_MSG(scratch_page_, "Scratch page not allocated.");
  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_REPORT_LINK_SPEED;
  ent->status = 0x0;  // Device will set this when the command completes.
  ent->report_link_speed.link_speed_address = scratch_page_->phys();
  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Report link speed command status is not 'passed' (0x%x)", ent->status.val());
    return ZX_ERR_INTERNAL;
  }

  auto volatile const ls_ptr = reinterpret_cast<GvnicAdminqLinkSpeed*>(scratch_page_->virt());
  link_speed_ = ls_ptr->link_speed;
  return ZX_OK;
}

zx_status_t Gvnic::ConfigureDeviceResources() {
  zx_status_t status;

  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_CONFIGURE_DEVICE_RESOURCES;
  ent->status = 0x0;  // Device will set this when the command completes.

  ZX_ASSERT_MSG(!counter_page_, "Counter page alredy allocated.");
  const uint32_t num_counters = dev_descr_.event_counters;
  status = buffer_factory_->CreateContiguous(
      bti_, ROUND_UP(num_counters * sizeof(uint32_t), zx_system_get_page_size()), 0,
      dma_buffer::CacheOptions::kEnabled, &counter_page_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Couldn't create counter page: %s", zx_status_get_string(status));
    return status;
  }
  if (!irq_doorbell_idx_page_) {
    status = buffer_factory_->CreateContiguous(bti_, zx_system_get_page_size(), 0,
                                               dma_buffer::CacheOptions::kEnabled,
                                               &irq_doorbell_idx_page_);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create irq doorbell index page: %s", zx_status_get_string(status));
      return status;
    }
  }

  ent->device_resources.counter_array = counter_page_->phys();
  ent->device_resources.irq_db_addr_base = irq_doorbell_idx_page_->phys();

  // The IRQ doorbells themselves are in BAR2. The IRQ doorbell indexes are filled in to indicate
  // which ones are used by IRQs (and therefore cannot be used by TX or RX queues). For the TX
  // queue(s), the doorbell is incremented for each unit of data (descriptor) to be sent, and the
  // corresponding counter acks that the transmit is complete, and the buffer(s) can be reused. For
  // RX queue(s), the doorbell is incremented for each "empty" packet buffer provided, and the
  // counter indicates when they have been filled.
  ent->device_resources.num_counters = num_counters;
  ent->device_resources.num_irq_dbs = kNumIrqDoorbellIdxs;
  ent->device_resources.irq_db_stride = sizeof(uint32_t) * kIrqDoorbellIdxStride;

  // From the docs:
  //
  // If ntfy_blk_msix_base_idx is 0, that means that the ntfy blocks start at the first (0 index)
  // interrupt, making the management interrupt the last interrupt. If ntfy_blk_msix_base_idx is 1,
  // that means that the ntfy blocks start at the second (1 index) interrupt, making the management
  // interrupt the first interrupt.
  ent->device_resources.ntfy_blk_msix_base_idx = 0;
  ent->device_resources.queue_format = GVNIC_GQI_QPL_FORMAT;

  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Configure device resources command status is not 'passed' (0x%x)",
           ent->status.val());
    return ZX_ERR_INTERNAL;
  }
  return ZX_OK;
}

zx_status_t Gvnic::RegisterPageList(std::unique_ptr<PageList>& page_list) {
  ZX_ASSERT_MSG(scratch_page_, "Scratch page not allocated.");
  page_list.reset(new PageList(buffer_factory_, bti_, scratch_page_));

  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_REGISTER_PAGE_LIST;
  ent->status = 0x0;  // Device will set this when the command completes.

  ent->register_page_list.page_list_id = page_list->id();
  ent->register_page_list.num_pages = page_list->length();
  ent->register_page_list.page_list_address = scratch_page_->phys();
  ent->register_page_list.page_size = scratch_page_->size();

  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Register page list command status is not 'passed' (0x%x)", ent->status.val());
    return ZX_ERR_INTERNAL;
  }
  return ZX_OK;
}

zx_status_t Gvnic::CreateTXQueue() {
  zx_status_t status;

  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_CREATE_TX_QUEUE;
  ent->status = 0x0;  // Device will set this when the command completes.

  const uint32_t qr_idx = GetNextQueueResourcesIndex();
  tx_queue_resources_ = GetQueueResourcesVirtAddr(qr_idx);

  tx_ring_len_ = dev_descr_.tx_queue_size;
  ZX_ASSERT_MSG(rounded_mtu_ * tx_ring_len_ <= tx_page_list_->pages()->size(),
                "Can't fit %u MTUs (%u bytes each) (= %u bytes) in the TX page list (%lu bytes)",
                tx_ring_len_, rounded_mtu_, rounded_mtu_ * tx_ring_len_,
                tx_page_list_->pages()->size());
  if (!tx_ring_) {
    status = buffer_factory_->CreateContiguous(bti_, tx_ring_len_ * sizeof(GvnicTxPktDesc), 0,
                                               dma_buffer::CacheOptions::kEnabled, &tx_ring_);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create tx ring: %s", zx_status_get_string(status));
      return status;
    }
  }

  ent->create_tx_queue.queue_id = 0;  // Only making one TX Queue.
  ent->create_tx_queue.queue_resources_addr = GetQueueResourcesPhysAddr(qr_idx);
  ent->create_tx_queue.tx_ring_addr = tx_ring_->phys();
  ent->create_tx_queue.queue_page_list_id = tx_page_list_->id();
  tx_irq_index_ = kTxIrqIndex;
  ent->create_tx_queue.ntfy_id = tx_irq_index_;
  ent->create_tx_queue.tx_comp_ring_addr = 0;  // Not relevant when in GQI format.
  ent->create_tx_queue.tx_ring_size = tx_ring_len_;
  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Create tx queue command status is not 'passed' (0x%x)", ent->status.val());
    return ZX_ERR_INTERNAL;
  }

  return ZX_OK;
}

zx_status_t Gvnic::CreateRXQueue() {
  zx_status_t status;

  GvnicAdminqEntry* ent = NextAdminQEntry();
  ent->opcode = GVNIC_ADMINQ_CREATE_RX_QUEUE;
  ent->status = 0x0;  // Device will set this when the command completes.

  const uint32_t qr_idx = GetNextQueueResourcesIndex();
  rx_queue_resources_ = GetQueueResourcesVirtAddr(qr_idx);

  rx_ring_len_ = dev_descr_.rx_queue_size;
  if (!rx_desc_ring_) {  // From NIC
    status = buffer_factory_->CreateContiguous(bti_, rx_ring_len_ * sizeof(GvnicRxDesc), 0,
                                               dma_buffer::CacheOptions::kEnabled, &rx_desc_ring_);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create rx desc ring: %s", zx_status_get_string(status));
      return status;
    }
  }

  if (!rx_data_ring_) {  // To NIC
    status = buffer_factory_->CreateContiguous(bti_, rx_ring_len_ * sizeof(uint64_t), 0,
                                               dma_buffer::CacheOptions::kEnabled, &rx_data_ring_);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create rx data ring: %s", zx_status_get_string(status));
      return status;
    }
  }

  ent->create_rx_queue.queue_id = 0;  // Only making one RX Queue.
  ent->create_rx_queue.slice = 0;     // Obsolete. Not used.
  rx_irq_index_ = kRxIrqIndex;
  ent->create_rx_queue.ntfy_id = rx_irq_index_;
  ent->create_rx_queue.queue_resources_addr = GetQueueResourcesPhysAddr(qr_idx);
  ent->create_rx_queue.rx_desc_ring_addr = rx_desc_ring_->phys();
  ent->create_rx_queue.rx_data_ring_addr = rx_data_ring_->phys();
  ent->create_rx_queue.queue_page_list_id = rx_page_list_->id();
  ent->create_rx_queue.rx_ring_size = rx_ring_len_;
  ent->create_rx_queue.packet_buffer_size = rounded_mtu_;
  SubmitPendingAdminQEntries(true);
  if (ent->status != GVNIC_ADMINQ_STATUS_PASSED) {
    zxlogf(ERROR, "Create rx queue command status is not 'passed' (0x%x)", ent->status.val());
    return ZX_ERR_INTERNAL;
  }

  return ZX_OK;
}

GvnicAdminqEntry* Gvnic::NextAdminQEntry() {
  ZX_ASSERT_MSG(admin_queue_num_pending_ + admin_queue_num_allocated_ < GVNIC_ADMINQ_LEN,
                "Ran out of admin queue entries. %u pending, %u allocated, %lu available",
                admin_queue_num_pending_, admin_queue_num_allocated_, GVNIC_ADMINQ_LEN);
  admin_queue_num_allocated_++;
  const uint32_t idx = admin_queue_index_++ % GVNIC_ADMINQ_LEN;
  ZX_ASSERT_MSG(admin_queue_, "Admin queue is not allocated.");
  const auto addr = &(static_cast<GvnicAdminqEntry*>(admin_queue_->virt())[idx]);

  // A driver must zero out any unused portion of the 64-byte command structure. Commands which
  // contain non-zero bytes in unused fields will be rejected by the device.
  memset(addr, 0, sizeof(*addr));
  return addr;
}

void Gvnic::SubmitPendingAdminQEntries(bool wait) {
  ZX_ASSERT_MSG(admin_queue_num_allocated_ > 0, "Cannot submit, %u entries allocated",
                admin_queue_num_allocated_);
  std::atomic_thread_fence(std::memory_order_release);
  SET_LOCAL_REG_AND_WRITE_TO_MMIO(admin_queue_doorbell, admin_queue_index_);
  admin_queue_num_pending_ += admin_queue_num_allocated_;
  admin_queue_num_allocated_ = 0;
  if (wait) {
    WaitForAdminQueueCompletion();
  }
}

void Gvnic::WaitForAdminQueueCompletion() {
  ZX_ASSERT_MSG(admin_queue_num_pending_ > 0, "Cannot wait for completion, %u pending.",
                admin_queue_num_pending_);
  // In practice, this "polling" succeeds instantly.
  POLL_AT_MOST_N_TIMES_UNTIL_CONDITION_IS_MET(
      10000,
      READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(admin_queue_counter) ==
          regs_.admin_queue_doorbell,
      "admin queue completion.");
  std::atomic_thread_fence(std::memory_order_acquire);
  admin_queue_num_pending_ = 0;
}

uint32_t Gvnic::GetNextQueueResourcesIndex() {
  zx_status_t status;
  if (!queue_resources_) {
    status = buffer_factory_->CreateContiguous(
        bti_, zx_system_get_page_size(), 0, dma_buffer::CacheOptions::kEnabled, &queue_resources_);
    ZX_ASSERT_MSG(status == ZX_OK, "Couldn't create queue resources: %s",
                  zx_status_get_string(status));
  }
  const auto num_queue_resources = queue_resources_->size() / sizeof(GvnicQueueResources);
  ZX_ASSERT_MSG(next_queue_resources_index_ < num_queue_resources,
                "Out of queue resources next index (%u) is >= num available (%lu)",
                next_queue_resources_index_, num_queue_resources);
  return next_queue_resources_index_++;
}

zx_paddr_t Gvnic::GetQueueResourcesPhysAddr(uint32_t index) {
  ZX_ASSERT_MSG(queue_resources_,
                "Queue resources is not allocated yet. "
                "How did index %u get allocated without calling GetNextQueueResourcesIndex?",
                index);
  const zx_paddr_t base = queue_resources_->phys();
  // zx_paddr_t is ultimately just an unsigned long. This is normal integer arithmetic, not pointer
  // arithmetic, so we need to multiply by the size of the elements.
  const zx_paddr_t addr = base + (sizeof(GvnicQueueResources) * index);
  return addr;
}

GvnicQueueResources* Gvnic::GetQueueResourcesVirtAddr(uint32_t index) {
  ZX_ASSERT_MSG(queue_resources_,
                "Queue resources is not allocated yet. "
                "How did index %u get allocated without calling GetNextQueueResourcesIndex?",
                index);
  const auto base = reinterpret_cast<GvnicQueueResources*>(queue_resources_->virt());
  return &(base[index]);
}

zx_status_t Gvnic::StartRXThread() {
  if (rx_dispatcher_.get()) {
    zxlogf(ERROR, "RX dispatcher already exists");
    return ZX_ERR_ALREADY_BOUND;
  }
  zx::result dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "gvnic-rx-dispatcher",
      [this](fdf_dispatcher_t*) { rx_dispatcher_shutdown_.Signal(); });
  if (dispatcher.is_error()) {
    zxlogf(ERROR, "Failed to create RX dispatcher: %s", dispatcher.status_string());
    return dispatcher.status_value();
  }
  rx_dispatcher_ = std::move(dispatcher.value());

  // By using a synchronized dispatcher that allows sync calls this will behave as a separate thread
  // that will run RXThread forever. The combination of a synchronized dispatcher and allow sync
  // calls ensures that the posted task never gets inlined. It is guaranteed to run on its own
  // thread. It's important to have a dispatcher here instead of a thread for performance reasons.
  // It allows FIDL calls made from RXThread to be inlined, without a dispatcher that can't happen.
  if (zx_status_t status =
          async::PostTask(rx_dispatcher_.async_dispatcher(), [this]() { RXThread(); });
      status != ZX_OK) {
    zxlogf(ERROR, "Could not post rx thread task: %s", zx_status_get_string(status));
    return status;
  }

  return ZX_OK;
}

#define DOORBELL_IDX_TO_OFFSET(i) (sizeof(uint32_t) * i)

void Gvnic::WriteDoorbell(uint32_t index, uint32_t value) {
  const BigEndian<uint32_t> doorbell_val = value;
  std::atomic_thread_fence(std::memory_order_release);
  doorbell_mmio_->Write<uint32_t>(doorbell_val.GetBE(), DOORBELL_IDX_TO_OFFSET(index));
}

uint32_t Gvnic::ReadCounter(uint32_t index) {
  ZX_ASSERT_MSG(counter_page_, "Counter page not allocated.");
  const auto volatile counter_base = reinterpret_cast<BigEndian<uint32_t>*>(counter_page_->virt());
  const uint32_t val = counter_base[index];
  std::atomic_thread_fence(std::memory_order_acquire);
  return val;
}

// TODO(https://fxbug.dev/42059141): Find a clever way to get zerocopy rx to work, and then delete
// this method.
void Gvnic::WritePacketToBufferSpace(const netdev::wire::RxSpaceBuffer& buffer, uint8_t* data,
                                     uint32_t len) {
  network::SharedAutoLock lock(&vmo_lock_);
  vmo_map_.at(buffer.region.vmo).write(data, buffer.region.offset, len);
}

int Gvnic::RXThread() {
  const auto data_ring_base = reinterpret_cast<BigEndian<uint64_t>*>(rx_data_ring_->virt());
  const auto desc_ring_base = reinterpret_cast<GvnicRxDesc*>(rx_desc_ring_->virt());
  const auto pagelist_base = reinterpret_cast<uint8_t*>(rx_page_list_->pages()->virt());

  ZX_ASSERT_MSG(rounded_mtu_ * rx_ring_len_ <= rx_page_list_->pages()->size(),
                "Can't fit %u MTUs (%u bytes each) (= %u bytes) in the page list (%lu bytes)",
                rx_ring_len_, rounded_mtu_, rounded_mtu_ * rx_ring_len_,
                rx_page_list_->pages()->size());
  for (uint32_t i = 0; i < rx_ring_len_; i++) {
    data_ring_base[i] = rounded_mtu_ * i;
  }

  uint32_t ring_doorbell = rx_ring_len_;
  uint32_t ring_counter = 0;
  const uint32_t doorbell_idx = rx_queue_resources_->db_index;
  const uint32_t counter_idx = rx_queue_resources_->counter_index;
  WriteDoorbell(doorbell_idx, ring_doorbell);

  std::vector<netdev::wire::RxBuffer> completed_rx(rx_ring_len_);
  std::vector<netdev::wire::RxBufferPart> completed_rx_parts(rx_ring_len_);

  auto volatile const irq_doorbell_idxs =
      reinterpret_cast<BigEndian<uint32_t>*>(irq_doorbell_idx_page_->virt());
  const uint32_t irq_db_index = irq_doorbell_idxs[rx_irq_index_ * kIrqDoorbellIdxStride];
  for (;;) {
    zx_status_t status = rx_interrupt_.wait(nullptr);
    if (status == ZX_ERR_CANCELED) {
      break;
    }
    ZX_ASSERT_MSG(status == ZX_OK, "Couldn't wait for rx interrupt: %s",
                  zx_status_get_string(status));
    WriteDoorbell(irq_db_index, GVNIC_IRQ_ACK | GVNIC_IRQ_EVENT);

    const uint32_t counter = ReadCounter(counter_idx);
    uint32_t packets_processed = 0;
    uint32_t packets_written = 0;
    for (; ring_counter != counter; ring_counter++) {
      const uint32_t idx = ring_counter % rx_ring_len_;
      const uint16_t flags_seq = desc_ring_base[idx].flags_seq;
      const uint16_t raw_len = desc_ring_base[idx].length;
      packets_processed++;
      if ((flags_seq & (GVNIC_RX_FLAG_ERR | GVNIC_RX_FLAG_CONT)) || raw_len <= GVNIC_RX_PADDING ||
          raw_len > rounded_mtu_) {
        zxlogf(WARNING, "Dropping invalid RX packet (flags_seq=0x%04x, len=%u).", flags_seq,
               raw_len);
        continue;
      }
      const uint32_t len = raw_len - GVNIC_RX_PADDING;
      uint8_t* const data = pagelist_base + (idx * rounded_mtu_) + GVNIC_RX_PADDING;

      std::lock_guard rx_lock(rx_queue_lock_);
      if (!network_device_impl_started_ || rx_space_buffer_queue_.Count() == 0) {
        zxlogf(WARNING, "No buffer space. Dropping packet.");
        continue;
      }
      const netdev::wire::RxSpaceBuffer& buffer = rx_space_buffer_queue_.Front();
      if (len > buffer.region.length) {
        zxlogf(WARNING,
               "Recieved packet (%u bytes) will not fit in the buffer space (%lu bytes). "
               "Dropping packet.",
               len, buffer.region.length);
        continue;
      }
      completed_rx_parts[packets_written] = {.id = buffer.id, .offset = 0, .length = len};
      completed_rx[packets_written] = {
          .meta =
              {
                  .port = kNetworkPortId,
                  .frame_type = fuchsia_hardware_network::wire::FrameType::kEthernet,
              },
          .data = fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(
              &completed_rx_parts[packets_written], 1),
      };
      // TODO(https://fxbug.dev/42059141): Find a clever way to get zerocopy rx to work, instead of
      // calling this.
      WritePacketToBufferSpace(buffer, data, len);
      rx_space_buffer_queue_.Dequeue();
      packets_written++;
    }
    if (packets_processed) {
      ring_doorbell += packets_processed;
      WriteDoorbell(doorbell_idx, ring_doorbell);
    }
    if (packets_written) {
      fdf::Arena arena(0u);
      network::SharedAutoLock lock(&ifc_lock_);
      if (fidl::OneWayStatus status =
              ifc_.buffer(arena)->CompleteRx(fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(
                  completed_rx.data(), packets_written));
          !status.ok()) {
        zxlogf(ERROR, "Failed to complete %u RX buffers: %s", packets_written,
               status.FormatDescription().c_str());
        DdkAsyncRemove();
        break;
      }
    }
  }
  return 0;
}

void Gvnic::DdkInit(ddk::InitTxn txn) { txn.Reply(ZX_OK); }

void Gvnic::DdkUnbind(ddk::UnbindTxn txn) {
  Shutdown();
  txn.Reply();
}

void Gvnic::DdkSuspend(ddk::SuspendTxn txn) {
  Shutdown();
  txn.Reply(ZX_OK, txn.requested_state());
}

void Gvnic::Shutdown() {
  if (rx_interrupt_.is_valid()) {
    rx_interrupt_.destroy();
  }
  if (tx_interrupt_.is_valid()) {
    tx_interrupt_.destroy();
  }
  if (rx_dispatcher_.get()) {
    rx_dispatcher_.ShutdownAsync();
    rx_dispatcher_shutdown_.Wait();
    rx_dispatcher_.reset();
  }
  if (netdevice_dispatcher_.get()) {
    netdevice_dispatcher_.ShutdownAsync();
    netdevice_dispatcher_shutdown_.Wait();
    netdevice_dispatcher_.reset();
  }
  if (reg_mmio_.has_value()) {
    ResetCard(true);
  }
}

void Gvnic::DdkRelease() { delete this; }

// ------- NetworkDeviceImpl -------
// The quotes in the comments in this section come from the documentation of these fields in
// sdk/fidl/fuchsia.hardware.network.driver/network-device.fidl

void Gvnic::Init(netdev::wire::NetworkDeviceImplInitRequest* request, fdf::Arena& arena,
                 InitCompleter::Sync& completer) {
  // "`Init` is only called once during the lifetime of the device..."
  ZX_ASSERT_MSG(!network_device_impl_initialized_, "NetworkDeviceImplInit already called.");
  network_device_impl_initialized_ = true;

  fbl::AutoLock lock(&ifc_lock_);
  ifc_.Bind(std::move(request->iface), netdevice_dispatcher_.get());

  auto [client, server] = fdf::Endpoints<fuchsia_hardware_network_driver::NetworkPort>::Create();
  fdf::BindServer(netdevice_dispatcher_.get(), std::move(server), this);

  ifc_.buffer(arena)
      ->AddPort(kNetworkPortId, std::move(client))
      .Then([completer = completer.ToAsync()](
                fdf::WireUnownedResult<netdev::NetworkDeviceIfc::AddPort>& result) mutable {
        fdf::Arena arena(0u);
        if (!result.ok()) {
          zxlogf(ERROR, "Failed to add port: %s", result.FormatDescription().c_str());
          completer.buffer(arena).Reply(result.status());
          return;
        }
        if (result->status != ZX_OK) {
          zxlogf(ERROR, "Failed to add port: %s", zx_status_get_string(result->status));
          completer.buffer(arena).Reply(result->status);
          return;
        }
        completer.buffer(arena).Reply(ZX_OK);
      });
}

void Gvnic::Start(fdf::Arena& arena, StartCompleter::Sync& completer) {
  {
    std::lock_guard rx_lock(rx_queue_lock_);
    std::lock_guard tx_lock(tx_queue_lock_);
    if (network_device_impl_started_) {
      completer.buffer(arena).Reply(ZX_ERR_ALREADY_BOUND);
      return;
    }
    rx_space_buffer_queue_.Init(rx_ring_len_);
    tx_buffer_id_queue_.Init(tx_ring_len_);
    network_device_impl_started_ = true;
  }
  completer.buffer(arena).Reply(ZX_OK);
}

void Gvnic::AbortPendingTX() {
  std::lock_guard tx_lock(tx_queue_lock_);
  const uint32_t tx_total_count = tx_buffer_id_queue_.Count();
  std::vector<netdev::wire::TxResult> completed_tx;
  completed_tx.reserve(tx_total_count);
  for (uint32_t i = 0; i < tx_total_count; i++) {
    completed_tx.push_back({
        .id = tx_buffer_id_queue_.Front(),
        .status = ZX_ERR_BAD_STATE,
    });
    tx_buffer_id_queue_.Dequeue();
  }
  tx_counter_ = tx_doorbell_;
  if (tx_total_count) {
    fdf::Arena arena(0u);
    network::SharedAutoLock lock(&ifc_lock_);
    if (fidl::OneWayStatus status =
            ifc_.buffer(arena)->CompleteTx(fidl::VectorView<netdev::wire::TxResult>::FromExternal(
                completed_tx.data(), completed_tx.size()));
        !status.ok()) {
      zxlogf(ERROR, "Failed to complete %u TX buffers: %s", tx_total_count,
             status.FormatDescription().c_str());
      // This is a critical error, recover by unbinding to allow the driver to reload if possible.
      DdkAsyncRemove();
      return;
    }
  }
  tx_buffer_id_queue_.Init(0);  // Release the queue.
}

void Gvnic::AbortPendingRX() {
  std::lock_guard rx_lock(rx_queue_lock_);
  const uint32_t rx_total_count = rx_space_buffer_queue_.Count();
  std::vector<netdev::wire::RxBuffer> completed_rx(rx_total_count);
  std::vector<netdev::wire::RxBufferPart> completed_rx_parts(rx_total_count);
  for (uint32_t i = 0; i < rx_total_count; i++) {
    completed_rx_parts[i] = {.id = rx_space_buffer_queue_.Front().id, .offset = 0, .length = 0};
    completed_rx[i] = {
        .meta =
            {
                .port = kNetworkPortId,
                .frame_type = fuchsia_hardware_network::wire::FrameType::kEthernet,
            },
        .data =
            fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(&completed_rx_parts[i], 1),
    };
    rx_space_buffer_queue_.Dequeue();
  }
  if (rx_total_count) {
    fdf::Arena arena(0u);
    network::SharedAutoLock lock(&ifc_lock_);
    if (fidl::OneWayStatus status =
            ifc_.buffer(arena)->CompleteRx(fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(
                completed_rx.data(), rx_total_count));
        !status.ok()) {
      zxlogf(ERROR, "Failed to complete %u rx buffers: %s", rx_total_count,
             status.FormatDescription().c_str());
      // This is a critical error, recover by unbinding to allow the driver to reload if possible.
      DdkAsyncRemove();
      return;
    }
  }
  rx_space_buffer_queue_.Init(0);  // Release the queue.
}

void Gvnic::Stop(fdf::Arena& arena, StopCompleter::Sync& completer) {
  {
    std::lock_guard rx_lock(rx_queue_lock_);
    std::lock_guard tx_lock(tx_queue_lock_);
    ZX_ASSERT_MSG(network_device_impl_started_, "NetworkDeviceImpl not Started.");
    network_device_impl_started_ = false;
  }

  AbortPendingTX();
  AbortPendingRX();
  completer.buffer(arena).Reply();
}

void Gvnic::GetInfo(fdf::Arena& arena,
                    fdf::WireServer<netdev::NetworkDeviceImpl>::GetInfoCompleter::Sync& completer) {
  netdev::wire::DeviceImplInfo info =
      netdev::wire::DeviceImplInfo::Builder(arena)
          .tx_depth(tx_ring_len_)
          .rx_depth(rx_ring_len_)
          // "The typical choice of value is `rx_depth / 2`."
          .rx_threshold(rx_ring_len_ / 2)
          // "Devices that can't perform scatter-gather operations must set `max_buffer_parts`
          // to 1."
          .max_buffer_parts(1)
          // "Devices that do not support scatter-gather DMA may set this to a value smaller than a
          // page size to guarantee compatibility."
          .max_buffer_length(rounded_mtu_)
          // Don't care because we are not using these buggers for dma, but "Must be greater than
          // zero".
          .buffer_alignment(1)
          .min_rx_buffer_length(rounded_mtu_)
          .Build();
  completer.buffer(arena).Reply(info);
}

// TODO(https://fxbug.dev/42059141): Find a clever way to get zerocopy tx to work, and then delete
// this method.
void Gvnic::WriteBufferToCard(const netdev::wire::TxBuffer& buffer, uint8_t* data) {
  ZX_ASSERT_MSG(buffer.data.size() == 1, "Data count (%lu), should be 1.", buffer.data.size());
  ZX_ASSERT_MSG(buffer.head_length == 0, "Head length (%u) should be 0.", buffer.head_length);
  ZX_ASSERT_MSG(buffer.tail_length == 0, "Tail length (%u) should be 0.", buffer.tail_length);
  network::SharedAutoLock lock(&vmo_lock_);
  const auto len = static_cast<uint32_t>(buffer.data[0].length);
  ZX_ASSERT_MSG(len == buffer.data[0].length, "Length did not fit in uint32_t");
  ZX_ASSERT_MSG(len <= rounded_mtu_, "Length (%u) exceeds rounded MTU (%u)", len, rounded_mtu_);
  vmo_map_.at(buffer.data[0].vmo).read(data, buffer.data[0].offset, len);
}

void Gvnic::SendTXBuffers(const netdev::wire::TxBuffer* buf_list, size_t buf_count) {
  auto ring_base = reinterpret_cast<GvnicTxPktDesc*>(tx_ring_->virt());
  auto pagelist_base = reinterpret_cast<uint8_t*>(tx_page_list_->pages()->virt());
  for (uint32_t i = 0; i < buf_count; i++) {
    ZX_ASSERT_MSG(buf_list[i].data.size() == 1, "Data count (%lu), should be 1.",
                  buf_list[i].data.size());
    ZX_ASSERT_MSG(buf_list[i].head_length == 0, "Head length (%u) should be 0.",
                  buf_list[i].head_length);
    ZX_ASSERT_MSG(buf_list[i].tail_length == 0, "Tail length (%u) should be 0.",
                  buf_list[i].tail_length);
    uint32_t offset = tx_ring_index_ * rounded_mtu_;
    // TODO(https://fxbug.dev/42059141): Find a clever way to get zerocopy tx to work, instead of
    // calling this.
    WriteBufferToCard(buf_list[i], pagelist_base + offset);

    const auto full_len = buf_list[i].data[0].length;
    const uint16_t len = static_cast<uint16_t>(full_len);
    ZX_ASSERT_MSG(len == full_len, "len did not fit in uint16_t");

    ring_base[tx_ring_index_] = {
        .type_flags = GVNIC_TXD_STD,
        .checksum_offset = 0,  // "If checksum offload is not requested, the field is Reserved to 0"
        .l4_offset = 0,        // "If checksum offload is not requested, the field is Reserved to 0"
        .descriptor_count = 1,
        .len = len,
        .seg_len = len,
        .seg_addr = offset,
    };
    tx_ring_index_ = (tx_ring_index_ + 1) % tx_ring_len_;
  }
  tx_doorbell_ += buf_count;
  WriteDoorbell(tx_queue_resources_->db_index, tx_doorbell_);
}

void Gvnic::EnqueueTXBuffers(const netdev::wire::TxBuffer* buf_list, size_t buf_count) {
  for (uint32_t i = 0; i < buf_count; i++) {
    ZX_ASSERT_MSG(buf_list[i].data.size() == 1, "Buffer %u has count %lu (max is 1)", i,
                  buf_list[i].data.size());
    tx_buffer_id_queue_.Enqueue(buf_list[i].id);
  }
}

void Gvnic::FlushTXBuffers() {
  auto get_queue_count = [this] {
    std::lock_guard tx_lock(tx_queue_lock_);
    return tx_buffer_id_queue_.Count();
  };
  const auto initial_queue_count = get_queue_count();

  fdf::Arena arena(0u);
  do {
    std::vector<netdev::wire::TxResult> completed_tx;
    {
      std::lock_guard tx_lock(tx_queue_lock_);
      if (!network_device_impl_started_) {
        return;
      }
      const uint32_t counter = ReadCounter(tx_queue_resources_->counter_index);
      const uint32_t count = counter - tx_counter_;
      if (count == 0) {
        // Nothing to do.
        continue;
      }
      tx_counter_ = counter;

      completed_tx.resize(count);
      for (uint32_t i = 0; i < count; i++) {
        completed_tx[i] = {
            .id = tx_buffer_id_queue_.Front(),
            .status = ZX_OK,
        };
        tx_buffer_id_queue_.Dequeue();
      }
    }
    network::SharedAutoLock lock(&ifc_lock_);
    if (fidl::OneWayStatus status =
            ifc_.buffer(arena)->CompleteTx(fidl::VectorView<netdev::wire::TxResult>::FromExternal(
                completed_tx.data(), completed_tx.size()));
        !status.ok()) {
      zxlogf(ERROR, "Failed to complete %zu tx buffers: %s", completed_tx.size(),
             status.FormatDescription().c_str());
      // This is a critical error, recover by unbinding to allow the driver to reload if possible.
      DdkAsyncRemove();
      return;
    }
    // Don't give up until at least half the ring is available.
  } while (unlikely(initial_queue_count >= (tx_ring_len_ / 2) &&
                    get_queue_count() >= (tx_ring_len_ / 2)));
}

void Gvnic::QueueTx(netdev::wire::NetworkDeviceImplQueueTxRequest* request, fdf::Arena& arena,
                    QueueTxCompleter::Sync& completer) {
  {
    std::lock_guard tx_lock(tx_queue_lock_);
    if (!network_device_impl_started_) {
      std::vector<netdev::wire::TxResult> completed_tx(request->buffers.size());
      for (size_t i = 0; i < request->buffers.size(); i++) {
        completed_tx[i] = {
            .id = request->buffers[i].id,
            .status = ZX_ERR_UNAVAILABLE,
        };
      }
      if (!completed_tx.empty()) {
        network::SharedAutoLock lock(&ifc_lock_);
        [[maybe_unused]] fidl::OneWayStatus status =
            ifc_.buffer(arena)->CompleteTx(fidl::VectorView<netdev::wire::TxResult>::FromExternal(
                completed_tx.data(), completed_tx.size()));
      }
      return;
    }
    EnqueueTXBuffers(request->buffers.data(), request->buffers.size());
    SendTXBuffers(request->buffers.data(), request->buffers.size());
  }
  FlushTXBuffers();
}

void Gvnic::QueueRxSpace(netdev::wire::NetworkDeviceImplQueueRxSpaceRequest* request,
                         fdf::Arena& arena, QueueRxSpaceCompleter::Sync& completer) {
  std::lock_guard rx_lock(rx_queue_lock_);
  if (!network_device_impl_started_) {
    const size_t count = request->buffers.size();
    std::vector<netdev::wire::RxBuffer> completed_rx(count);
    std::vector<netdev::wire::RxBufferPart> completed_rx_parts(count);
    for (size_t i = 0; i < count; i++) {
      completed_rx_parts[i] = {.id = request->buffers[i].id, .offset = 0, .length = 0};
      completed_rx[i] = {
          .meta =
              {
                  .port = kNetworkPortId,
                  .frame_type = fuchsia_hardware_network::wire::FrameType::kEthernet,
              },
          .data =
              fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(&completed_rx_parts[i], 1),
      };
    }
    if (count > 0) {
      network::SharedAutoLock lock(&ifc_lock_);
      [[maybe_unused]] fidl::OneWayStatus status = ifc_.buffer(arena)->CompleteRx(
          fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(completed_rx.data(), count));
    }
    return;
  }
  for (const auto& buffer : request->buffers) {
    rx_space_buffer_queue_.Enqueue(buffer);
  }
}

void Gvnic::PrepareVmo(netdev::wire::NetworkDeviceImplPrepareVmoRequest* request, fdf::Arena& arena,
                       PrepareVmoCompleter::Sync& completer) {
  zx_status_t status = ZX_OK;
  {
    fbl::AutoLock lock(&vmo_lock_);
    if (!vmo_map_.insert({request->id, std::move(request->vmo)}).second) {
      status = ZX_ERR_INTERNAL;
    }
  }
  completer.buffer(arena).Reply(status);
}

void Gvnic::ReleaseVmo(netdev::wire::NetworkDeviceImplReleaseVmoRequest* request, fdf::Arena& arena,
                       ReleaseVmoCompleter::Sync& completer) {
  {
    fbl::AutoLock lock(&vmo_lock_);
    const auto num_erased = vmo_map_.erase(request->id);
    ZX_ASSERT_MSG(num_erased == 1, "Expected to erase one vmo, instead erased %lu.", num_erased);
  }
  completer.buffer(arena).Reply();
}

// ------- NetworkPort -------

void Gvnic::GetInfo(fdf::Arena& arena,
                    fdf::WireServer<netdev::NetworkPort>::GetInfoCompleter::Sync& completer) {
  static constexpr fuchsia_hardware_network::wire::FrameType kRxTypes[] = {
      fuchsia_hardware_network::wire::FrameType::kEthernet};
  static constexpr fuchsia_hardware_network::wire::FrameTypeSupport kTxTypes[] = {
      {.type = fuchsia_hardware_network::wire::FrameType::kEthernet,
       .features = fuchsia_hardware_network::wire::kFrameFeaturesRaw}};

  fuchsia_hardware_network::wire::PortBaseInfo info =
      fuchsia_hardware_network::wire::PortBaseInfo::Builder(arena)
          .port_class(fuchsia_hardware_network::wire::PortClass::kEthernet)
          .rx_types(kRxTypes)
          .tx_types(kTxTypes)
          .Build();
  completer.buffer(arena).Reply(info);
}

void Gvnic::GetStatus(fdf::Arena& arena, GetStatusCompleter::Sync& completer) {
  fuchsia_hardware_network::wire::StatusFlags flags;
  if (READ_REG_FROM_MMIO_TO_LOCAL_AND_RETURN_VALUE(dev_status) & GVNIC_DEVICE_STATUS_LINK_STATUS) {
    flags = fuchsia_hardware_network::wire::StatusFlags::kOnline;
  }
  fuchsia_hardware_network::wire::PortStatus status =
      fuchsia_hardware_network::wire::PortStatus::Builder(arena)
          .mtu(dev_descr_.mtu)
          .flags(flags)
          .Build();
  completer.buffer(arena).Reply(status);
}

void Gvnic::SetActive(fuchsia_hardware_network_driver::wire::NetworkPortSetActiveRequest* request,
                      fdf::Arena& arena, SetActiveCompleter::Sync& completer) {
  // Nohing to do.
}

void Gvnic::GetMac(fdf::Arena& arena, GetMacCompleter::Sync& completer) {
  auto [client, server] = fdf::Endpoints<fuchsia_hardware_network_driver::MacAddr>::Create();
  fdf::BindServer(netdevice_dispatcher_.get(), std::move(server), this);
  completer.buffer(arena).Reply(std::move(client));
}

void Gvnic::Removed(fdf::Arena& arena, RemovedCompleter::Sync& completer) {
  // The port is never removed, there's no extra cleanup needed here.
}

void Gvnic::GetAddress(fdf::Arena& arena, GetAddressCompleter::Sync& completer) {
  fuchsia_net::wire::MacAddress mac;
  memcpy(mac.octets.data(), dev_descr_.mac, ETH_ALEN);
  completer.buffer(arena).Reply(mac);
}

void Gvnic::GetFeatures(fdf::Arena& arena, GetFeaturesCompleter::Sync& completer) {
  netdev::wire::Features features =
      netdev::wire::Features::Builder(arena)
          .supported_modes(netdev::wire::SupportedMacFilterMode::kPromiscuous)
          // "Implementations must set 0 if multicast filtering is not supported."
          .multicast_filter_count(0)
          .Build();
  completer.buffer(arena).Reply(features);
}

void Gvnic::SetMode(fuchsia_hardware_network_driver::wire::MacAddrSetModeRequest* request,
                    fdf::Arena& arena, SetModeCompleter::Sync& completer) {
  ZX_ASSERT_MSG(request->mode == fuchsia_hardware_network::wire::MacFilterMode::kPromiscuous,
                "unsupported mode %u", static_cast<uint32_t>(request->mode));
  ZX_ASSERT_MSG(request->multicast_macs.size() == 0, "unsupported multicast count %zu",
                request->multicast_macs.size());
  completer.buffer(arena).Reply();
}

static zx_driver_ops_t gvnic_driver_ops = []() -> zx_driver_ops_t {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = Gvnic::Bind;
  return ops;
}();

}  // namespace gvnic

ZIRCON_DRIVER(Gvnic, gvnic::gvnic_driver_ops, "zircon", GVNIC_VERSION);
