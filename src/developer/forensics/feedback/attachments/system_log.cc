// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/attachments/system_log.h"

#include <fuchsia/logger/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <ios>
#include <memory>
#include <string>
#include <vector>

#include "src/developer/forensics/feedback_data/constants.h"
#include "src/developer/forensics/utils/cobalt/logger.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/log_format.h"
#include "src/developer/forensics/utils/purge_memory.h"
#include "src/lib/backoff/exponential_backoff.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics::feedback {
namespace {

constexpr int32_t kDefaultLogSeverity = 0;
const std::vector<std::string> kDefaultTags = {};

size_t AppendRepeated(const size_t last_msg_repeated, std::string& append_to) {
  const std::string repeated_str =
      last_msg_repeated == 1
          ? feedback_data::kRepeatedOnceFormatStr
          : fxl::StringPrintf(feedback_data::kRepeatedFormatStr, last_msg_repeated);

  append_to.append(repeated_str);
  return repeated_str.size();
}

}  // namespace

LogBuffer::LogBuffer(const StorageSize capacity, RedactorBase* redactor)
    : redactor_(redactor), capacity_(capacity.ToBytes()) {}

std::optional<zx::time_boot> LogBuffer::FirstTimestamp() const {
  return messages_.empty() ? std::nullopt : std::make_optional(messages_.front().timestamp);
}

std::optional<zx::time_boot> LogBuffer::LastTimestamp() const {
  return messages_.empty() ? std::nullopt : std::make_optional(messages_.back().timestamp);
}

bool LogBuffer::Add(LogSink::MessageOr message) {
  if (message.is_ok()) {
    redactor_->Redact(message.value().msg);
    for (std::string& tag : message.value().tags) {
      redactor_->Redact(tag);
    }
  } else {
    redactor_->Redact(message.error());
  }

  // Assume timestamp 0 if no messages have been added yet.
  const zx::time_boot last_timestamp =
      (messages_.empty()) ? zx::time_boot(0) : messages_.back().timestamp;
  const std::string& msg = (message.is_ok()) ? message.value().msg : message.error();
  const int32_t& severity = (message.is_ok()) ? message.value().severity : kDefaultLogSeverity;
  const std::vector<std::string>& tags = (message.is_ok()) ? message.value().tags : kDefaultTags;

  // Adds a new message to |messages_| and updates internal accounting.
  auto AddNew = [this, &message, &msg, &severity, &tags, last_timestamp] {
    messages_.emplace_back(message, last_timestamp);
    size_ += messages_.back().msg.size();

    last_msg_ = msg;
    last_severity_ = severity;
    last_tags = tags;
    last_msg_repeated_ = 0;
    is_sorted_ &= messages_.back().timestamp >= last_timestamp;

    return true;
  };

  const zx::time_boot action_timestamp = (message.is_ok()) ? message.value().time : last_timestamp;
  auto on_return = ::fit::defer([this, action_timestamp] {
    RunActions(action_timestamp);
    EnforceCapacity();
  });

  if (messages_.empty()) {
    return AddNew();
  }

  // The most recent message is repeated, don't need to create new data.
  if (last_msg_ == msg && last_severity_ == severity && last_tags == tags) {
    ++last_msg_repeated_;
    return true;
  }

  // Inject a signal the most previously added message was repeated.
  if (last_msg_repeated_ > 0) {
    size_ += AppendRepeated(last_msg_repeated_, messages_.back().msg);
  }

  return AddNew();
}

void LogBuffer::NotifyInterruption() {
  messages_.clear();
  ResetLastMessage();
  is_sorted_ = true;
  size_ = 0u;

  // Executing and deleting all remaining actions is safe because non-SystemLog controlled
  // interruptions aren't expected to occur.
  for (auto& [_, action] : actions_at_time_) {
    action();
  }
  actions_at_time_.clear();
}

std::string LogBuffer::ToString() {
  // Ensure messages appear in time order.
  Sort();

  std::string out;
  out.reserve(size_);
  for (const Message& message : messages_) {
    out.append(message.msg);
  }

  // Inject a signal the last message was repeated because the signal doesn't exist in the
  // log yet.
  if (last_msg_repeated_ > 0) {
    AppendRepeated(last_msg_repeated_, out);
  }

  return out;
}

void LogBuffer::ExecuteAfter(const zx::time_boot uptime, ::fit::closure action) {
  actions_at_time_.insert({uptime, std::move(action)});
}

void LogBuffer::Sort() {
  // No sort is needed.
  if (is_sorted_) {
    return;
  }

  // Inject a signal the last message was repeated because the sort may change which message is
  // last.
  if (last_msg_repeated_ > 0) {
    size_ += AppendRepeated(last_msg_repeated_, messages_.back().msg);
  }

  std::stable_sort(messages_.begin(), messages_.end(), [](const Message& lhs, const Message& rhs) {
    return lhs.timestamp < rhs.timestamp;
  });
  is_sorted_ = true;

  // Reset the message last added.
  //
  // Note: info used to deduplicate messages is lost; it has not yet been proven important enough
  // in the system log to justify the cost of identifying what the original msg was and
  // aggregating all adjacent messages that match it. For example, it may be possible to see the
  // sequence:
  //
  // LOG MESSAGE A
  // !!! MESSAGE REPEATED 3 MORE TIMES!!!
  // LOG MESSAGE A
  //
  // in a final system log.
  ResetLastMessage();
}

