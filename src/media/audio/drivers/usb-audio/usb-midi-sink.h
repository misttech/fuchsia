// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_DRIVERS_USB_AUDIO_USB_MIDI_SINK_H_
#define SRC_MEDIA_AUDIO_DRIVERS_USB_AUDIO_USB_MIDI_SINK_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.midi/cpp/wire.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/driver/node/cpp/add_child.h>
#include <lib/fit/function.h>
#include <lib/sync/completion.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <memory>
#include <optional>

#include <fbl/mutex.h>
#include <usb/request-cpp.h>
#include <usb/usb.h>

namespace audio {
namespace usb {

class UsbMidiSink : public fidl::WireServer<fuchsia_hardware_midi::Controller>,
                    public fidl::WireServer<fuchsia_hardware_midi::Device> {
 public:
  using UsbDevice = ::usb::UsbDevice;
  using UsbRequest = ::usb::Request<>;
  using UsbRequestQueue = ::usb::RequestQueue<>;

  static constexpr std::string_view kClassName = "midi";

  UsbMidiSink(const UsbDevice& usb, size_t parent_req_size)
      : usb_(usb), parent_req_size_(parent_req_size) {}

  static zx::result<std::unique_ptr<UsbMidiSink>> Create(
      fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent, const UsbDevice& usb,
      int index, const usb_interface_descriptor_t* intf, const usb_endpoint_descriptor_t* ep,
      size_t req_size);

  void Stop(fit::callback<void()> completer);

  // FIDL methods.
  void OpenSession(OpenSessionRequestView request, OpenSessionCompleter::Sync& completer) override;

  void GetInfo(GetInfoCompleter::Sync& completer) final;
  void Read(ReadCompleter::Sync& completer) final;
  void Write(WriteRequestView request, WriteCompleter::Sync& completer) final;

 private:
  zx_status_t Init(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent, int index,
                   const usb_interface_descriptor_t* intf, const usb_endpoint_descriptor_t* ep);
  void Serve(fidl::ServerEnd<fuchsia_hardware_midi::Controller> server);
  void WriteComplete(usb_request_t* req);

  zx_status_t WriteInternal(const uint8_t* src, size_t length);

  UsbDevice usb_;

  // pool of free USB requests
  UsbRequestQueue free_write_reqs_ TA_GUARDED(mutex_);
  // mutex for synchronizing access to free_write_reqs and open
  fbl::Mutex mutex_;
  // completion signals free_write_reqs not empty
  sync_completion_t free_write_completion_;

  struct Binding {
    fidl::ServerBindingRef<fuchsia_hardware_midi::Device> binding;
    std::optional<fit::callback<void()>> stop_completer;
  };
  std::optional<Binding> binding_;

  fidl::ServerBindingGroup<fuchsia_hardware_midi::Controller> controller_bindings_;
  driver_devfs::Connector<fuchsia_hardware_midi::Controller> devfs_connector_{
      fit::bind_member<&UsbMidiSink::Serve>(this)};
  std::optional<fdf::OwnedChildNode> child_;

  uint64_t parent_req_size_;
};

}  // namespace usb
}  // namespace audio

#endif  // SRC_MEDIA_AUDIO_DRIVERS_USB_AUDIO_USB_MIDI_SINK_H_
