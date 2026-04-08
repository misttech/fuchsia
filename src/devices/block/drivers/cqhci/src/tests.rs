// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod fake_cqhci;

pub use fake_cqhci::TestCommandQueueHost;

use crate::CqhciDriver;
use block_client::{BlockClient, BufferSlice, MutableBufferSlice, RemoteBlockClient};
use fdf_component::testing::harness::DriverUnderTest;
use fidl_fuchsia_hardware_block_volume::{self as fvolume};
use fidl_fuchsia_storage_block as fblock;
use fidl_next_fuchsia_hardware_rpmb as rpmb;
use fidl_next_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::{FutureExt as _, StreamExt as _};
use sdmmc_spec::{CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, SdhciInterruptStatusRegister};
use std::pin::pin;
use std::sync::Arc;
use zx::HandleBased as _;

use fake_cqhci::{Blocker, FakeCqhci};

fn connect_block_proxy(
    started_driver: &DriverUnderTest<'_, CqhciDriver>,
    partition_name: &str,
) -> fblock::BlockProxy {
    let incoming = started_driver.driver_outgoing();
    let service = incoming
        .service::<fidl_fuchsia_hardware_block_volume::ServiceProxy>()
        .instance(partition_name)
        .connect()
        .expect("failed to connect to service");

    service.connect_to_volume().expect("failed to connect to volume")
}

async fn connect_block_client(
    started_driver: &DriverUnderTest<'_, CqhciDriver>,
    partition_name: &str,
) -> RemoteBlockClient {
    RemoteBlockClient::new(connect_block_proxy(started_driver, partition_name))
        .await
        .expect("failed to create block client")
}

