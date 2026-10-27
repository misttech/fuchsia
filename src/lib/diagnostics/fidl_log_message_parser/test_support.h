// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_DIAGNOSTICS_FIDL_LOG_MESSAGE_PARSER_TEST_SUPPORT_H_
#define SRC_LIB_DIAGNOSTICS_FIDL_LOG_MESSAGE_PARSER_TEST_SUPPORT_H_

#include <fuchsia/diagnostics/cpp/fidl.h>
#include <fuchsia/logger/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/structured_backend/cpp/log_buffer.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>

#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

namespace diagnostics::fidl_log_message_parser::testing {

// Encodes a log record into raw FXT bytes using fuchsia_logging::LogBuffer.
std::vector<uint8_t> BuildFxtRecord(zx::basic_time<ZX_CLOCK_BOOT> timestamp,
                                    FuchsiaLogSeverity severity, std::string_view message,
                                    zx_koid_t pid = 0, zx_koid_t tid = 0,
                                    const std::vector<std::string>& tags = {});

// Encodes a log record into a VMO with ZX_PROP_VMO_CONTENT_SIZE set,
// ready for use in FormattedContent::WithFxt.
zx::vmo BuildFxtVmo(zx::basic_time<ZX_CLOCK_BOOT> timestamp, FuchsiaLogSeverity severity,
                    std::string_view message, zx_koid_t pid = 0, zx_koid_t tid = 0,
                    const std::vector<std::string>& tags = {});

// Transcodes a single JSON log string (or array) conforming to Archivist's JSON schema
// into a VMO containing the corresponding FXT records.
zx::vmo EncodeJsonLogToFxtVmo(std::string_view json_str);

// Transcodes a batch of JSON log strings into a vector of FXT VMOs.
std::vector<zx::vmo> EncodeJsonBatchToFxtVmos(const std::vector<std::string>& json_batch);

// Test compatibility shim: parses JSON FormattedContent using the legacy Archivist schema,
// validates payloads, converts valid messages to FXT records, and decodes them via
// ConvertFormattedContentToLogMessages. Forwards FXT FormattedContent directly.
fpromise::result<std::vector<fpromise::result<fuchsia::logger::LogMessage, std::string>>,
                 std::string>
ConvertFormattedContentForTest(fuchsia::diagnostics::FormattedContent content);

}  // namespace diagnostics::fidl_log_message_parser::testing

#endif  // SRC_LIB_DIAGNOSTICS_FIDL_LOG_MESSAGE_PARSER_TEST_SUPPORT_H_