void LogBuffer::RunActions(const zx::time_boot timestamp) {
  for (auto it = actions_at_time_.lower_bound(timestamp); it != actions_at_time_.end();) {
    it->second();
    actions_at_time_.erase(it++);
  }
}

void LogBuffer::EnforceCapacity() {
  if (size_ <= capacity_) {
    return;
  }

  // Ensure messages are dropped in time order.
  Sort();
  while (size_ > capacity_ && !messages_.empty()) {
    size_ -= messages_.front().msg.size();
    messages_.pop_front();
  }
}

void LogBuffer::ResetLastMessage() {
  last_msg_ = "";
  last_severity_ = 0;
  last_tags = {};
  last_msg_repeated_ = 0u;
}

LogBuffer::Message::Message(const LogSink::MessageOr& message, zx::time_boot default_timestamp)
    : timestamp(message.is_ok() ? message.value().time : default_timestamp),
      msg(message.is_ok() ? Format(message.value())
                          : fxl::StringPrintf("!!! Failed to format chunk: %s !!!\n",
                                              message.error().c_str())) {}

SystemLog::SystemLog(async_dispatcher_t* dispatcher,
                     std::shared_ptr<sys::ServiceDirectory> services, timekeeper::Clock* clock,
                     RedactorBase* redactor, const zx::duration active_period,
                     cobalt::Logger* cobalt, LogBuffer* buffer)
    : dispatcher_(dispatcher),
      buffer_(buffer),
      source_(dispatcher, services, buffer_,
              std::make_unique<backoff::ExponentialBackoff>(zx::min(1), 2u, zx::hour(1))),
      clock_(clock),
      cobalt_(cobalt),
      active_period_(active_period) {}

namespace {

// Creates a callable object that can be used to complete the system log collection flow with an
// ok status or a timeout and a promise to consume that result.
auto CompletesAndConsume() {
  ::fpromise::bridge<void, Error> bridge;
  auto completer =
      std::make_shared<::fpromise::completer<void, Error>>(std::move(bridge.completer));

  return std::make_tuple(
      [completer] {
        if (!*(completer)) {
          return;
        }

        completer->complete_ok();
      },
      [completer](const Error error) {
        if (!*(completer)) {
          return;
        }

        FX_LOGS(WARNING) << "System log collection error " << ToString(error);
        completer->complete_error(error);
      },
      std::move(bridge.consumer).promise_or(::fpromise::error(Error::kLogicError)));
}

}  // namespace

::fpromise::promise<AttachmentValue> SystemLog::Get(const uint64_t ticket) {
  FX_CHECK(!completers_.contains(ticket)) << "Ticket used twice: " << ticket;

  if (!is_active_) {
    is_active_ = true;
    source_.Start();
  }

  auto [complete_ok, complete_error, consume] = CompletesAndConsume();

  completers_[ticket] = std::move(complete_error);

  // Cancel the outstanding |make_inactive_| because logs are being requested.
  make_inactive_.Cancel();

  fxl::WeakPtr<SystemLog> self = ptr_factory_.GetWeakPtr();

  buffer_->ExecuteAfter(clock_->BootNow(), std::move(complete_ok));

  return consume.then([self, ticket](const ::fpromise::result<void, Error>& result)
                          -> ::fpromise::result<AttachmentValue> {
    if (!self) {
      return ::fpromise::ok(AttachmentValue(Error::kLogicError));
    }

    if (result.is_error() && result.error() == Error::kLogicError) {
      FX_LOGS(FATAL) << "Log collection promise was incorrectly dropped";
    }

    self->completers_.erase(ticket);

    // Cancel the outstanding |make_inactive_| because the "active" period should be extended.
    self->make_inactive_.Cancel();
    self->make_inactive_.PostDelayed(self->dispatcher_, self->active_period_);

    std::string system_log = self->buffer_->ToString();

    // ToString sorts the logs, so we can assume the platform's log buffer is at capacity if the
    // first log has a timestamp > 0. Note that this is not referring to Feedback's buffer's
    // capacity (checked by EnforceCapacity).
    if (const std::optional<zx::time_boot> first_timestamp = self->buffer_->FirstTimestamp();
        first_timestamp.has_value() && first_timestamp.value() > zx::time_boot(0)) {
      self->cobalt_->LogIntegerEvent(cobalt_registry::kSyslogBytesAtCapacityMetricId,
                                     system_log.size());

      if (const std::optional<zx::time_boot> last_timestamp = self->buffer_->LastTimestamp();
          last_timestamp.has_value()) {
        const int64_t duration = (*last_timestamp - *first_timestamp).to_mins();
        self->cobalt_->LogIntegerEvent(cobalt_registry::kSyslogDurationAtCapacityMetricId,
                                       duration);
      }
    }

    if (system_log.empty()) {
      const Error error = (result.is_ok()) ? Error::kMissingValue : result.error();
      return ::fpromise::ok(AttachmentValue(error));
    }

    return ::fpromise::ok(result.is_ok() ? AttachmentValue(std::move(system_log))
                                         : AttachmentValue(std::move(system_log), result.error()));
  });
}

void SystemLog::ForceCompletion(const uint64_t ticket, const Error error) {
  if (completers_.contains(ticket)) {
    completers_[ticket](error);
  }
}

void SystemLog::MakeInactive() {
  FX_LOGS(INFO) << "System log not requested for " << active_period_.to_secs()
                << " seconds after last collection terminated, stopping streaming";

  is_active_ = false;
  source_.Stop();

  PurgeAllMemoryAfter(dispatcher_, zx::sec(0));
}

}  // namespace forensics::feedback
