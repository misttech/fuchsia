// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "uart16550.h"

#include <fidl/fuchsia.hardware.serialimpl/cpp/driver/wire.h>
#include <lib/ddk/binding_driver.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/metadata.h>
#include <lib/uart/ns8250.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <algorithm>

// The register types and constants are defined in the uart library.
using namespace uart::ns8250;

namespace uart16550 {

static constexpr int64_t kPioIndex = 0;
static constexpr int64_t kIrqIndex = 0;

static constexpr uint32_t kDefaultConfig = fuchsia_hardware_serialimpl::wire::kSerialDataBits8 |
                                           fuchsia_hardware_serialimpl::wire::kSerialStopBits1 |
                                           fuchsia_hardware_serialimpl::wire::kSerialParityNone;

static constexpr fuchsia_hardware_serial::wire::SerialPortInfo kInfo = {
    .serial_class = fuchsia_hardware_serial::Class::kGeneric,
    .serial_vid = 0,
    .serial_pid = 0,
};

Uart16550::Uart16550()
    : DeviceType(nullptr),
      acpi_fidl_(acpi::Client::Create(fidl::WireSyncClient<fuchsia_hardware_acpi::Device>())) {}

Uart16550::Uart16550(zx_device_t* parent, acpi::Client acpi)
    : DeviceType(parent), acpi_fidl_(std::move(acpi)) {}

zx_status_t Uart16550::Create(void* /*ctx*/, zx_device_t* parent) {
  auto acpi = acpi::Client::Create(parent);
  if (acpi.is_error()) {
    return acpi.status_value();
  }
  auto dev = std::make_unique<Uart16550>(parent, std::move(acpi.value()));

  auto status = dev->Init();
  if (status != ZX_OK) {
    zxlogf(DEBUG, "%s: Init failed", __func__);
    return status;
  }

  {
    fuchsia_hardware_serialimpl::Service::InstanceHandler handler({.device = dev->GetHandler()});
    auto result =
        dev->outgoing_.AddService<fuchsia_hardware_serialimpl::Service>(std::move(handler));
    if (result.is_error()) {
      zxlogf(ERROR, "AddService failed: %s", result.status_string());
      return result.error_value();
    }
  }

  auto [directory_client, directory_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();

  {
    auto result = dev->outgoing_.Serve(std::move(directory_server));
    if (result.is_error()) {
      zxlogf(ERROR, "Failed to serve the outgoing directory: %s", result.status_string());
      return result.error_value();
    }
  }

  std::array<const char*, 1> fidl_service_offers{fuchsia_hardware_serialimpl::Service::Name};
  dev->DdkAdd(ddk::DeviceAddArgs("uart16550")
                  .set_outgoing_dir(directory_client.TakeChannel())
                  .set_runtime_service_offers(fidl_service_offers));

  // Release because devmgr is now in charge of the device.
  static_cast<void>(dev.release());
  return ZX_OK;
}

size_t Uart16550::FifoDepth() const { return uart_fifo_len_; }

bool Uart16550::Enabled() {
  std::lock_guard<std::mutex> lock(device_mutex_);
  return enabled_;
}

// Create RX and TX FIFOs, obtain interrupt and port handles from the ACPI
// device, obtain port permissions, set up default configuration, and start the
// interrupt handler thread.
zx_status_t Uart16550::Init() {
  zx::resource io_port;
  auto pio = acpi_fidl_.borrow()->GetPio(kPioIndex);
  if (!pio.ok() || pio->is_error()) {
    zxlogf(DEBUG, "%s: acpi_.GetPio failed", __func__);
    return pio.ok() ? pio->error_value() : pio.status();
  }
  io_port.reset(pio->value()->pio.release());

  auto irq = acpi_fidl_.borrow()->MapInterrupt(kIrqIndex);
  if (!irq.ok() || irq->is_error()) {
    zxlogf(ERROR, "%s: acpi_.MapInterrupt failed", __func__);
    return irq.ok() ? irq->error_value() : irq.status();
  }
  interrupt_.reset(irq->value()->irq.release());

  zx_info_resource_t resource_info;
  zx_status_t status =
      io_port.get_info(ZX_INFO_RESOURCE, &resource_info, sizeof(resource_info), nullptr, nullptr);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: io_port.get_info failed", __func__);
    return status;
  }

  const auto port_base = static_cast<uint16_t>(resource_info.base);
  const auto port_size = static_cast<uint32_t>(resource_info.size);

  if (port_base != resource_info.base) {
    zxlogf(ERROR, "%s: overflowing UART port base", __func__);
    return ZX_ERR_BAD_STATE;
  }

  if (port_size != resource_info.size) {
    zxlogf(ERROR, "%s: overflowing UART port size", __func__);
    return ZX_ERR_BAD_STATE;
  }

  if (port_size != uart::ns8250::kIoSlots<ZBI_KERNEL_DRIVER_I8250_PIO_UART>) {
    zxlogf(ERROR, "%s: unsupported UART port count", __func__);
    return ZX_ERR_NOT_SUPPORTED;
  }

  status = zx_ioports_request(io_port.get(), port_base, port_size);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: zx_ioports_request failed", __func__);
    return status;
  }

  {
    std::lock_guard<std::mutex> lock(device_mutex_);
#ifdef __x86_64__
    port_io_.emplace<hwreg::RegisterPio>(port_base);
#else
    ZX_PANIC("uart16550 driver supports only direct PIO, which is x86-only");
#endif
    InitFifosLocked();
  }

  status = Config(kMaxBaudRate, kDefaultConfig);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: SerialImplConfig failed", __func__);
    return status;
  }

  interrupt_thread_ = std::thread([&] { HandleInterrupts(); });

  return ZX_OK;
}

#if UART16550_TESTING
zx_status_t Uart16550::Init(zx::interrupt interrupt, hwreg::Mock::RegisterIo port_mock) {
  interrupt_ = std::move(interrupt);
  {
    std::lock_guard<std::mutex> lock(device_mutex_);
    port_io_.emplace<hwreg::Mock::RegisterIo>(port_mock);
    InitFifosLocked();
  }

  auto status = Config(kMaxBaudRate, kDefaultConfig);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: SerialImplConfig failed", __func__);
    return status;
  }

  interrupt_thread_ = std::thread([&] { HandleInterrupts(); });

  return ZX_OK;
}
#endif  // UART16550_TESTING

zx::unowned_interrupt Uart16550::InterruptHandle() { return zx::unowned_interrupt(interrupt_); }

void Uart16550::GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) {
  completer.buffer(arena).ReplySuccess(kInfo);
}

void Uart16550::Config(fuchsia_hardware_serialimpl::wire::DeviceConfigRequest* request,
                       fdf::Arena& arena, ConfigCompleter::Sync& completer) {
  completer.buffer(arena).Reply(zx::make_result(Config(request->baud_rate, request->flags)));
}

