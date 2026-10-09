// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "fdomain")]
pub use fdomain_client::*;

#[cfg(feature = "fdomain")]
pub type Dialect = fdomain_client::fidl::FDomainResourceDialect;

#[cfg(not(feature = "fdomain"))]
pub type Dialect = ::fidl::encoding::DefaultFuchsiaResourceDialect;

#[cfg(feature = "fdomain")]
pub use fdomain_client::Channel as AsyncChannel;

#[cfg(feature = "fdomain")]
pub use fdomain_client::Socket as AsyncSocket;

#[cfg(feature = "fdomain")]
pub fn socket_to_async(s: AsyncSocket) -> AsyncSocket {
    s
}

#[cfg(not(feature = "fdomain"))]
pub fn socket_to_async(s: Socket) -> AsyncSocket {
    AsyncSocket::from_socket(s)
}

#[cfg(feature = "fdomain")]
pub fn channel_to_async(s: AsyncChannel) -> AsyncChannel {
    s
}

#[cfg(not(feature = "fdomain"))]
pub fn channel_to_async(s: Channel) -> AsyncChannel {
    AsyncChannel::from_channel(s)
}

#[cfg(not(feature = "fdomain"))]
pub use ::fidl::endpoints::ProxyHasDomain;

#[cfg(feature = "fdomain")]
pub use fdomain_client::fidl::Proxy as ProxyHasDomain;

#[cfg(not(feature = "fdomain"))]
pub use ::fidl::*;

#[cfg(not(feature = "fdomain"))]
pub use zx::MessageBuf;

#[cfg(feature = "fdomain")]
pub type NullableHandle = fdomain_client::Handle;

#[cfg(not(feature = "fdomain"))]
pub mod fidl {
    pub use ::fidl::endpoints::*;
}

#[cfg(feature = "fdomain")]
pub type ClientArg = std::sync::Arc<fdomain_client::Client>;
#[cfg(not(feature = "fdomain"))]
pub type ClientArg = ::fidl::endpoints::ZirconClient;

#[cfg(not(feature = "fdomain"))]
pub async fn wait_for_signals(
    handle: &impl AsHandleRef,
    signals: ::fidl::Signals,
) -> Result<::fidl::Signals, ::fidl::Status> {
    fuchsia_async::OnSignalsRef::new(handle.as_handle_ref(), signals).await
}

#[cfg(feature = "fdomain")]
pub async fn wait_for_signals(
    handle: &Handle,
    signals: ::fidl::Signals,
) -> Result<::fidl::Signals> {
    OnFDomainSignals::new(handle, signals).await
}
