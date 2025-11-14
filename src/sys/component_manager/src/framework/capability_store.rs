// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::model::component::WeakComponentInstance;
use crate::sandbox_util::take_handle_as_stream;
use futures::FutureExt;
use futures::future::BoxFuture;
use std::sync::LazyLock;
use {fidl_fuchsia_component_sandbox as fsandbox, fuchsia_async as fasync};

static RECEIVER_SCOPE: LazyLock<fasync::Scope> = LazyLock::new(|| fasync::Scope::new());

pub fn serve(
    server_end: zx::Channel,
    _target: WeakComponentInstance,
    _source: WeakComponentInstance,
) -> BoxFuture<'static, Result<(), anyhow::Error>> {
    async move {
        let stream = take_handle_as_stream::<fsandbox::CapabilityStoreMarker>(server_end);
        sandbox::serve_capability_store(stream, &*RECEIVER_SCOPE).await.map_err(Into::into)
    }
    .boxed()
}