zx_status_t Uart16550::Config(uint32_t baud_rate, uint32_t flags) {
  if (Enabled()) {
    zxlogf(ERROR, "%s: attempted to configure when enabled", __func__);
    return ZX_ERR_BAD_STATE;
  }

  if (baud_rate == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  const auto divisor = static_cast<uint16_t>(kMaxBaudRate / baud_rate);
  if (divisor != kMaxBaudRate / baud_rate || divisor == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if ((flags & fuchsia_hardware_serialimpl::wire::kSerialFlowCtrlMask) !=
          fuchsia_hardware_serialimpl::wire::kSerialFlowCtrlNone &&
      !SupportsAutomaticFlowControl()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  const auto lower = static_cast<uint8_t>(divisor);
  const auto upper = static_cast<uint8_t>(divisor >> 8);

  std::lock_guard<std::mutex> lock(device_mutex_);

  auto lcr = LineControlRegister::Get().ReadFrom(&port_io_);

  lcr.set_divisor_latch_access(true).WriteTo(&port_io_);

  DivisorLatchLowerRegister::Get().FromValue(0).set_data(lower).WriteTo(&port_io_);
  DivisorLatchUpperRegister::Get().FromValue(0).set_data(upper).WriteTo(&port_io_);

  lcr.set_divisor_latch_access(false);

  if (flags & fuchsia_hardware_serialimpl::wire::kSerialSetBaudRateOnly) {
    lcr.WriteTo(&port_io_);
    return ZX_OK;
  }

  switch (flags & fuchsia_hardware_serialimpl::wire::kSerialDataBitsMask) {
    case fuchsia_hardware_serialimpl::wire::kSerialDataBits5:
      lcr.set_word_length(LineControlRegister::kWordLength5);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialDataBits6:
      lcr.set_word_length(LineControlRegister::kWordLength6);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialDataBits7:
      lcr.set_word_length(LineControlRegister::kWordLength7);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialDataBits8:
      lcr.set_word_length(LineControlRegister::kWordLength8);
      break;
  }

  switch (flags & fuchsia_hardware_serialimpl::wire::kSerialStopBitsMask) {
    case fuchsia_hardware_serialimpl::wire::kSerialStopBits1:
      lcr.set_stop_bits(LineControlRegister::kStopBits1);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialStopBits2:
      lcr.set_stop_bits(LineControlRegister::kStopBits2);
      break;
  }

  switch (flags & fuchsia_hardware_serialimpl::wire::kSerialParityMask) {
    case fuchsia_hardware_serialimpl::wire::kSerialParityNone:
      lcr.set_parity_enable(false);
      lcr.set_even_parity(false);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialParityOdd:
      lcr.set_parity_enable(true);
      lcr.set_even_parity(false);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialParityEven:
      lcr.set_parity_enable(true);
      lcr.set_even_parity(true);
      break;
  }

  lcr.WriteTo(&port_io_);

  auto mcr = ModemControlRegister::Get().FromValue(0);

  // The below is necessary for interrupts on some devices.
  mcr.set_auxiliary_out_2(true);

  switch (flags & fuchsia_hardware_serialimpl::wire::kSerialFlowCtrlMask) {
    case fuchsia_hardware_serialimpl::wire::kSerialFlowCtrlNone:
      mcr.set_automatic_flow_control_enable(false);
      mcr.set_data_terminal_ready(true);
      mcr.set_request_to_send(true);
      break;
    case fuchsia_hardware_serialimpl::wire::kSerialFlowCtrlCtsRts:
      mcr.set_automatic_flow_control_enable(true);
      mcr.set_data_terminal_ready(false);
      mcr.set_request_to_send(false);
      break;
  }

  mcr.WriteTo(&port_io_);

  return ZX_OK;
}

void Uart16550::Enable(fuchsia_hardware_serialimpl::wire::DeviceEnableRequest* request,
                       fdf::Arena& arena, EnableCompleter::Sync& completer) {
  completer.buffer(arena).Reply(zx::make_result(Enable(request->enable)));
}

zx_status_t Uart16550::Enable(bool enable) {
  std::lock_guard<std::mutex> lock(device_mutex_);
  if (enabled_) {
    if (!enable) {
      if (read_completer_ || write_context_) {
        zxlogf(ERROR, "Attempted to disable with a pending read or write request");
        return ZX_ERR_BAD_STATE;
      }

      // The device is enabled, and will be disabled.
      InterruptEnableRegister::Get()
          .FromValue(0)
          .set_rx_available(false)
          .set_line_status(false)
          .set_modem_status(false)
          .set_tx_empty(false)
          .WriteTo(&port_io_);
    }
  } else {
    if (enable) {
      // The device is disabled, and will be enabled.
      ResetFifosLocked();
      InterruptEnableRegister::Get()
          .FromValue(0)
          .set_rx_available(true)
          .set_line_status(true)
          .set_modem_status(true)
          .set_tx_empty(false)
          .WriteTo(&port_io_);
    }
  }
  enabled_ = enable;
  return ZX_OK;
}

size_t Uart16550::DrainRxFifo(cpp20::span<uint8_t> buffer) {
  size_t actual = 0;
  auto lcr = LineStatusRegister::Get().ReadFrom(&port_io_);
  auto rbr = RxBufferRegister::Get();
  for (; lcr.data_ready() && actual < buffer.size(); lcr.ReadFrom(&port_io_), actual++) {
    buffer[actual] = rbr.ReadFrom(&port_io_).data();
  }

  return actual;
}

void Uart16550::Read(fdf::Arena& arena, ReadCompleter::Sync& completer) {
  std::lock_guard<std::mutex> lock(device_mutex_);

  if (!enabled_) {
    zxlogf(ERROR, "%s: attempted to read when disabled", __func__);
    return completer.buffer(arena).ReplyError(ZX_ERR_BAD_STATE);
  }
  if (read_completer_) {
    // Per the serialimpl protocol, ZX_ERR_ALREADY_BOUND should be returned if the client makes a
    // read request when one was already in progress.
    return completer.buffer(arena).ReplyError(ZX_ERR_ALREADY_BOUND);
  }

  auto lcr = LineStatusRegister::Get().ReadFrom(&port_io_);
  if (!lcr.data_ready()) {
    // The RX FIFO is empty, store the completer until we get some bytes to return.
    read_completer_.emplace(completer.ToAsync());
    return;
  }

  // This was the maximum size for reads from the serial core driver at the time of our conversion
  // from Banjo to FIDL.
  uint8_t buf[fuchsia_io::wire::kMaxBuf];
  size_t actual = DrainRxFifo({buf, std::size(buf)});
  completer.buffer(arena).ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(buf, actual));
}

cpp20::span<const uint8_t> Uart16550::FillTxFifo(cpp20::span<const uint8_t> data) {
  auto tbr = TxBufferRegister::Get();
  const size_t writable = std::min(data.size(), uart_fifo_len_);
  for (size_t i = 0; i < writable; i++) {
    tbr.FromValue(0).set_data(data[i]).WriteTo(&port_io_);
  }

  return data.subspan(writable);
}

void Uart16550::Write(fuchsia_hardware_serialimpl::wire::DeviceWriteRequest* request,
                      fdf::Arena& arena, WriteCompleter::Sync& completer) {
  std::lock_guard<std::mutex> lock(device_mutex_);
  if (!enabled_) {
    zxlogf(ERROR, "%s: attempted to write when disabled", __func__);
    return completer.buffer(arena).ReplyError(ZX_ERR_BAD_STATE);
  }
  if (write_context_) {
    // Per the serialimpl protocol, ZX_ERR_ALREADY_BOUND should be returned if the client makes a
    // write request when one was already in progress.
    return completer.buffer(arena).ReplyError(ZX_ERR_ALREADY_BOUND);
  }

  cpp20::span<const uint8_t> remaining = request->data.get();
  if (remaining.empty()) {
    return completer.buffer(arena).ReplySuccess();
  }

  InterruptEnableRegister::Get().ReadFrom(&port_io_).set_tx_empty(true).WriteTo(&port_io_);

  if (LineStatusRegister::Get().ReadFrom(&port_io_).tx_empty()) {
    remaining = FillTxFifo(remaining);
  }

  // Copy the remaining write data to the vector, resizing if necessary.
  write_buffer_.clear();
  write_buffer_.insert(write_buffer_.begin(), remaining.begin(), remaining.end());

  write_context_.emplace(completer.ToAsync(), write_buffer_);
}

void Uart16550::CancelAll() {
  std::lock_guard<std::mutex> lock(device_mutex_);

  fdf::Arena arena('UART');

  if (read_completer_) {
    read_completer_->buffer(arena).ReplyError(ZX_ERR_CANCELED);
    read_completer_.reset();
  }

  if (write_context_) {
    write_context_->completer.buffer(arena).ReplyError(ZX_ERR_CANCELED);
    write_context_.reset();
    InterruptEnableRegister::Get().ReadFrom(&port_io_).set_tx_empty(false).WriteTo(&port_io_);
  }
}

void Uart16550::CancelAll(fdf::Arena& arena, CancelAllCompleter::Sync& completer) {
  CancelAll();
  completer.buffer(arena).Reply();
}

void Uart16550::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_serialimpl::Device> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  zxlogf(ERROR, "Unknown method ordinal %lu", metadata.method_ordinal);
}

void Uart16550::DdkRelease() {
  CancelAll();
  Enable(false);
  // End the interrupt loop by canceling waits.
  interrupt_.destroy();
  interrupt_thread_.join();
  delete this;
}

fidl::ProtocolHandler<fuchsia_hardware_serialimpl::Device> Uart16550::GetHandler() {
  return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->get(),
                                 fidl::kIgnoreBindingClosure);
}

