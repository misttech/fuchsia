// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub const REVISION_VAR: &str = "hw-revision";
pub const IS_USERSPACE_VAR: &str = "is-userspace";
pub const MAX_DOWNLOAD_SIZE_VAR: &str = "max-download-size";
pub const PRODUCT_VAR: &str = "product";

pub const LOCKED_VAR: &str = "vx-locked";

// Streaming flash variables

// Standalone
pub const STREAM_SEGMENT_SIZE: &str = "stream-segment-size";

// Takes a 'partition' argument
pub const PARTITION_SIZE: &str = "partition-size";
pub const PARTITION_START: &str = "partition-start";
