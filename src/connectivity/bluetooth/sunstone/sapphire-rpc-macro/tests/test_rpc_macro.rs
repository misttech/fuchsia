// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use sapphire_async::executor::BoundedExecutor;
use sapphire_async::rpc::{RpcCfg, RpcChannel};
use sapphire_async::testing::TestExecutor;
use sapphire_collections::storage::ArrayStorage;
use sapphire_rpc_macro::rpc;
use sapphire_sync::mutex::raw::SingleThreadMutex;

struct TestCfg;
impl RpcCfg for TestCfg {
    type Mtx = SingleThreadMutex;
    type Chan = ArrayStorage<10>;
}

struct MockService {
    add_count: usize,
    greet_count: usize,
}

impl MockService {
    fn new() -> Self {
        Self { add_count: 0, greet_count: 0 }
    }
}

#[rpc]
impl MockService {
    async fn add(&mut self, x: i32, y: i32) -> i32 {
        self.add_count += 1;
        x + y
    }

    async fn greet(&mut self, name: String) -> String {
        self.greet_count += 1;
        format!("Hello, {}", name)
    }

    // it can handle sync calls too
    fn ping(&self) {
        // unit return type, sync fn
    }

    pub async fn echo(&self, val: u32) -> u32 {
        val
    }
}

#[test]
fn test_rpc_macro_basic() {
    let mut channel = RpcChannel::<MockServiceRpc, TestCfg>::new();
    let (client_handle, server_handle) = channel.split();
    let client = MockServiceClient::new(client_handle);

    let mut server = MockService::new();

    BoundedExecutor::new(TestExecutor::new(), |s| {
        // Spawn server loop
        s.spawn(async {
            while let Ok((req, responder)) = server_handle.recv().await {
                server.route_request(req, responder).await;
            }
        });

        s.block_on(async {
            assert_eq!(client.add(10, 20).await.unwrap(), 30);
            assert_eq!(client.greet("Fuchsia".to_string()).await.unwrap(), "Hello, Fuchsia");
            client.ping().await.unwrap();
            assert_eq!(client.echo(42).await.unwrap(), 42);
        });
    });
}

struct ImmutableService;

#[rpc]
impl ImmutableService {
    fn new_instance() -> Self {
        Self
    }

    async fn get_data(&self, id: u32) -> Result<String, i32> {
        if id == 0 { Err(-1) } else { Ok(format!("data_{}", id)) }
    }
}

#[test]
fn test_rpc_macro_immutable_and_assoc_fn() {
    let mut channel = RpcChannel::<ImmutableServiceRpc, TestCfg>::new();
    let (client_handle, server_handle) = channel.split();
    let client = ImmutableServiceClient::new(client_handle);

    // Verify associated function works
    let server = ImmutableService::new_instance();

    BoundedExecutor::new(TestExecutor::new(), |s| {
        // Since the service is fully immutable, we can have multiple handlers concurrently.
        for _ in 0..4 {
            s.spawn(async {
                while let Ok((req, responder)) = server_handle.recv().await {
                    // Notice: server is immutable (&server), not &mut server
                    server.route_request(req, responder).await;
                }
            });
        }

        s.block_on(async {
            assert_eq!(client.get_data(10).await.unwrap(), Ok("data_10".to_string()));
            assert_eq!(client.get_data(0).await.unwrap(), Err(-1));
        });
    });
}