bool Uart16550::SupportsAutomaticFlowControl() const { return uart_fifo_len_ == kFifoDepth16750; }

void Uart16550::ResetFifosLocked() {
  // 16750 requires we toggle extended fifo while divisor latch is enabled.
  LineControlRegister::Get().FromValue(0).set_divisor_latch_access(true).WriteTo(&port_io_);
  FifoControlRegister::Get()
      .FromValue(0)
      .set_fifo_enable(true)
      .set_rx_fifo_reset(true)
      .set_tx_fifo_reset(true)
      .set_dma_mode(0)
      .set_extended_fifo_enable(true)
      .set_receiver_trigger(FifoControlRegister::kMaxTriggerLevel)
      .WriteTo(&port_io_);
  LineControlRegister::Get().FromValue(0).set_divisor_latch_access(false).WriteTo(&port_io_);
}

void Uart16550::InitFifosLocked() {
  ResetFifosLocked();
  const auto iir = InterruptIdentRegister::Get().ReadFrom(&port_io_);
  if (iir.fifos_enabled()) {
    if (iir.extended_fifo_enabled()) {
      uart_fifo_len_ = kFifoDepth16750;
    } else {
      uart_fifo_len_ = kFifoDepth16550A;
    }
  } else {
    uart_fifo_len_ = kFifoDepthGeneric;
  }
}

