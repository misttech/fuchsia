// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fidl_fuchsia_developer_ffx as ffx;
use protocols::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum EchoError {
    #[error("FIDL error: {0}")]
    Fidl(#[from] fidl::Error),
}

#[ffx_protocol]
#[derive(Default)]
pub struct Echo;

#[async_trait(?Send)]
impl FidlProtocol for Echo {
    type Protocol = ffx::EchoMarker;
    type StreamHandler = FidlInstancedStreamHandler<Self>;
    type Error = EchoError;

    async fn handle(
        &self,
        _cx: &Context,
        req: ffx::EchoRequest,
    ) -> std::result::Result<(), EchoError> {
        match req {
            ffx::EchoRequest::EchoString { value, responder } => {
                responder.send(&String::from(value)).map_err(EchoError::from)
            }
        }
    }

    async fn start(&mut self, _cx: &Context) -> std::result::Result<(), EchoError> {
        log::debug!("started echo protocol");
        Ok(())
    }

    async fn stop(&mut self, _cx: &Context) -> std::result::Result<(), EchoError> {
        log::debug!("stopped echo protocol");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocols::testing::FakeDaemonBuilder;

    #[fuchsia::test]
    async fn test_echo() {
        let env = ffx_config::test_init().unwrap();
        let daemon = FakeDaemonBuilder::new(&env.context).register_fidl_protocol::<Echo>().build();
        let proxy = daemon.open_proxy::<ffx::EchoMarker>().await;
        let string = "check-it-out".to_owned();
        assert_eq!(string, proxy.echo_string(string.clone().as_ref()).await.unwrap());
    }
}
