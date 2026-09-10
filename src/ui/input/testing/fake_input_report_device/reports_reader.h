// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_INPUT_TESTING_FAKE_INPUT_REPORT_DEVICE_REPORTS_READER_H_
#define SRC_UI_INPUT_TESTING_FAKE_INPUT_REPORT_DEVICE_REPORTS_READER_H_

#include <fuchsia/input/report/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/wait.h>
#include <lib/async/time.h>
#include <lib/fidl/cpp/binding_set.h>

#include <deque>
#include <optional>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>

namespace fake_input_report_device {

class FakeInputDevice;

// Creates a fake class that vends the InputReportsReader API. This should be
// created and managed by FakeInputDevice.
// If this class is bound on a separate thread, that thread must be joined before
// this class is destructed.
class FakeInputReportsReader final : public fuchsia::input::report::InputReportsReader {
 public:
  // Create a FakeInputReportsReader. The pointer to FakeInputDevice is unmanaged and
  // the FakeInputDevice must outlive FakeInputReportsReader.
  explicit FakeInputReportsReader(
      fidl::InterfaceRequest<fuchsia::input::report::InputReportsReader> request,
      async_dispatcher_t* dispatcher, FakeInputDevice* device)
      : binding_(this, std::move(request), dispatcher), device_(device) {
    // Create a wait that will be called on dispatcher's shutdown that will free our
    // callback if one exists. This is why the dispatcher must be shutdown before FakeInputDevice
    // is destructed.
    zx::event::create(0, &shutdown_event_);
    dispatcher_shutdown_.emplace(shutdown_event_.get());
    dispatcher_shutdown_->Begin(binding_.dispatcher(),
                                [this](async_dispatcher_t* dispatcher, async::WaitOnce* wait,
                                       zx_status_t status, const zx_packet_signal_t* signal) {
                                  fbl::AutoLock lock(&lock_);
                                  callback_.reset();
                                });
  }

  void ReadInputReports(ReadInputReportsCallback callback) override;

  // Queues up the ReadInputReports callback if one exists. The callback will be run on the
  // async_dispatcher.
  void QueueCallback();

 private:
  // Send the ReadInputReports callback. This can only be called on the async_dispatcher thread.
  void Callback();
  void CallbackLocked() __TA_REQUIRES(lock_);

  // `shutdown_event_` must be declared before `dispatcher_shutdown_` so that `dispatcher_shutdown_`
  // is destructed (and cancelled) before `shutdown_event_` handle is closed.
  zx::event shutdown_event_;
  std::optional<async::WaitOnce> dispatcher_shutdown_;

  fbl::Mutex lock_;
  fidl::Binding<fuchsia::input::report::InputReportsReader> binding_ __TA_GUARDED(lock_);
  std::optional<ReadInputReportsCallback> callback_ __TA_GUARDED(lock_);
  FakeInputDevice* device_;
};

// Creates a fake class that vends the InputReportsReaderV2 API. This should be
// created and managed by FakeInputDevice.
// If this class is bound on a separate thread, that thread must be joined before
// this class is destructed.
class FakeInputReportsReaderV2 final : public fuchsia::input::report::InputReportsReaderV2 {
 public:
  explicit FakeInputReportsReaderV2(
      fidl::InterfaceRequest<fuchsia::input::report::InputReportsReaderV2> request,
      async_dispatcher_t* dispatcher, uint16_t max_unacknowledged_reports);

  void AcknowledgeReports(uint64_t last_acknowledged_report_stamp) override;
  void handle_unknown_method(uint64_t ordinal, bool method_has_response) override {}

  // Queues and sends reports to the client.
  void SendReports(std::vector<fuchsia::input::report::InputReport> reports);

 private:
  void SendReportsLocked() __TA_REQUIRES(lock_);

  // `shutdown_event_` must be declared before `dispatcher_shutdown_` so that `dispatcher_shutdown_`
  // is destructed (and cancelled) before `shutdown_event_` handle is closed.
  zx::event shutdown_event_;
  std::optional<async::WaitOnce> dispatcher_shutdown_;

  fbl::Mutex lock_;
  fidl::Binding<fuchsia::input::report::InputReportsReaderV2> binding_ __TA_GUARDED(lock_);
  const uint16_t max_unacknowledged_reports_;
  uint64_t last_report_stamp_ __TA_GUARDED(lock_) = 0;
  uint64_t last_acknowledged_report_stamp_ __TA_GUARDED(lock_) = 0;
  std::deque<fuchsia::input::report::InputReport> pending_reports_ __TA_GUARDED(lock_);
};

}  // namespace fake_input_report_device

#endif  // SRC_UI_INPUT_TESTING_FAKE_INPUT_REPORT_DEVICE_REPORTS_READER_H_
