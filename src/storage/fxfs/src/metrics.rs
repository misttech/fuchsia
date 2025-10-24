// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fuchsia_inspect::{Inspector, LazyNode, Node};
use fuchsia_sync::Mutex;
use futures::future::BoxFuture;
use std::sync::LazyLock;

/// Root node to which the filesystem Inspect tree will be attached.
fn root() -> Node {
    #[cfg(target_os = "fuchsia")]
    static FXFS_ROOT_NODE: LazyLock<Mutex<fuchsia_inspect::Node>> =
        LazyLock::new(|| Mutex::new(fuchsia_inspect::component::inspector().root().clone_weak()));
    #[cfg(not(target_os = "fuchsia"))]
    static FXFS_ROOT_NODE: LazyLock<Mutex<Node>> = LazyLock::new(|| Mutex::new(Node::default()));

    FXFS_ROOT_NODE.lock().clone_weak()
}

/// `fs.detail` node for holding fxfs-specific metrics.
pub fn detail() -> Node {
    static DETAIL_NODE: LazyLock<Mutex<Node>> =
        LazyLock::new(|| Mutex::new(root().create_child("fs.detail")));

    DETAIL_NODE.lock().clone_weak()
}

pub fn register_fs(
    populate_stores_fn: impl Fn() -> BoxFuture<'static, Result<Inspector, Error>>
    + Sync
    + Send
    + 'static,
) -> LazyNode {
    root().create_lazy_child("stores", populate_stores_fn)
}