// Loop and wait on the interrupt handle. When an interrupt is detected, read the interrupt
// identifier. If there is data available in the hardware RX FIFO, notify readable. If the
// hardware TX FIFO is empty, notify writable. If there is a line status error, log it. If
// there is a modem status, log it.
void Uart16550::HandleInterrupts() {
  // Ignore the timestamp.
  while (interrupt_.wait(nullptr) == ZX_OK) {
    std::lock_guard<std::mutex> lock(device_mutex_);

    if (!enabled_) {
      // Interrupts should be disabled now and we shouldn't respond to them.
      continue;
    }

    fdf::Arena arena('UART');

    const auto identifier = InterruptIdentRegister::Get().ReadFrom(&port_io_).interrupt_id();

    switch (identifier) {
      case InterruptType::kNone:
        break;
      case InterruptType::kRxLineStatus: {
        // Clear the interrupt.
        const auto lsr = LineStatusRegister::Get().ReadFrom(&port_io_);
        if (lsr.overrun_error()) {
          zxlogf(ERROR, "%s: overrun error (OE) detected", __func__);
        }
        if (lsr.parity_error()) {
          zxlogf(ERROR, "%s: parity error (PE) detected", __func__);
        }
        if (lsr.framing_error()) {
          zxlogf(ERROR, "%s: framing error (FE) detected", __func__);
        }
        if (lsr.break_interrupt()) {
          zxlogf(ERROR, "%s: break interrupt (BI) detected", __func__);
        }
        if (lsr.error_in_rx_fifo()) {
          zxlogf(ERROR, "%s: error in rx fifo detected", __func__);
        }
        break;
      }
      case InterruptType::kRxDataAvailable:  // In both cases, there is data ready in the rx fifo.
      case InterruptType::kCharTimeout:
        if (read_completer_) {
          uint8_t buf[fuchsia_io::wire::kMaxBuf];
          size_t actual = DrainRxFifo({buf, std::size(buf)});
          read_completer_->buffer(arena).ReplySuccess(
              fidl::VectorView<uint8_t>::FromExternal(buf, actual));
          read_completer_.reset();
        }
        break;
      case InterruptType::kTxEmpty: {
        if (write_context_) {
          if (write_context_->data.empty()) {
            // No more data to write, complete the request and disable the TX empty interrupt.
            write_context_->completer.buffer(arena).ReplySuccess();
            write_context_.reset();
          } else {
            write_context_->data = FillTxFifo(write_context_->data);
            // There is still data that needs to be written -- break early to keep the TX empty
            // interrupt enabled.
            break;
          }
        }

        InterruptEnableRegister::Get().ReadFrom(&port_io_).set_tx_empty(false).WriteTo(&port_io_);
        break;
      }
      case InterruptType::kModemStatus: {
        // Clear the interrupt.
        const auto msr = ModemStatusRegister::Get().ReadFrom(&port_io_);
        if (msr.clear_to_send()) {
          zxlogf(INFO, "%s: clear to send (CTS) detected", __func__);
        }
        if (msr.data_set_ready()) {
          zxlogf(INFO, "%s: data set ready (DSR) detected", __func__);
        }
        if (msr.ring_indicator()) {
          zxlogf(INFO, "%s: ring indicator (RI) detected", __func__);
        }
        if (msr.data_carrier_detect()) {
          zxlogf(INFO, "%s: data carrier (DCD) detected", __func__);
        }
        break;
      }
      case InterruptType::kDw8250BusyDetect: {
        // dw8250 only, supposed to read from a USR register which is not present in 16550
        break;
      }
    }
  }
}

static constexpr zx_driver_ops_t driver_ops = [] {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = Uart16550::Create;
  return ops;
}();

}  // namespace uart16550

// clang-format off
ZIRCON_DRIVER(uart16550, uart16550::driver_ops, "zircon", "0.1");
