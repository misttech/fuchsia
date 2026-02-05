// Copyright 2027 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// VMO holding blobs served by fxfs have a name starting with this prefix.
pub const BLOB_NAME_PREFIX: &str = "blob-";

// Length of the hash hex representation appended to the VMO name.
pub const BLOB_NAME_HASH_LENGTH: usize = 8;
