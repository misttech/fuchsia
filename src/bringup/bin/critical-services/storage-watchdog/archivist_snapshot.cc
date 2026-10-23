// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/archivist_snapshot.h"

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/zx/clock.h>

#include <condition_variable>
#include <memory>
#include <mutex>
#include <thread>

#include "src/bringup/bin/critical-services/storage-watchdog/diagnostics_text.h"

namespace storage_watchdog {
namespace {

using fuchsia_diagnostics::DataType;

constexpr size_t kMaxLogLines = 200;
constexpr size_t kMaxStorageLogLines = 50;
constexpr size_t kMaxLogBytes = 24ULL * 1024;
constexpr size_t kMaxInspectBytes = 16ULL * 1024;
constexpr size_t kMaxInspectBytesPerComponent = 10ULL * 1024;
// How long Archivist waits for one component's Inspect data.
constexpr int64_t kInspectComponentTimeoutSecs = 5;

// Storage and power components, by generic names. "[...]" selects every Inspect tree, not just
// "root" (drivers name theirs), and the colon in a collection child's name is escaped.
constexpr const char* kInspectSelectors[] = {
    "bootstrap/fshost:[...]root",
    "bootstrap/fshost/**:[...]root",
    "bootstrap/boot-drivers\\:*ufs*:[...]root",
    "bootstrap/boot-drivers\\:*sdmmc*:[...]root",
    "bootstrap/boot-drivers\\:*nvme*:[...]root",
    // power-broker's whole tree is big; take the leases and each element's state.
    "bootstrap/power-broker:[...]root/broker:leases",
    "bootstrap/power-broker:[...]root/broker/topology/fuchsia.inspect.Graph/topology/*/meta",
};

struct Fetch {
  std::mutex mutex;
  std::condition_variable cv;
  bool done = false;
  std::string error;
  std::vector<std::string> batches;
};

// Reads a whole snapshot of |type| from Archivist into |fetch|.
void RunFetch(DataType type, Fetch& fetch) {
  auto fail = [&fetch](std::string error) {
    std::scoped_lock lock(fetch.mutex);
    fetch.error = std::move(error);
  };
  zx::result accessor = component::Connect<fuchsia_diagnostics::ArchiveAccessor>();
  if (accessor.is_error()) {
    fail(std::string("connect: ") + accessor.status_string());
    return;
  }
  fuchsia_diagnostics::StreamParameters params;
  params.data_type(type);
  params.stream_mode(fuchsia_diagnostics::StreamMode::kSnapshot);
  params.format(fuchsia_diagnostics::Format::kJson);
  if (type == DataType::kInspect) {
    std::vector<fuchsia_diagnostics::SelectorArgument> selectors;
    for (const char* selector : kInspectSelectors) {
      selectors.push_back(fuchsia_diagnostics::SelectorArgument::WithRawSelector(selector));
    }
    params.client_selector_configuration(
        fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectors(std::move(selectors)));
    fuchsia_diagnostics::PerformanceConfiguration performance;
    performance.batch_retrieval_timeout_seconds(kInspectComponentTimeoutSecs);
    params.performance_configuration(std::move(performance));
  } else {
    params.client_selector_configuration(
        fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectAll(true));
  }
  auto [client, server] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  if (auto result =
          fidl::Call(*accessor)->StreamDiagnostics({std::move(params), std::move(server)});
      result.is_error()) {
    fail("StreamDiagnostics: " + result.error_value().FormatDescription());
    return;
  }
  fidl::SyncClient iterator(std::move(client));
  while (true) {
    auto result = iterator->GetNext();
    if (result.is_error()) {
      fail("GetNext: " + result.error_value().FormatDescription());
      return;
    }
    if (result->batch().empty()) {
      return;
    }
    for (const fuchsia_diagnostics::FormattedContent& content : result->batch()) {
      if (content.Which() != fuchsia_diagnostics::FormattedContent::Tag::kJson) {
        continue;
      }
      const fuchsia_mem::Buffer& buffer = content.json().value();
      std::string json(buffer.size(), '\0');
      if (buffer.vmo().read(json.data(), 0, json.size()) == ZX_OK) {
        std::scoped_lock lock(fetch.mutex);
        fetch.batches.push_back(std::move(json));
      }
    }
  }
}

std::shared_ptr<Fetch> StartFetch(DataType type) {
  auto fetch = std::make_shared<Fetch>();
  std::thread([type, fetch] {
    RunFetch(type, *fetch);
    std::scoped_lock lock(fetch->mutex);
    fetch->done = true;
    fetch->cv.notify_all();
  }).detach();
  return fetch;
}

// Waits for |fetch| until |deadline|, and returns its batches and a summary with |lines|.
std::vector<std::string> Wait(Fetch& fetch, zx::time deadline, std::string* status) {
  std::unique_lock lock(fetch.mutex);
  const bool done = fetch.cv.wait_for(
      lock, std::chrono::nanoseconds((deadline - zx::clock::get_monotonic()).get()),
      [&fetch] { return fetch.done; });
  *status = std::to_string(fetch.batches.size()) + " batches" + (done ? "" : ", TIMED OUT") +
            (fetch.error.empty() ? "" : ", error: " + fetch.error);
  return fetch.batches;
}

}  // namespace

ArchivistSnapshots TakeArchivistSnapshots(zx::time deadline) {
  std::shared_ptr logs = StartFetch(DataType::kLogs);
  std::shared_ptr inspect = StartFetch(DataType::kInspect);

  ArchivistSnapshots snapshots;
  std::string status;
  LogTail tail(kMaxLogLines, kMaxStorageLogLines, kMaxLogBytes);
  for (const std::string& json : Wait(*logs, deadline, &status)) {
    tail.AddJson(json);
  }
  snapshots.logs.lines = tail.Lines();
  snapshots.logs.summary =
      "logs: " + status + ", " + std::to_string(snapshots.logs.lines.size()) + " lines";

  snapshots.inspect.lines = InspectLines(Wait(*inspect, deadline, &status),
                                         kMaxInspectBytesPerComponent, kMaxInspectBytes);
  snapshots.inspect.summary =
      "inspect: " + status + ", " + std::to_string(snapshots.inspect.lines.size()) + " lines";
  return snapshots;
}

}  // namespace storage_watchdog
