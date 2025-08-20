// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fuchsia_archive::AsyncUtf8Reader;
use fuchsia_async as fasync;
use fuchsia_fs::file::{AsyncGetSize, AsyncReadAt};
use futures::lock::Mutex;
use futures::{Future, FutureExt as _, TryStreamExt as _};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

async fn serve_file<T: AsyncReadAt + AsyncGetSize + Unpin + Send + 'static>(
    req: Request<Body>,
    far_reader: Arc<Mutex<AsyncUtf8Reader<T>>>,
) -> Response<Body> {
    let path = req.uri().path();
    let file_name = path.strip_prefix('/').unwrap_or(path);
    // TODO(https://fxbug.dev/439655098): Support streaming files.
    match far_reader.lock().await.read_file(file_name).await {
        Ok(file) => Response::new(Body::from(file)),
        Err(fuchsia_archive::Error::PathNotPresent(_)) => {
            Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap()
        }
        Err(e) => {
            log::error!("Failed to read file: {:#}", anyhow::anyhow!(e));
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        }
    }
}

pub fn start_server<T: AsyncReadAt + AsyncGetSize + Unpin + Send + 'static>(
    far_reader: AsyncUtf8Reader<T>,
) -> Result<
    (
        impl Future<Output = Result<(), hyper::Error>> + 'static,
        impl FnOnce() -> Arc<Mutex<AsyncUtf8Reader<T>>>,
        SocketAddr,
    ),
    Error,
> {
    let far_reader = Arc::new(Mutex::new(far_reader));
    let (close_sender, close_receiver) = futures::channel::oneshot::channel();
    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 0);
    let listener = fasync::net::TcpListener::bind(&addr)?;
    let local_addr = listener.local_addr()?;
    let server_fut = {
        let far_reader = Arc::clone(&far_reader);
        let server = Server::builder(hyper::server::accept::from_stream(
            listener
                .accept_stream()
                .map_ok(|(conn, _addr)| fuchsia_hyper::TcpStream { stream: conn }),
        ))
        .executor(fuchsia_hyper::Executor)
        .serve(make_service_fn(move |_| {
            let far_reader = Arc::clone(&far_reader);
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    serve_file(req, Arc::clone(&far_reader)).map(Ok::<_, Infallible>)
                }))
            }
        }));
        server.with_graceful_shutdown(async {
            let _ = close_receiver.await;
        })
    };
    let close_fn = move || {
        let _ = close_sender.send(());
        far_reader
    };
    Ok((server_fut, close_fn, local_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_fs::file::Adapter;
    use futures::io::Cursor;
    use std::collections::BTreeMap;
    use std::io::Read;

    #[fuchsia::test]
    async fn test_server() {
        let mut far_data = Vec::new();
        let file_content = "hello world";
        let files = BTreeMap::from([(
            "file1.txt",
            (file_content.len() as u64, Box::new(file_content.as_bytes()) as Box<dyn Read>),
        )]);
        fuchsia_archive::write(&mut far_data, files).unwrap();

        let reader = Adapter::new(Cursor::new(far_data));
        let far_reader = AsyncUtf8Reader::new(reader).await.unwrap();

        let (server_fut, close_fn, addr) = start_server(far_reader).unwrap();
        let server_task = fasync::Task::spawn(server_fut);

        let client = fuchsia_hyper::new_client();
        let uri = format!("http://{addr}/file1.txt");
        let response = client.get(uri.parse().unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(body.as_ref(), file_content.as_bytes());

        let uri = format!("http://{addr}/nonexistent.txt");
        let response = client.get(uri.parse().unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let far_reader = close_fn();
        server_task.await.unwrap();
        let _far_reader = Arc::into_inner(far_reader).unwrap().into_inner();
    }
}
