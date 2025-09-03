// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This module is used to house common "global" configuration values that may
// cross multiple crates, plugins or tools so as to avoid large,
// cross-binary dependency graphs.

/// The default target to communicate with if no target is specified.
pub const TARGET_DEFAULT_KEY: &str = "target.default";

/// The timeout used before giving up on attempting to connect to a FIDL proxy.
pub const PROXY_TIMEOUT: &'static str = "proxy.timeout_secs";

/// The timeout used before giving up on uploading metrics in fractional seconds.
pub const METRICS_UPLOAD_TIMEOUT_KEY: &'static str = "metrics.upload_timeout";

/// The timeout, in milliseconds, when using the local discovery lib to locate a device.
pub const LOCAL_DISCOVERY_TIMEOUT: &str = "discovery.timeout";

/// This is a bit of a special case: the upload default timeout could potentially
/// be inaccessible due to not being able to initialize an `EnvironmentContext` correctly, so there
/// _needs_ to be a reasonable backup somewhere if that is the case. See `ffx_command::report_bug()`
/// for where this use-case is taken into account. While this isn't a "key" per se, it's just being
/// kept in this module for ease of discoverability/autocomplete.
// LINT.IfChange
pub const METRICS_UPLOAD_TIMEOUT_DEFAULT: f64 = 2.0;
// LINT.ThenChange(../../../docs/configuration.md, ../../../data/config.json)
