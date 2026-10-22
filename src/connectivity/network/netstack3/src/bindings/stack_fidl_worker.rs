// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_net_stack::{StackRequest, StackRequestStream};
use futures::TryStreamExt as _;
use log::error;

use super::util::ResultExt as _;

pub(crate) async fn serve(stream: StackRequestStream) -> Result<(), fidl::Error> {
    stream
        .try_for_each(|req| async move {
            match req {
                StackRequest::SetDhcpClientEnabled { responder, id: _, enable } => {
                    // TODO(https://fxbug.dev/42162065): Remove this once
                    // DHCPv4 client is implemented out-of-stack.
                    if enable {
                        error!("TODO(https://fxbug.dev/42062356): Support starting DHCP client");
                    }
                    responder.send(Ok(())).unwrap_or_log("failed to respond");
                }
            }
            Ok(())
        })
        .await
}
