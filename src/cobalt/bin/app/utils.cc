// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/utils.h"

#include <fuchsia/buildinfo/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>

#include <format>

#include "third_party/cobalt/src/lib/util/pem_util.h"
#include "third_party/cobalt/src/public/lib/status.h"

namespace cobalt {

fpromise::result<void, fuchsia::metrics::Error> ToMetricsResult(const Status &s) {
  switch (s.error_code()) {
    case StatusCode::OK:
      return fpromise::ok();
    case StatusCode::INVALID_ARGUMENT:
      return fpromise::error(fuchsia::metrics::Error::INVALID_ARGUMENTS);
    case StatusCode::RESOURCE_EXHAUSTED:
      return fpromise::error(fuchsia::metrics::Error::BUFFER_FULL);
    case StatusCode::CANCELLED:
    case StatusCode::UNKNOWN:
    case StatusCode::DEADLINE_EXCEEDED:
    case StatusCode::NOT_FOUND:
    case StatusCode::ALREADY_EXISTS:
    case StatusCode::PERMISSION_DENIED:
    case StatusCode::FAILED_PRECONDITION:
    case StatusCode::ABORTED:
    case StatusCode::OUT_OF_RANGE:
    case StatusCode::UNIMPLEMENTED:
    case StatusCode::INTERNAL:
    case StatusCode::UNAVAILABLE:
    case StatusCode::DATA_LOSS:
    case StatusCode::UNAUTHENTICATED:
    default:
      return fpromise::error(fuchsia::metrics::Error::INTERNAL_ERROR);
  }
}

std::string ReadPublicKeyPem(const std::string &pem_file_path) {
  FX_LOGS(DEBUG) << "Reading PEM file at " << pem_file_path;
  std::string pem_out;
  FX_CHECK(util::PemUtil::ReadTextFile(pem_file_path, &pem_out))
      << "Unable to read file public key PEM file from path " << pem_file_path << ".";
  return pem_out;
}

std::string GetSystemVersion(const fuchsia::buildinfo::BuildInfo &build_info) {
  if (!build_info.has_product_version() || build_info.product_version().empty() ||
      !build_info.has_platform_version() || build_info.platform_version().empty()) {
    return "<version not specified>";
  }

  if (build_info.product_version() != build_info.platform_version()) {
    return std::format("{}--{}", build_info.product_version(), build_info.platform_version());
  }

  return build_info.product_version();
}

}  // namespace cobalt
