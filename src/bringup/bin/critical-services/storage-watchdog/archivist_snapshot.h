// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_ARCHIVIST_SNAPSHOT_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_ARCHIVIST_SNAPSHOT_H_

#include <lib/zx/time.h>

#include <string>
#include <vector>

namespace storage_watchdog {

struct ArchivistSnapshot {
  // A one-line summary, including errors and timeouts.
  std::string summary;
  std::vector<std::string> lines;
};

struct ArchivistSnapshots {
  ArchivistSnapshot logs;
  ArchivistSnapshot inspect;
};

// Takes snapshots of the system log tail and of storage Inspect data from Archivist (which runs
// from bootfs and keeps logs in RAM). Each is fetched on its own thread, which is abandoned at
// |deadline| in case Archivist, or a component it asks for Inspect data, is stuck.
ArchivistSnapshots TakeArchivistSnapshots(zx::time deadline);

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_ARCHIVIST_SNAPSHOT_H_