fn connect_inline_encryption_proxy(
    started_driver: &DriverUnderTest<'_, CqhciDriver>,
    partition_name: &str,
) -> fidl_fuchsia_hardware_inlineencryption::DeviceProxy {
    let incoming = started_driver.driver_outgoing();
    let service = incoming
        .service::<fvolume::ServiceProxy>()
        .instance(partition_name)
        .connect()
        .expect("failed to connect to service");

    service.connect_to_inline_encryption().expect("failed to connect to inline encryption")
}
#[fuchsia::test]
async fn test_driver_lifecycle() {
    let (_fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_io() {
    let (_fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    let block_client = connect_block_client(&started_driver, "user").await;

    let buf = vec![11u8; 512];
    block_client.write_at(BufferSlice::from(&buf[..]), 0).await.expect("write failed");

    let mut read_buf = vec![0u8; 512];
    block_client
        .read_at(MutableBufferSlice::from(&mut read_buf[..]), 0)
        .await
        .expect("read failed");
    assert_eq!(read_buf, buf);

    block_client.flush().await.expect("flush failed");
    block_client.trim(0..1024).await.expect("trim failed");

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_switch_partition() {
    let (_fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    let user_client = connect_block_client(&started_driver, "user").await;
    let boot1_client = connect_block_client(&started_driver, "boot1").await;

    let user_pattern = vec![0xaa; 512];
    let boot1_pattern = vec![0xbb; 512];

    user_client.write_at(BufferSlice::from(&user_pattern[..]), 0).await.expect("user write failed");
    boot1_client
        .write_at(BufferSlice::from(&boot1_pattern[..]), 0)
        .await
        .expect("boot1 write failed");

    let user_fut = async {
        for _ in 0..10 {
            let mut buf = vec![0u8; 512];
            user_client
                .read_at(MutableBufferSlice::from(&mut buf[..]), 0)
                .await
                .expect("user read failed");
            assert_eq!(buf, user_pattern);
        }
    };

    let boot1_fut = async {
        for _ in 0..10 {
            let mut buf = vec![0u8; 512];
            boot1_client
                .read_at(MutableBufferSlice::from(&mut buf[..]), 0)
                .await
                .expect("boot1 read failed");
            assert_eq!(buf, boot1_pattern);
        }
    };

    futures::join!(user_fut, boot1_fut);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_forwards_non_cq_interrupts() {
    let (mut fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    // Generate a non-CQ interrupt
    fixture.set_sdhci_interrupt_status({
        let mut sdhci_interrupt_status = SdhciInterruptStatusRegister::from_raw(0);
        sdhci_interrupt_status.set_command_complete(true);
        sdhci_interrupt_status
    });
    fixture.handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    // Wait until the first non-CQ interrupt is delivered to the delegator.
    let irq =
        fixture.handles.non_cq_interrupt_receiver.next().await.expect("Failed to wait for irq");

    // Trigger again while it's not acked.
    fixture.handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    // Give time for it to be processed.
    fasync::Timer::new(std::time::Duration::from_millis(10)).await;

    // No new interrupt should have been triggered because we haven't acked the first one.
    assert!(fixture.handles.non_cq_interrupt_receiver.next().now_or_never().is_none());

    // Now ack the virtual interrupt.  The second IRQ should be delivered.
    let _ = irq.ack();

    let irq2 =
        fixture.handles.non_cq_interrupt_receiver.next().await.expect("Failed to wait for irq");

    fixture.set_sdhci_interrupt_status(SdhciInterruptStatusRegister::from_raw(0));
    let _ = irq2.ack();

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_error_recovery() {
    let (fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    struct TestState {
        on_error_event: event_listener::Event,
        num_errors: usize,
        abort: bool,
    }
    let state = Arc::new(Mutex::new(TestState {
        on_error_event: event_listener::Event::new(),
        num_errors: 0,
        abort: false,
    }));
    let on_error_listener = state.lock().on_error_event.listen();

    // Start two tasks to continuously submit requests.
    let reader = {
        let block_client = connect_block_client(&started_driver, "user").await;
        let state = state.clone();
        fixture.scope.spawn(async move {
            loop {
                let mut buf = vec![0u8; 512];
                let result = block_client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await;
                let mut state = state.lock();
                if result.is_err() {
                    state.num_errors += 1;
                    state.on_error_event.notify(usize::MAX);
                }
                if state.abort {
                    break;
                }
            }
        })
    };
    let flusher = {
        let block_client = connect_block_client(&started_driver, "user").await;
        let state = state.clone();
        fixture.scope.spawn(async move {
            loop {
                let result = block_client.flush().await;
                let mut state = state.lock();
                if result.is_err() {
                    state.num_errors += 1;
                    state.on_error_event.notify(usize::MAX);
                }
                if state.abort {
                    break;
                }
            }
        })
    };

    // Fail a task.
    fixture.fail_next(1);
    on_error_listener.await;
    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    state.lock().abort = true;
    reader.await;
    flusher.await;

    assert!(state.lock().num_errors <= 2);

    started_driver.stop_driver().await;
}

#[fuchsia::test(threads = 4)]
async fn test_concurrency() {
    let (_fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let started_driver = Mutex::new(Some(started_driver));

    let proxy0 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "user");
    let proxy1 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "user");
    let proxy2 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "user");
    let proxy3 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "boot1");

    let user_client1 = RemoteBlockClient::new(proxy0).await.expect("failed to create block client");
    let user_client2 = RemoteBlockClient::new(proxy1).await.expect("failed to create block client");
    let user_client3 = RemoteBlockClient::new(proxy2).await.expect("failed to create block client");
    let boot1_client = RemoteBlockClient::new(proxy3).await.expect("failed to create block client");

    let mut pattern = vec![0x11; 512];
    pattern.extend(vec![0x22; 1024]);

    for i in 0..100 {
        user_client1
            .write_at(BufferSlice::from(&pattern[..]), i * 1536)
            .await
            .expect("read failed");
    }

    let pattern = vec![0x33; 512];
    for i in 0..100 {
        boot1_client
            .write_at(BufferSlice::from(&pattern[..]), i * 1536)
            .await
            .expect("read failed");
    }

    let do_io = |client: RemoteBlockClient, pattern: Vec<u8>, offset: u64| async move {
        for i in 0..100 {
            let mut buf = vec![0x00; pattern.len()];
            client
                .read_at(MutableBufferSlice::from(&mut buf[..]), offset + i * 1536)
                .await
                .expect("read failed");
            assert_eq!(buf, pattern);
        }
    };
    let do_flush = |client: RemoteBlockClient| async move {
        for _ in 0..50 {
            client.flush().await.expect("flush failed");
        }
    };

    futures::join!(
        do_io(user_client1, vec![0x11; 512], 0),
        do_io(user_client2, vec![0x22; 1024], 512),
        do_flush(user_client3),
        do_io(boot1_client, vec![0x33; 512], 0),
    );

    let driver = started_driver.lock().take().unwrap();
    driver.stop_driver().await;
}

#[fuchsia::test(threads = 4)]
async fn test_shutdown_with_active_clients() {
    let (fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let started_driver = Mutex::new(Some(started_driver));

    let proxy0 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "user");
    let proxy1 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "user");
    let proxy2 = connect_block_proxy((*started_driver.lock()).as_ref().unwrap(), "boot1");

    let user_client0 = RemoteBlockClient::new(proxy0).await.expect("failed to create block client");
    let user_client1 = RemoteBlockClient::new(proxy1).await.expect("failed to create block client");
    let boot1_client = RemoteBlockClient::new(proxy2).await.expect("failed to create block client");

    let do_io = |client: RemoteBlockClient| async move {
        for _ in 0..100 {
            let mut buf = vec![0x00; 512];
            let _ = client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await;
        }
    };
    let do_flush = |client: RemoteBlockClient| async move {
        let _ = client.flush().await;
    };

    let io_fut = fixture.scope.spawn(do_io(user_client0));
    let flush_fut = fixture.scope.spawn(do_flush(user_client1));
    let io_fut2 = fixture.scope.spawn(do_io(boot1_client));

    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    let driver = started_driver.lock().take().unwrap();
    let stop_fut = driver.stop_driver();
    futures::join!(stop_fut, io_fut, flush_fut, io_fut2,);
}

#[fuchsia::test]
async fn test_rpmb() {
    fn connect_rpmb_client(
        started_driver: &DriverUnderTest<'_, CqhciDriver>,
        scope: &fasync::Scope,
    ) -> fidl_next::Client<rpmb::Rpmb> {
        let rpmb_service: fdf_component::ServiceInstance<rpmb::Service> =
            started_driver.driver_outgoing().service().connect_next().unwrap();

        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        rpmb_service.device(server_end).unwrap();
        client_end.spawn_on(scope)
    }

    let (mut fixture, mut harness) = FakeCqhci::new(None);

    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let started_driver = Mutex::new(Some(started_driver));

    let block_client = RemoteBlockClient::new(connect_block_proxy(
        started_driver.lock().as_ref().unwrap(),
        "user",
    ))
    .await
    .expect("failed to create block client");

    let rpmb_client = connect_rpmb_client(started_driver.lock().as_ref().unwrap(), &fixture.scope);

    let rpmb_svc_fut = async {
        while let Some(responder) = fixture.handles.rpmb_request_receiver.next().await {
            assert_eq!(fixture.in_progress_tasks(), 0);
            let completed_tasks = fixture.completed_tasks();
            fasync::Timer::new(std::time::Duration::from_micros(500)).await;
            assert_eq!(fixture.in_progress_tasks(), 0);
            assert_eq!(fixture.completed_tasks(), completed_tasks);
            let _ = responder.send(());
        }
    };

    let read_fut = async move {
        for i in 0..100 {
            let mut buf = vec![0x00; 512];
            block_client
                .read_at(MutableBufferSlice::from(&mut buf[..]), 0)
                .await
                .expect("read failed");
            if i % 10 == 0 {
                block_client.flush().await.expect("flush failed");
            }
        }
    };

    let rpmb_fut = async move {
        let vmo = zx::Vmo::create(1024).expect("failed to create vmo");
        for _ in 0..100 {
            let request = rpmb::Request {
                tx_frames: fmem::Range {
                    vmo: vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                    offset: 0,
                    size: 0,
                },
                rx_frames: None,
            };
            rpmb_client.request(request).await.expect("FIDL error").expect("rpmb request failed");
            // Do a brief sleep to avoid completely starving the reader.  This increases the
            // chances of interesting interleavings.
            fasync::Timer::new(std::time::Duration::from_micros(500)).await;
        }
    };

    let mut read_futs = pin!(futures::future::join(read_fut, rpmb_fut).fuse());
    let mut rpmb_svc_fut = pin!(rpmb_svc_fut.fuse());
    futures::select! {
        _ = read_futs => {}
        _ = rpmb_svc_fut => unreachable!(),
    }

    let driver = started_driver.lock().take().unwrap();
    driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_shutdown_with_active_dcmd() {
    let mut blocker = Blocker::default();
    let (fixture, mut harness) = FakeCqhci::new(Some(blocker.hook()));
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    let block_client0 = connect_block_client(&started_driver, "user").await;
    let block_client1 = connect_block_client(&started_driver, "user").await;

    blocker.block(1 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT);

    let flush0_task = fixture.scope.spawn(async move {
        // The first flush might or might not succeed.
        let _ = block_client0.flush().await;
    });

    // Wait for the DCMD to start.
    let unblock = blocker.next().await;

    // Issue another flush which should fail.
    let flush1_task = fixture.scope.spawn(async move {
        block_client1.flush().await.expect_err("flush should fail");
    });

    // Make sure no future DCMDs block.
    blocker.block(0);

    // Unblock the DCMD in 10ms.
    let _unblock_task = fixture.scope.spawn(async move {
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
        drop(unblock);
    });

    started_driver.stop_driver().await;

    futures::join!(flush0_task, flush1_task);
}

#[fuchsia::test]
async fn test_shutdown_with_blocked_transfers() {
    let blocker = Blocker::default();
    let (fixture, mut harness) = FakeCqhci::new(Some(blocker.hook()));
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    // Issue one read to switch the partition.
    let block_client = connect_block_client(&started_driver, "user").await;
    let mut buf = vec![0x00; 512];
    let _ = block_client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await;

    // Block all requests.
    blocker.block(!(1 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT));

    let read_scope = fixture.scope.new_child();
    for _ in 0..31 {
        let block_client = connect_block_client(&started_driver, "user").await;
        read_scope.spawn(async move {
            let mut buf = vec![0x00; 512];
            let _ = block_client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await;
        });
    }

    // Wait for allrequests to be enqueued and submitted to the hardware.
    let unblock: Vec<_> = blocker.take(31).collect().await;

    started_driver.stop_driver().await;

    drop(unblock);

    read_scope.await;
}

#[fuchsia::test]
async fn test_flush_while_queue_not_empty() {
    let mut blocker = Blocker::default();
    let (fixture, mut harness) = FakeCqhci::new(Some(blocker.hook()));
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    let block_client = connect_block_client(&started_driver, "user").await;

    // Perform a dummy read to ensure the partition is switched and avoid DCMDs during the test.
    {
        let mut buf = vec![0u8; 512];
        block_client
            .read_at(MutableBufferSlice::from(&mut buf[..]), 0)
            .await
            .expect("placeholder read failed");
    }

    // Now enable blocking for everything except dcmds for the actual test.
    blocker.block(!(1 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT));

    // Submit a read request.
    let read_fut = fixture.scope.spawn(async move {
        let mut buf = vec![0u8; 512];
        block_client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await.expect("read failed");
    });

    // Wait for the transfer to be submitted to hardware.
    let unblock = blocker.next().await;

    // Unblock all future requests.
    blocker.block(0);

    // Submit a flush request.
    let block_client_clone = connect_block_client(&started_driver, "user").await;
    let mut flush_fut = fixture.scope.spawn(async move {
        block_client_clone.flush().await.expect("flush failed");
    });

    // The flush should be blocked and not yet submitted as a DCMD.  We wait a bit to give it a
    // chance to be submitted if it were not blocked.
    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    // Check if anything else was submitted (it shouldn't be).
    assert!(blocker.next().now_or_never().is_none());

    // And double check the flush request didn't finish.
    assert!((&mut flush_fut).now_or_never().is_none());

    // Unblock the read request.
    drop(unblock);

    futures::join!(read_fut, flush_fut);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_inline_crypto() {
    let (_fixture, mut harness) = FakeCqhci::new(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");

    let proxy = connect_inline_encryption_proxy(&started_driver, "user");

    let response = proxy
        .program_key(b"my-wrapped-key", 4096)
        .await
        .expect("FIDL error")
        .expect("program_key failed");
    assert_eq!(response, 5);

    let response = proxy
        .derive_raw_secret(b"my-wrapped-key")
        .await
        .expect("FIDL error")
        .expect("derive_raw_secret failed");
    assert_eq!(response, vec![1, 2, 3, 4]);

    started_driver.stop_driver().await;
}
