// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/system_log_recorder/system_log_recorder.h"

#include <lib/syslog/cpp/macros.h>

#include <utility>

#include "src/developer/forensics/utils/purge_memory.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {

// No rate limiting in the first minute of recording to allow us to catch up on all the log
// messages prior to listening.
constexpr zx::duration kNoRateLimitDuration = zx::sec(60);

// 150 seconds was chosen as an appropriate delay in https://fxbug.dev/397695466. Experiments showed
// memory usage staying low after an initial surge of usage within the first 150 seconds. This surge
// could be due to the initial snapshot of logs from Diagnostics or due to rate limiting being
// disabled for the first 60 seconds.
constexpr zx::duration kPurgeMemoryDuration = zx::sec(150);

SystemLogRecorder::SystemLogRecorder(async_dispatcher_t* archive_dispatcher,
                                     async_dispatcher_t* write_dispatcher,
                                     std::shared_ptr<sys::ServiceDirectory> services,
                                     WriteParameters write_parameters,
                                     std::unique_ptr<RedactorBase> redactor,
                                     std::unique_ptr<Encoder> encoder)
    : archive_dispatcher_(archive_dispatcher),
      write_period_(write_parameters.period),
      is_running_(false),
      store_(write_parameters.total_log_size / write_parameters.max_num_files,
             write_parameters.max_write_size, std::move(redactor), std::move(encoder)),
      log_source_(archive_dispatcher, std::move(services), &store_),
      writer_(write_dispatcher, std::in_place, write_parameters.logs_dir,
              write_parameters.max_num_files),
      receiver_(this, archive_dispatcher) {}

void SystemLogRecorder::Start() {
  is_running_ = true;
  log_source_.Start();
  periodic_write_task_.Post(archive_dispatcher_);

  async::PostDelayedTask(
      archive_dispatcher_, [this] { store_.TurnOnRateLimiting(); }, kNoRateLimitDuration);

  PurgeAllMemoryAfter(archive_dispatcher_, kPurgeMemoryDuration);
}

void SystemLogRecorder::Flush(const std::optional<std::string>& message,
                              ::fit::callback<void()> callback) {
  FX_LOGS(INFO) << "Received signal to flush cached logs to disk";
  if (message.has_value()) {
    store_.AppendToEnd(message.value());
  }

  // Consume the data on the main thread, then pass the data to the writer thread to ensure data is
  // safely written to disk ASAP when the system log recorder (and maybe the system) is expected to
  // stop soon.
  LogMessageStore::ConsumeResult result = store_.Consume();
  writer_.AsyncCall(&SystemLogWriter::Write, std::move(result));

  flush_callbacks_.push(std::move(callback));
  writer_.AsyncCall(&SystemLogWriter::Fsync)
      .Then(receiver_.Once(&SystemLogRecorder::OnFlushComplete));
}

void SystemLogRecorder::OnFlushComplete(bool) {
  if (flush_callbacks_.empty()) {
    return;
  }

  ::fit::callback<void()> callback = std::move(flush_callbacks_.front());
  flush_callbacks_.pop();
  if (callback) {
    callback();
  }
}

void SystemLogRecorder::StopAndDeleteLogs() {
  is_running_ = false;

  // Stop collecting logs.
  log_source_.Stop();
  periodic_write_task_.Cancel();

  // Consume the data currently in the store to clear the buffer.
  std::ignore = store_.Consume();
  writer_.AsyncCall(&SystemLogWriter::DeleteLogs);

  FX_LOGS(INFO) << "Stopped log recording and flushed persisted logs";
}

void SystemLogRecorder::PeriodicWriteTask() {
  // Consume the data on the main thread to avoid thread safety issues in store_. Move the data to
  // the writer thread.
  LogMessageStore::ConsumeResult result = store_.Consume();
  writer_.AsyncCall(&SystemLogWriter::Write, std::move(result))
      .Then(receiver_.Once(&SystemLogRecorder::OnWriteComplete));
}

void SystemLogRecorder::OnWriteComplete(bool success) {
  if (is_running_) {
    periodic_write_task_.PostDelayed(archive_dispatcher_, write_period_);
  }
}

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics
