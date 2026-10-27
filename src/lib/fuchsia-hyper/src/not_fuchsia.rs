// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{HyperConnectorFuture, SocketOptions, TcpOptions, TcpStream, parse_ip_addr};
use futures::{StreamExt, io};
use http::uri::{Scheme, Uri};
use hyper_util::rt::TokioIo;
use log::{debug, warn};
use netext::TokioAsyncReadExt;
use rustls::RootCertStore;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use tokio::net;
use tokio::time::Duration;
use tower_service::Service;

fn load_certs_from_env(
    cert_file: Option<&Path>,
    cert_dir: Option<&Path>,
) -> Option<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file_path = cert_file.and_then(|path| {
        if path.exists() {
            Some(path)
        } else {
            warn!("SSL_CERT_FILE is set to {path:?}, but the path does not exist");
            None
        }
    });

    let dir_path = cert_dir.and_then(|path| {
        if path.exists() {
            Some(path)
        } else {
            warn!("SSL_CERT_DIR is set to {path:?}, but the path does not exist");
            None
        }
    });

    if file_path.is_none() && dir_path.is_none() {
        return None;
    }

    debug!(
        "Loading TLS CA certificates from SSL_CERT_FILE={file_path:?}, SSL_CERT_DIR={dir_path:?}"
    );
    let res = rustls_native_certs::load_certs_from_paths(file_path, dir_path);
    for err in &res.errors {
        warn!(
            "Error loading TLS CA certificates from env (file={file_path:?}, dir={dir_path:?}): {err}"
        );
    }
    if res.certs.is_empty() {
        warn!(
            "No valid TLS CA certificates loaded from configured SSL paths (file={file_path:?}, dir={dir_path:?})"
        );
    } else {
        debug!(
            "Loaded {} TLS CA certificates from env (file={file_path:?}, dir={dir_path:?})",
            res.certs.len()
        );
    }

    Some(res.certs)
}

fn load_native_certs() -> Vec<rustls::pki_types::CertificateDer<'static>> {
    let env_file = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    let env_dir = std::env::var_os("SSL_CERT_DIR").map(PathBuf::from);

    if let Some(certs) = load_certs_from_env(env_file.as_deref(), env_dir.as_deref()) {
        return certs;
    }

    debug!("Loading TLS CA certificates from platform root store");
    rustls_native_certs::load_native_certs()
        .expect("Could not load TLS CA certificates from platform root store")
}

pub fn new_root_cert_store() -> Arc<RootCertStore> {
    // It can be expensive to parse the certs, so cache them
    static ROOT_STORE: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
        let mut root_store = rustls::RootCertStore::empty();

        let certs = load_native_certs();

        if !certs.is_empty() {
            let (added, ignored) = root_store.add_parsable_certificates(certs);

            if ignored != 0 {
                warn!("Failed to load {ignored} certificates into the root store");
            }

            if added == 0 {
                panic!("Unable to load any TLS CA certificates from platform root store")
            }
        }

        Arc::new(root_store)
    });

    Arc::clone(&ROOT_STORE)
}

/// A Async-std-compatible implementation of hyper's `Connect` trait which allows
/// creating a TcpStream to a particular destination.
#[derive(Clone, Debug)]
pub struct HyperConnector {
    tcp_options: TcpOptions,
    socket_options: SocketOptions,
}

impl From<(TcpOptions, SocketOptions)> for HyperConnector {
    fn from((tcp_options, socket_options): (TcpOptions, SocketOptions)) -> Self {
        Self { tcp_options, socket_options }
    }
}

impl HyperConnector {
    pub fn new() -> Self {
        Self::from_tcp_options(TcpOptions::default())
    }

    pub fn from_tcp_options(tcp_options: TcpOptions) -> Self {
        Self { tcp_options, socket_options: SocketOptions::default() }
    }
}

impl Service<Uri> for HyperConnector {
    type Response = TokioIo<TcpStream>;
    type Error = std::io::Error;
    type Future = HyperConnectorFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let self_ = self.clone();
        HyperConnectorFuture { fut: Box::pin(async move { self_.call_async(dst).await }) }
    }
}

impl HyperConnector {
    async fn call_async(&self, dst: Uri) -> Result<TokioIo<TcpStream>, io::Error> {
        let port = match dst.port() {
            Some(p) => p.as_u16(),
            None => {
                if dst.scheme() == Some(&Scheme::HTTPS) {
                    443
                } else {
                    80
                }
            }
        };

        let host = match dst.host() {
            Some(host) => host,
            _ => return Err(io::Error::other("missing host in Uri")),
        };

        let addr = parse_ip_addr(host, port, |_| async {
            Err(io::Error::other("does not yet support non-integer zone ids"))
        })
        .await?;

        if self.socket_options.bind_device.is_some() {
            unimplemented!(
                "TODO(https://fxbug.dev/42083862) fuchsia-hyper does not support bind_device on non-fuchsia devices"
            );
        }

        let stream = if let Some(addr) = addr {
            match tokio::time::timeout(CONNECT_TIMEOUT, net::TcpStream::connect(addr)).await {
                Ok(res) => res?,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "connection attempt timed out",
                    ));
                }
            }
        } else {
            resolve_host_port(host, port).await?
        };
        // `TcpOptions::apply` only configures keepalive and receive buffer size options,
        // so setting `TCP_NODELAY` here is not overwritten below.
        let _ = stream.set_nodelay(true);
        let () = self.tcp_options.apply(&stream)?;

        Ok(TokioIo::new(TcpStream { stream: stream.into_multithreaded_futures_stream() }))
    }
}

/// Connection attempt delay according to RFC 8305 §5.
const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);
/// Individual connection attempt timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(2000);
/// DNS resolution timeout. Kept relatively short (2s) as host-side tools primarily perform
/// local network lookups (e.g. connecting to development devices).
const DNS_TIMEOUT: Duration = Duration::from_millis(2000);

/// Sorts/interleaves addresses to alternate between address families (IPv6, IPv4) per RFC 8305 §4.
/// Duplicate addresses are filtered out to avoid redundant connection attempts.
fn interleave_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = std::collections::HashSet::new();
    let (v6, v4): (Vec<SocketAddr>, Vec<SocketAddr>) =
        addrs.into_iter().filter(|addr| seen.insert(*addr)).partition(|addr| addr.is_ipv6());
    let mut result = Vec::with_capacity(v6.len() + v4.len());
    let mut v6_iter = v6.into_iter();
    let mut v4_iter = v4.into_iter();
    loop {
        match (v6_iter.next(), v4_iter.next()) {
            (Some(a6), Some(a4)) => {
                result.push(a6);
                result.push(a4);
            }
            (Some(a6), None) => {
                result.push(a6);
                result.extend(v6_iter);
                break;
            }
            (None, Some(a4)) => {
                result.push(a4);
                result.extend(v4_iter);
                break;
            }
            (None, None) => break,
        }
    }
    result
}

/// Connects to one of candidate addresses using Happy Eyeballs v2 (RFC 8305).
async fn happy_eyeballs_connect(addrs: Vec<SocketAddr>) -> Result<net::TcpStream, io::Error> {
    happy_eyeballs_connect_inner(
        addrs,
        HAPPY_EYEBALLS_DELAY,
        CONNECT_TIMEOUT,
        net::TcpStream::connect,
    )
    .await
}

async fn happy_eyeballs_connect_inner<C, Fut>(
    addrs: Vec<SocketAddr>,
    delay: Duration,
    connect_timeout: Duration,
    connect_fn: C,
) -> Result<net::TcpStream, io::Error>
where
    C: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<net::TcpStream, io::Error>>,
{
    let mut addr_iter = addrs.into_iter();
    let Some(first_addr) = addr_iter.next() else {
        return Err(io::Error::other("destination resolved to no address"));
    };

    let mut in_flight = futures::stream::FuturesUnordered::new();
    let mut last_err = None;

    // Start first connection attempt.
    in_flight.push(tokio::time::timeout(connect_timeout, connect_fn(first_addr)));
    let mut next_attempt_timer =
        (!addr_iter.as_slice().is_empty()).then(|| Box::pin(tokio::time::sleep(delay)));

    while !in_flight.is_empty() {
        tokio::select! {
            res = in_flight.next() => {
                if let Some(res) = res {
                    match res {
                        Ok(Ok(stream)) => return Ok(stream),
                        Ok(Err(err)) => {
                            debug!("Connection attempt failed: {err}");
                            last_err = Some(err);
                        }
                        Err(_) => {
                            debug!("Connection attempt timed out after {connect_timeout:?}");
                            last_err = Some(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "connection attempt timed out",
                            ));
                        }
                    }
                }
                // Per RFC 8305 §5, if a connection attempt fails, immediately initiate
                // the next connection attempt without waiting for the delay timer.
                if let Some(next_addr) = addr_iter.next() {
                    in_flight.push(tokio::time::timeout(connect_timeout, connect_fn(next_addr)));
                    next_attempt_timer = (!addr_iter.as_slice().is_empty())
                        .then(|| Box::pin(tokio::time::sleep(delay)));
                } else {
                    next_attempt_timer = None;
                }
            }
            () = async { next_attempt_timer.as_mut().unwrap().await }, if next_attempt_timer.is_some() => {
                if let Some(next_addr) = addr_iter.next() {
                    in_flight.push(tokio::time::timeout(connect_timeout, connect_fn(next_addr)));
                    next_attempt_timer = (!addr_iter.as_slice().is_empty())
                        .then(|| Box::pin(tokio::time::sleep(delay)));
                } else {
                    next_attempt_timer = None;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("all connection attempts failed")))
}

/// Resolve a hostname into an address using Happy Eyeballs v2 (RFC 8305).
async fn resolve_host_port(host: &str, port: u16) -> Result<net::TcpStream, io::Error> {
    let addrs = match tokio::time::timeout(DNS_TIMEOUT, net::lookup_host((host, port))).await {
        Ok(res) => res?,
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "DNS resolution timed out")),
    };
    let interleaved = interleave_addrs(addrs);
    happy_eyeballs_connect(interleaved).await
}

////////////////////////////////////////////////////////////////////////////////
///// tests

#[cfg(test)]
mod test {
    use super::*;
    use crate::*;
    use anyhow::{Error, Result};
    use futures::future::BoxFuture;
    use futures::stream::FuturesUnordered;
    use futures::{StreamExt, TryStreamExt};
    use http_body_util::BodyExt as _;
    use hyper::{Response, StatusCode};
    use std::convert::Infallible;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    const TEST_CERT_1: &str = "-----BEGIN CERTIFICATE-----\n\
MIICGzCCAaGgAwIBAgIQQdKd0XLq7qeAwSxs6S+HUjAKBggqhkjOPQQDAzBPMQsw\n\
CQYDVQQGEwJVUzEpMCcGA1UEChMgSW50ZXJuZXQgU2VjdXJpdHkgUmVzZWFyY2gg\n\
R3JvdXAxFTATBgNVBAMTDElTUkcgUm9vdCBYMjAeFw0yMDA5MDQwMDAwMDBaFw00\n\
MDA5MTcxNjAwMDBaME8xCzAJBgNVBAYTAlVTMSkwJwYDVQQKEyBJbnRlcm5ldCBT\n\
ZWN1cml0eSBSZXNlYXJjaCBHcm91cDEVMBMGA1UEAxMMSVNSRyBSb290IFgyMHYw\n\
EAYHKoZIzj0CAQYFK4EEACIDYgAEzZvVn4CDCuwJSvMWSj5cz3es3mcFDR0HttwW\n\
+1qLFNvicWDEukWVEYmO6gbf9yoWHKS5xcUy4APgHoIYOIvXRdgKam7mAHf7AlF9\n\
ItgKbppbd9/w+kHsOdx1ymgHDB/qo0IwQDAOBgNVHQ8BAf8EBAMCAQYwDwYDVR0T\n\
AQH/BAUwAwEB/zAdBgNVHQ4EFgQUfEKWrt5LSDv6kviejM9ti6lyN5UwCgYIKoZI\n\
zj0EAwMDaAAwZQIwe3lORlCEwkSHRhtFcP9Ymd70/aTSVaYgLXTWNLxBo1BfASdW\n\
tL4ndQavEi51mI38AjEAi/V3bNTIZargCyzuFJ0nN6T5U6VR5CmD1/iQMVtCnwr1\n\
/q4AaOeMSQ+2b1tbFfLn\n\
-----END CERTIFICATE-----\n";

    const TEST_CERT_2: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5\n\
MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g\n\
Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG\n\
A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg\n\
Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl\n\
ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j\n\
QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr\n\
ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr\n\
BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM\n\
YyRIHN8wfdVoOw==\n\
-----END CERTIFICATE-----\n";

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fuchsia_hyper_test_{}_{}",
                std::process::id(),
                id
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    trait AsyncReadWrite: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send {}
    impl<T> AsyncReadWrite for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send {}

    async fn fetch_url<W: Write>(url: hyper::Uri, mut buffer: W) -> Result<StatusCode> {
        let client = new_https_client();

        let mut res = client.get(url).await?;
        let status = res.status();

        if status == StatusCode::OK {
            while let Some(frame) = res.frame().await {
                if let Ok(data) = frame?.into_data() {
                    buffer.write_all(&data)?;
                }
            }
            buffer.flush()?;
        }

        Ok(status)
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_download_succeeds() -> Result<()> {
        let (listener, addr) = {
            let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0);
            let listener = TcpListener::bind(&addr).await.unwrap();
            let local_addr = listener.local_addr().unwrap();
            (listener, local_addr)
        };

        #[cfg(target_os = "fuchsia")]
        let listener =
            listener.incoming().map_err(Error::from).map_ok(|conn| TcpStream { stream: conn });
        #[cfg(not(target_os = "fuchsia"))]
        let listener = netext::TcpListenerStream(listener).map_err(Error::from).map_ok(|conn| {
            TcpStream { stream: netext::TokioAsyncReadExt::into_multithreaded_futures_stream(conn) }
        });

        let mut connections = listener
            .map_ok(|conn| Pin::new(Box::new(conn)) as Pin<Box<dyn AsyncReadWrite>>)
            .boxed();

        let (stop, mut rx_stop) = futures::channel::oneshot::channel();

        let server = async move {
            loop {
                let conn = match futures::future::select(connections.next(), &mut rx_stop).await {
                    futures::future::Either::Left((Some(Ok(conn)), _)) => conn,
                    _ => break,
                };
                let io = hyper_util::rt::TokioIo::new(conn);
                let service = hyper::service::service_fn(
                    move |_req: hyper::Request<hyper::body::Incoming>| async move {
                        Ok::<_, Infallible>(Response::new(http_body_util::Full::new(
                            hyper::body::Bytes::from("Hello"),
                        )))
                    },
                );
                let builder = hyper_util::server::conn::auto::Builder::new(Executor);
                let mut conn_fut = Box::pin(builder.serve_connection(io, service));
                match futures::future::select(&mut conn_fut, &mut rx_stop).await {
                    futures::future::Either::Left(_) => {}
                    futures::future::Either::Right(_) => {
                        conn_fut.as_mut().graceful_shutdown();
                        let _ = conn_fut.await;
                        break;
                    }
                }
            }
            Ok(())
        };

        let client = async {
            let output: Vec<u8> = Vec::new();
            let status = fetch_url(format!("http://{addr}").parse::<hyper::Uri>().unwrap(), output)
                .await
                .unwrap();
            match status {
                StatusCode::OK | StatusCode::FOUND => {}
                _ => assert!(false, "Unexpected status code: {}", status),
            }
            stop.send(()).expect("server to still be running");
            Ok(())
        };

        let mut tasks: FuturesUnordered<BoxFuture<'_, Result<(), Error>>> = FuturesUnordered::new();
        tasks.push(Box::pin(server));
        tasks.push(Box::pin(client));
        while let Some(Ok(())) = tasks.next().await {}
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_download_handles_bad_domain() -> Result<()> {
        let output: Vec<u8> = Vec::new();
        let res = fetch_url("https://domain.invalid".parse::<hyper::Uri>()?, output).await;
        assert!(res.is_err());
        Ok(())
    }
    #[test]
    fn test_load_certs_from_env() {
        let temp_dir = TestTempDir::new();

        // Neither file nor dir provided -> returns None.
        assert!(load_certs_from_env(None, None).is_none());

        // Non-existent file and dir -> returns None.
        let missing_file = temp_dir.path.join("nonexistent.pem");
        let missing_dir = temp_dir.path.join("nonexistent_dir");
        assert!(load_certs_from_env(Some(&missing_file), Some(&missing_dir)).is_none());

        // Valid SSL_CERT_FILE -> loads certificate.
        let cert_file = temp_dir.path.join("bundle.pem");
        std::fs::write(&cert_file, TEST_CERT_1).unwrap();
        let certs = load_certs_from_env(Some(&cert_file), None).expect("should load from file");
        assert_eq!(certs.len(), 1);
        let mut store = rustls::RootCertStore::empty();
        let (added, ignored) = store.add_parsable_certificates(certs);
        assert_eq!(added, 1);
        assert_eq!(ignored, 0);

        // Valid SSL_CERT_DIR -> loads certificate from directory.
        let cert_dir = temp_dir.path.join("certs");
        std::fs::create_dir_all(&cert_dir).unwrap();
        std::fs::write(cert_dir.join("cert2.pem"), TEST_CERT_2).unwrap();
        let dir_certs = load_certs_from_env(None, Some(&cert_dir)).expect("should load from dir");
        assert_eq!(dir_certs.len(), 1);

        // Both SSL_CERT_FILE and SSL_CERT_DIR -> loads from both.
        let both_certs = load_certs_from_env(Some(&cert_file), Some(&cert_dir))
            .expect("should load from file and dir");
        assert_eq!(both_certs.len(), 2);

        // Existing SSL_CERT_FILE with no valid certificates -> returns Some(empty) to override
        // rather than falling back to the system root store.
        let empty_file = temp_dir.path.join("empty.pem");
        std::fs::write(&empty_file, "not a valid pem cert").unwrap();
        let empty_certs = load_certs_from_env(Some(&empty_file), None)
            .expect("explicit existing file should override even when empty");
        assert!(empty_certs.is_empty());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_resolve_host_port_connects_to_localhost() -> Result<()> {
        // Reserve the same port on both IPv4 and IPv6 loopback so another
        // concurrent test (e.g. `test_download_succeeds` binding `[::1]:0`)
        // cannot be assigned the same port on the other address family and
        // intercept the `localhost` connection attempt.
        let mut rejected_v4 = Vec::new();
        let (v4_listener, v6_listener, port) = loop {
            let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let port = v4.local_addr()?.port();
            match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
                Ok(v6) => break (v4, v6, port),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    rejected_v4.push(v4);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        };
        drop(rejected_v4);

        let accept_fut = async move {
            match futures::future::select(
                Box::pin(v4_listener.accept()),
                Box::pin(v6_listener.accept()),
            )
            .await
            {
                futures::future::Either::Left((res, _))
                | futures::future::Either::Right((res, _)) => {
                    let (_stream, _peer) = res?;
                    Ok::<(), io::Error>(())
                }
            }
        };

        let connect_fut = resolve_host_port("localhost", port);
        let ((), _stream) = futures::future::try_join(accept_fut, connect_fut).await?;
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_happy_eyeballs_empty_and_all_fail() -> Result<()> {
        // Empty candidate list returns destination resolved to no address error.
        let err = happy_eyeballs_connect(vec![]).await.unwrap_err();
        assert_eq!(err.to_string(), "destination resolved to no address");

        // All connection attempts fail with ConnectionRefused -> returns last error.
        let addr1: SocketAddr = "[::1]:1".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let err = happy_eyeballs_connect_inner(
            vec![addr1, addr2],
            Duration::from_millis(10),
            Duration::from_millis(50),
            |_addr| async {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_happy_eyeballs_immediate_retry_on_failure() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let good_addr = listener.local_addr()?;
        let bad_addr: SocketAddr = "[::1]:1".parse().unwrap();

        let accept_fut = async move {
            let (_stream, _peer) = listener.accept().await?;
            Ok::<(), io::Error>(())
        };

        // Use a very long delay (60s) to verify that when the first attempt fails immediately,
        // the second attempt is initiated immediately per RFC 8305 §5 without waiting for the
        // delay timer.
        let connect_fut = tokio::time::timeout(
            Duration::from_secs(2),
            happy_eyeballs_connect_inner(
                vec![bad_addr, good_addr],
                Duration::from_secs(60),
                Duration::from_secs(2),
                move |addr| async move {
                    if addr == bad_addr {
                        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"))
                    } else {
                        net::TcpStream::connect(addr).await
                    }
                },
            ),
        );

        let (accept_res, connect_res) = futures::future::join(accept_fut, connect_fut).await;
        accept_res?;
        let _stream = connect_res.expect("should not wait for 60s delay timer")?;
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_happy_eyeballs_staggered_fallback_and_timeout() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let good_addr = listener.local_addr()?;
        let hanging_addr1: SocketAddr = "[::1]:1".parse().unwrap();
        let hanging_addr2: SocketAddr = "127.0.0.1:2".parse().unwrap();

        let accept_fut = async move {
            let (_stream, _peer) = listener.accept().await?;
            Ok::<(), io::Error>(())
        };

        // First attempt hangs; after the 20ms delay timer expires, the second attempt connects
        // and succeeds while the first is still pending.
        let connect_fut = happy_eyeballs_connect_inner(
            vec![hanging_addr1, good_addr],
            Duration::from_millis(20),
            Duration::from_secs(2),
            move |addr| async move {
                if addr == hanging_addr1 {
                    futures::future::pending().await
                } else {
                    net::TcpStream::connect(addr).await
                }
            },
        );

        let (accept_res, connect_res) = futures::future::join(accept_fut, connect_fut).await;
        accept_res?;
        let _stream = connect_res?;

        // All attempts hang past connect_timeout -> fails with TimedOut.
        let timeout_err = happy_eyeballs_connect_inner(
            vec![hanging_addr1, hanging_addr2],
            Duration::from_millis(10),
            Duration::from_millis(30),
            |_addr| futures::future::pending(),
        )
        .await
        .unwrap_err();
        assert_eq!(timeout_err.kind(), io::ErrorKind::TimedOut);
        Ok(())
    }

    #[test]
    fn test_interleave_addrs() {
        let v6_1: SocketAddr = "[::1]:80".parse().unwrap();
        let v6_2: SocketAddr = "[::2]:80".parse().unwrap();
        let v4_1: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let v4_2: SocketAddr = "127.0.0.2:80".parse().unwrap();

        let interleaved = interleave_addrs(vec![v6_1, v6_2, v4_1, v4_2]);
        assert_eq!(interleaved, vec![v6_1, v4_1, v6_2, v4_2]);

        let interleaved_v4_only = interleave_addrs(vec![v4_1, v4_2]);
        assert_eq!(interleaved_v4_only, vec![v4_1, v4_2]);

        let interleaved_v6_only = interleave_addrs(vec![v6_1, v6_2]);
        assert_eq!(interleaved_v6_only, vec![v6_1, v6_2]);

        let interleaved_dedup = interleave_addrs(vec![v6_1, v6_1, v4_1, v4_1]);
        assert_eq!(interleaved_dedup, vec![v6_1, v4_1]);
    }
}
