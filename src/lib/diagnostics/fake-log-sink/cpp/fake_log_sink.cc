// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/diagnostics/fake-log-sink/cpp/fake_log_sink.h"

#include <lib/async-loop/loop.h>
#include <lib/zx/socket.h>
#include <zircon/assert.h>

#include <condition_variable>

#include <sdk/lib/syslog/cpp/log_settings.h>

#include "src/lib/diagnostics/log/message/rust/cpp-log-decoder/log_decoder.h"

namespace fuchsia_logging {

class FakeLogSink::Impl : public fidl::Server<fuchsia_logger::LogSink> {
 public:
  explicit Impl(RawLogSeverity severity) : severity_(severity) {}

  void set_severity(RawLogSeverity severity) {
    std::unique_lock lock(mutex_);
    severity_ = severity;
    if (!interest_change_completers_.empty()) {
      for (auto& completer : interest_change_completers_) {
        completer.Reply(
            fit::ok(fuchsia_logger::LogSinkWaitForInterestChangeResponse().data().min_severity(
                static_cast<fuchsia_diagnostics_types::Severity>(severity))));
      }
      interest_change_completers_.clear();
      last_reported_severity_ = severity;
    }
  }

  bool WaitForRecord(zx::time deadline) const {
    std::unique_lock lock(mutex_);
    return WaitForRecord(lock, deadline) != 0;
  }

  std::vector<uint8_t> ReadRecord() {
    std::unique_lock lock(mutex_);
    size_t amount = WaitForRecord(lock, zx::time::infinite());
    std::vector<uint8_t> buffer(amount);
    size_t actual;
    if (socket_.read(0, buffer.data(), buffer.size(), &actual) != ZX_OK) {
      return {};
    };
    buffer.resize(actual);
    return buffer;
  }

 private:
  size_t WaitForRecord(std::unique_lock<std::mutex>& lock, zx::time deadline) const {
    condition_.wait(lock, [this] { return socket_.is_valid(); });
    zx_info_socket_t info;
    for (;;) {
      if (socket_.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr) != ZX_OK) {
        return 0;
      }
      if (info.rx_buf_available > 0) {
        return info.rx_buf_available;
      }
      if (socket_.wait_one(ZX_SOCKET_READABLE, deadline, nullptr) != ZX_OK) {
        return 0;
      }
    }
  }

  void ConnectStructured(ConnectStructuredRequest& request,
                         ConnectStructuredCompleter::Sync& completer) override {
    std::unique_lock lock(mutex_);
    socket_ = std::move(request.socket());
    last_reported_severity_ = std::nullopt;
    condition_.notify_all();
  }

  void WaitForInterestChange(WaitForInterestChangeCompleter::Sync& completer) override {
    std::unique_lock lock(mutex_);
    if (!last_reported_severity_.has_value() || *last_reported_severity_ != severity_) {
      last_reported_severity_ = severity_;
      completer.Reply(
          fit::ok(fuchsia_logger::LogSinkWaitForInterestChangeResponse().data().min_severity(
              static_cast<fuchsia_diagnostics_types::Severity>(severity_))));
    } else {
      interest_change_completers_.push_back(completer.ToAsync());
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_logger::LogSink> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    ZX_PANIC("Unexpected call to handle_unknown_method");
  }

  mutable std::mutex mutex_;
  mutable std::condition_variable condition_;
  zx::socket socket_;
  std::optional<RawLogSeverity> last_reported_severity_;
  RawLogSeverity severity_;
  std::vector<WaitForInterestChangeCompleter::Async> interest_change_completers_;
};

FakeLogSink::FakeLogSink(RawLogSeverity severity,
                         fidl::ServerEnd<fuchsia_logger::LogSink> server_end)
    : loop_(std::make_unique<async::Loop>(&kAsyncLoopConfigNeverAttachToThread)),
      impl_(std::make_shared<Impl>(severity)) {
  loop_->StartThread();

  if (server_end.is_valid()) {
    fidl::BindServer(loop_->dispatcher(), std::move(server_end), impl_);
  } else {
    auto endpoints = fidl::CreateEndpoints<fuchsia_logger::LogSink>();
    ZX_ASSERT(endpoints.is_ok());

    // Create a dispatcher for the global logger.
    static async::Loop* global_loop = [] {
      auto* loop = new async::Loop(&kAsyncLoopConfigNeverAttachToThread);
      loop->StartThread();
      return loop;
    }();

    fidl::BindServer(loop_->dispatcher(), std::move(endpoints->server), impl_);

    LogSettingsBuilder builder;
    builder.WithLogSink(endpoints->client.TakeChannel().release())
        .WithDispatcher(global_loop->dispatcher())
        .BuildAndInitialize();
  }
}

bool FakeLogSink::WaitForRecord(zx::time deadline) const { return impl_->WaitForRecord(deadline); }

std::vector<uint8_t> FakeLogSink::ReadRecord() { return impl_->ReadRecord(); }

std::optional<diagnostics::reader::LogsData> FakeLogSink::ReadLogsData() {
  auto record = impl_->ReadRecord();
  if (record.empty()) {
    return {};
  }
  auto raw_message = fuchsia_decode_log_message_to_json(record.data(), record.size());
  rapidjson::Document document;
  document.Parse(raw_message);
  fuchsia_free_decoded_log_message(raw_message);
  rapidjson::Document log;
  log.CopyFrom(document.GetArray()[0], log.GetAllocator());
  return diagnostics::reader::LogsData(std::move(log));
}

void FakeLogSink::set_severity(RawLogSeverity severity) { impl_->set_severity(severity); }

}  // namespace fuchsia_logging
