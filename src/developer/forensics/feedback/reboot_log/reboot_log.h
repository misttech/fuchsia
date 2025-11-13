// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_REBOOT_LOG_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_REBOOT_LOG_H_

#include <lib/zx/time.h>

#include <memory>
#include <optional>
#include <string>

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

namespace forensics {
namespace feedback {

// Wrapper around a device's reboot log.
class RebootLog {
 public:
  static RebootLog ParseRebootLog(const std::string& zircon_reboot_log_path,
                                  const std::string& graceful_shutdown_info_path,
                                  const std::string& legacy_graceful_reboot_log_path,
                                  bool not_a_fdr);

  const std::string& RebootLogStr() const { return reboot_log_str_; }
  const std::optional<std::string>& Dlog() const { return dlog_; }
  const FinalShutdownInfo& GetFinalShutdownInfo() const { return *final_shutdown_info_; }
  const std::optional<zx::duration>& Uptime() const { return last_boot_uptime_; }
  const std::optional<zx::duration>& Runtime() const { return last_boot_runtime_; }
  const std::optional<std::string>& CriticalProcess() const { return critical_process_; }

  // Exposed for testing purposes.
  RebootLog(std::unique_ptr<FinalShutdownInfo> final_shutdown_info, std::string reboot_log_str,
            std::optional<std::string> dlog, std::optional<zx::duration> last_boot_uptime,
            std::optional<zx::duration> last_boot_runtime,
            std::optional<std::string> critical_process);

 private:
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info_;
  std::string reboot_log_str_;
  std::optional<std::string> dlog_;
  std::optional<zx::duration> last_boot_uptime_;
  std::optional<zx::duration> last_boot_runtime_;
  std::optional<std::string> critical_process_;
};

}  // namespace feedback
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_REBOOT_LOG_H_
