// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::CqhciDriver;
use crate::command_queue::SubmittedTaskForTesting;
use block_client::{BlockClient, BufferSlice, MutableBufferSlice, RemoteBlockClient};
use fdf::WeakDispatcher;
use fdf_component::testing::harness::{DriverUnderTest, TestHarness};
use fdf_component::{ServiceInstance, ServiceOffer};
use fdf_fidl::{DriverChannel, FidlExecutor};
use fidl_fuchsia_hardware_block_volume::{self as fvolume};
use fidl_fuchsia_storage_block as fblock;
use fidl_next_fuchsia_hardware_cqhci::{self as cqhci, CqhciHostInfo, EmmcPartitionId};
use fidl_next_fuchsia_hardware_rpmb as rpmb;
use fidl_next_fuchsia_hardware_sdmmc::{self as sdmmc, SdmmcHostCap, SdmmcHostInfo};
use fidl_next_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::{FutureExt as _, StreamExt as _};
use mmio::Mmio;
use sdmmc_spec::{
    CQHCI_CQ_IS_OFFSET, CQHCI_CQ_TCN_OFFSET, CQHCI_CQ_TDBR_OFFSET,
    CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, CqhciCqInterruptStatusRegister, Direction,
    EXT_CSD_BARRIER_SUPPORT, EXT_CSD_BARRIER_SUPPORT_MASK, EXT_CSD_CACHE_CTRL,
    EXT_CSD_CACHE_EN_MASK, SDHCI_IS_OFFSET, SdhciInterruptStatusRegister,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zx::HandleBased as _;

struct MockRpmbServer {
    request_callback: Option<mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>>,
}

impl MockRpmbServer {
    fn new(
        request_callback: Option<mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>>,
    ) -> Self {
        Self { request_callback }
    }
}

impl rpmb::DriverRpmbServerHandler for MockRpmbServer {
    async fn get_device_info(
        &mut self,
        responder: fidl_next::Responder<rpmb::driver_rpmb::GetDeviceInfo, DriverChannel>,
    ) {
        responder
            .respond(rpmb::natural::DeviceInfo::EmmcInfo(rpmb::natural::EmmcDeviceInfo {
                cid: [0; 16],
                rpmb_size: 0,
                reliable_write_sector_count: 0,
            }))
            .await
            .ok();
    }

    async fn request(
        &mut self,
        _request: fidl_next::Request<rpmb::driver_rpmb::Request, DriverChannel>,
        responder: fidl_next::Responder<rpmb::driver_rpmb::Request, DriverChannel>,
    ) {
        if let Some(ref mut cb) = self.request_callback {
            let (tx, rx) = futures::channel::oneshot::channel();
            let _ = cb.unbounded_send(tx);
            let _ = rx.await;
        }
        responder.respond(()).await.ok();
    }
}

struct MockCqhciServer {
    mmio_vmo: zx::Vmo,
    sdhci_mmio_vmo: zx::Vmo,
    on_non_cq_interrupt_sink: mpsc::UnboundedSender<Arc<zx::VirtualInterrupt>>,
    hardware_irq: zx::VirtualInterrupt,
    fake_bti: zx::Bti,
}

impl MockCqhciServer {
    fn new(
        mmio_vmo: zx::Vmo,
        sdhci_mmio_vmo: zx::Vmo,
        on_non_cq_interrupt_sink: mpsc::UnboundedSender<Arc<zx::VirtualInterrupt>>,
        hardware_irq: zx::VirtualInterrupt,
    ) -> Self {
        let bti = fake_bti::FakeBti::create().unwrap();

        MockCqhciServer {
            mmio_vmo,
            sdhci_mmio_vmo,
            fake_bti: bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            on_non_cq_interrupt_sink,
            hardware_irq,
        }
    }
}

impl cqhci::CqhciServerHandler for MockCqhciServer {
    async fn host_info(&mut self, responder: fidl_next::Responder<cqhci::cqhci::HostInfo>) {
        let mut ext_csd = vec![0; 512];
        // Advertise all features, which make the driver take more interesting paths.
        ext_csd[EXT_CSD_CACHE_CTRL] = EXT_CSD_CACHE_EN_MASK;
        ext_csd[EXT_CSD_BARRIER_SUPPORT] = EXT_CSD_BARRIER_SUPPORT_MASK;
        let info = {
            CqhciHostInfo {
                sdmmc_host_info: SdmmcHostInfo {
                    max_transfer_size: u32::MAX,
                    caps: SdmmcHostCap::empty(),
                    max_buffer_regions: 128,
                },
                partitions: vec![
                    cqhci::EmmcPartition {
                        id: EmmcPartitionId::UserDataPartition,
                        block_count: 1024,
                        block_size: 512,
                    },
                    cqhci::EmmcPartition {
                        id: EmmcPartitionId::BootPartition1,
                        block_count: 1024,
                        block_size: 512,
                    },
                ],
                rca: 1,
                ext_csd,
            }
        };
        responder.respond(&info).await.ok();
    }

    async fn initialize_command_queueing(
        &mut self,
        request: fidl_next::Request<cqhci::cqhci::InitializeCommandQueueing, DriverChannel>,
        responder: fidl_next::Responder<cqhci::cqhci::InitializeCommandQueueing>,
    ) {
        let payload = request.payload();

        let virtual_interrupt = payload.virtual_interrupt;
        let hardware_irq = Arc::new(zx::VirtualInterrupt::from(
            self.hardware_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into_handle(),
        ));
        let on_non_cq_interrupt_sink = self.on_non_cq_interrupt_sink.clone();

        std::thread::spawn(move || {
            loop {
                match virtual_interrupt.wait() {
                    Ok(_) => {
                        on_non_cq_interrupt_sink.unbounded_send(hardware_irq.clone()).unwrap();
                    }
                    Err(zx::Status::CANCELED) => break,
                    Err(e) => panic!("wait failed: {:?}", e),
                }
            }
        });

        let mmio = self.mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let sdhci_mmio = self.sdhci_mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let bti = self.fake_bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let interrupt = zx::Interrupt::from(
            self.hardware_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into_handle(),
        );

        responder
            .respond(sdmmc::CqhciInitializeCommandQueueingResponse {
                cqhci_mmio: mmio,
                cqhci_mmio_offset: 0,
                sdhci_mmio,
                sdhci_mmio_offset: 0,
                bti,
                interrupt,
            })
            .await
            .ok();
    }

    async fn enable_cqhci(&mut self, responder: fidl_next::Responder<cqhci::cqhci::EnableCqhci>) {
        responder.respond(()).await.ok();
    }

    async fn disable_cqhci(&mut self, responder: fidl_next::Responder<cqhci::cqhci::DisableCqhci>) {
        let mut cqhci_mmio = mmio::vmo::VmoMapping::map(
            0,
            0x2000,
            self.mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();
        cqhci_mmio.store32(CQHCI_CQ_IS_OFFSET, 0);
        cqhci_mmio.store32(CQHCI_CQ_TCN_OFFSET, 0);

        let mut sdhci_mmio = mmio::vmo::VmoMapping::map(
            0,
            0x2000,
            self.sdhci_mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();
        sdhci_mmio.store32(SDHCI_IS_OFFSET, 0);

        responder.respond(()).await.ok();
    }
}

struct Service {
    dispatcher: FidlExecutor<WeakDispatcher>,
    mmio_vmo: zx::Vmo,
    sdhci_mmio_vmo: zx::Vmo,
    on_non_cq_interrupt_sink: mpsc::UnboundedSender<Arc<zx::VirtualInterrupt>>,
    hardware_irq: zx::VirtualInterrupt,
    rpmb_request_callback: Option<mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>>,
}

impl cqhci::ServiceHandler for Service {
    fn cqhci(
        &self,
        server_end: fidl_next::ServerEnd<fidl_next_fuchsia_hardware_cqhci::Cqhci, DriverChannel>,
    ) {
        server_end.spawn_on(
            MockCqhciServer::new(
                self.mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                self.sdhci_mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                self.on_non_cq_interrupt_sink.clone(),
                self.hardware_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            ),
            &self.dispatcher,
        );
    }

    fn rpmb(&self, server_end: fidl_next::ServerEnd<fidl_next_fuchsia_hardware_rpmb::DriverRpmb>) {
        server_end
            .spawn_on(MockRpmbServer::new(self.rpmb_request_callback.clone()), &self.dispatcher);
    }
}

struct TestHandles {
    mmio: zx::Vmo,
    sdhci_mmio: zx::Vmo,
    hardware_irq: zx::VirtualInterrupt,
    /// A receiver for non-CQ interrupts.  Each time the driver receives a non-CQ interrupt, it
    /// will forward it to the delegator, which in turn forwards the physical IRQ to this receiver
    /// to be acked by the test.  Note that the test MUST ack the interrupt, or the driver will
    /// never resume handling interrupts.
    on_non_cq_interrupt_receiver: mpsc::UnboundedReceiver<Arc<zx::VirtualInterrupt>>,
    /// A receiver for RPMB requests.  Each time the driver receives an RPMB request, it will send
    /// a message to this receiver, and will wait for the test to send a message back before
    /// continuing.
    rpmb_request_receiver: mpsc::UnboundedReceiver<futures::channel::oneshot::Sender<()>>,
}

struct FakeTaskHandlerInner {
    mmio: mmio::region::MmioRegion<mmio::vmo::VmoMemory>,
    sdhci_mmio: mmio::region::MmioRegion<mmio::vmo::VmoMemory>,
    requests_to_fail: usize,
    stall: bool,
}

struct FakeTaskHandler {
    inner: Mutex<FakeTaskHandlerInner>,
    irq: zx::VirtualInterrupt,
    // A side-channel which receives all tasks submitted by the driver.
    // We need this because the doorbell register has W1S semantics in the real world, which we
    // cannot easily simulate.  Races can occur where two simultaneously submitted tasks erase
    // each other's slots in the doorbell register upon submission.
    doorbell_receiver: Mutex<std::sync::mpsc::Receiver<u8>>,
}

impl FakeTaskHandler {
    fn new(handles: &TestHandles, doorbell_receiver: std::sync::mpsc::Receiver<u8>) -> Arc<Self> {
        let mmio = mmio::vmo::VmoMapping::map(
            0,
            0x2000,
            handles.mmio.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();
        let sdhci_mmio = mmio::vmo::VmoMapping::map(
            0,
            0x2000,
            handles.sdhci_mmio.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();
        Arc::new(Self {
            inner: Mutex::new(FakeTaskHandlerInner {
                mmio,
                sdhci_mmio,
                requests_to_fail: 0,
                stall: false,
            }),
            irq: handles.hardware_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            doorbell_receiver: Mutex::new(doorbell_receiver),
        })
    }

    fn fail_next(&self, count: usize) {
        self.inner.lock().requests_to_fail = count;
    }

    fn stall(&self, stall: bool) {
        self.inner.lock().stall = stall;
    }

    fn spawn(self: &Arc<Self>, scope: &fasync::Scope) -> fasync::JoinHandle<()> {
        let this = self.clone();
        scope.spawn(async move {
            loop {
                this.poll();
                fasync::Timer::new(std::time::Duration::from_millis(1)).await;
            }
        })
    }

    fn poll(&self) {
        {
            let mut inner = self.inner.lock();
            let mut doorbell = inner.mmio.load32(CQHCI_CQ_TDBR_OFFSET as usize);

            // Also check the side-channel for any doorbells that might have been overwritten/lost due
            // to lack of W1S semantics on the VMO.
            let doorbell_receiver = self.doorbell_receiver.lock();
            while let Ok(slot) = doorbell_receiver.try_recv() {
                doorbell |= 1 << slot;
            }

            if inner.stall || doorbell == 0 {
                return;
            }

            // Only clear the bits we are about to process.  If another thread writes to TDBR
            // *after* we read it but *before* we write it back, we don't want to clobber that
            // write.
            let current_tdbr = inner.mmio.load32(CQHCI_CQ_TDBR_OFFSET as usize);
            inner.mmio.store32(CQHCI_CQ_TDBR_OFFSET, current_tdbr & !doorbell);

            let mut sdhci_irq_status =
                SdhciInterruptStatusRegister::from_raw(inner.sdhci_mmio.load32(SDHCI_IS_OFFSET));
            let mut cqhci_irq_status = CqhciCqInterruptStatusRegister::from_raw(
                inner.mmio.load32(CQHCI_CQ_IS_OFFSET as usize),
            );
            if inner.requests_to_fail > 0 {
                inner.requests_to_fail -= 1;
                sdhci_irq_status.set_cqhci_interrupt(true);
                cqhci_irq_status.set_response_error_detected(true);
            } else {
                sdhci_irq_status.set_cqhci_interrupt(true);
                cqhci_irq_status.set_task_complete(true);
                // Accumulate completions in TCN (don't overwrite existing ones)
                let current_tcn = inner.mmio.load32(CQHCI_CQ_TCN_OFFSET);
                inner.mmio.store32(CQHCI_CQ_TCN_OFFSET, current_tcn | doorbell);
            }
            inner.mmio.store32(CQHCI_CQ_IS_OFFSET, cqhci_irq_status.raw());
            inner.sdhci_mmio.store32(SDHCI_IS_OFFSET, sdhci_irq_status.raw());
            inner.mmio.write_barrier();
        }

        self.irq.trigger(zx::BootInstant::get()).expect("failed to trigger interrupt");
    }
}

fn setup() -> (TestHarness<CqhciDriver>, fasync::Scope, TestHandles) {
    let mmio_vmo = zx::Vmo::create(0x2000).unwrap();
    let sdhci_mmio_vmo = zx::Vmo::create(0x2000).unwrap();
    let hardware_irq = zx::Interrupt::create_virtual().unwrap();
    let (on_non_cq_interrupt_sink, on_non_cq_interrupt_receiver) = mpsc::unbounded();
    let (rpmb_tx, rpmb_rx) = mpsc::unbounded();
    let scope = fasync::Scope::new_with_name("test");
    let mut service_fs = ServiceFs::new();
    let mut harness = TestHarness::<CqhciDriver>::new();

    let test_handles = TestHandles {
        mmio: mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        sdhci_mmio: sdhci_mmio_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        hardware_irq: hardware_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        on_non_cq_interrupt_receiver,
        rpmb_request_receiver: rpmb_rx,
    };

    let offers = [ServiceOffer::<cqhci::Service>::new_next()
        .add_named_next(
            &mut service_fs,
            "default",
            Service {
                dispatcher: FidlExecutor::from(harness.dispatcher().clone()),
                mmio_vmo,
                sdhci_mmio_vmo,
                on_non_cq_interrupt_sink,
                hardware_irq,
                rpmb_request_callback: Some(rpmb_tx),
            },
        )
        .build_driver_offer()];
    for offer in offers {
        harness = harness.add_offer(offer);
    }
    harness = harness.set_driver_incoming(service_fs);

    (harness, scope, test_handles)
}

fn connect_block_proxy(
    started_driver: &DriverUnderTest<'_, CqhciDriver>,
    partition_name: &str,
) -> fblock::BlockProxy {
    let incoming = started_driver.driver_outgoing();
    let service = incoming
        .service::<fvolume::ServiceProxy>()
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

fn connect_rpmb_client(
    started_driver: &DriverUnderTest<'_, CqhciDriver>,
    scope: &fasync::Scope,
) -> fidl_next::Client<rpmb::Rpmb> {
    let rpmb_service: ServiceInstance<rpmb::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fidl_next::fuchsia::create_channel();
    rpmb_service.device(server_end).unwrap();
    client_end.spawn_on(scope)
}

async fn start_driver<'a>(
    harness: &'a mut TestHarness<CqhciDriver>,
    scope: &fasync::Scope,
    handles: &TestHandles,
    additional_hook: Option<Box<dyn Fn(SubmittedTaskForTesting<'_>) + Send + Sync>>,
) -> (DriverUnderTest<'a, CqhciDriver>, Arc<FakeTaskHandler>) {
    let (doorbell_sender, doorbell_receiver) = std::sync::mpsc::channel();
    let doorbell_sender = Arc::new(Mutex::new(doorbell_sender));

    let task_handler = FakeTaskHandler::new(handles, doorbell_receiver);
    task_handler.spawn(scope);

    let started_driver = harness.start_driver().await.expect("failed to start driver");

    started_driver
        .get_driver()
        .unwrap()
        .command_queue
        .lock()
        .as_ref()
        .unwrap()
        .set_task_submitted_hook(Box::new(move |transfer_op| {
            match &transfer_op {
                SubmittedTaskForTesting::Transfer(_, transfer) => {
                    let _ = doorbell_sender.lock().send(transfer.tdl_slot());
                }
                SubmittedTaskForTesting::DirectCmd => {
                    let _ = doorbell_sender.lock().send(CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT);
                }
            }
            if let Some(hook) = &additional_hook {
                hook(transfer_op);
            }
        }));

    (started_driver, task_handler)
}

#[fuchsia::test]
async fn test_driver_lifecycle() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, _task_handler) = start_driver(&mut harness, &scope, &handles, None).await;
    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_io() {
    let (mut harness, scope, handles) = setup();
    let data = Arc::new(Mutex::new(vec![0u8; 65536]));
    let data_clone = data.clone();
    let (started_driver, _task_handler) = start_driver(
        &mut harness,
        &scope,
        &handles,
        Some(Box::new(move |transfer_op| {
            let SubmittedTaskForTesting::Transfer(partition, transfer) = transfer_op else {
                return;
            };
            assert_eq!(partition, EmmcPartitionId::UserDataPartition);
            let mut data = data_clone.lock();
            assert!(transfer.offset() + transfer.length() <= data.len() as u64);
            let range =
                transfer.offset() as usize..transfer.offset() as usize + transfer.length() as usize;
            match transfer.direction() {
                Direction::Read => {
                    transfer
                        .vmo()
                        .write(&data[range], transfer.vmo_offset())
                        .expect("vmo write failed");
                }
                Direction::Write => {
                    transfer
                        .vmo()
                        .read(&mut data[range], transfer.vmo_offset())
                        .expect("vmo read failed");
                }
            }
        })),
    )
    .await;

    let block_client = connect_block_client(&started_driver, "user").await;

    let mut buf = vec![11u8; 512];
    block_client.write_at(BufferSlice::from(&buf[..]), 0).await.expect("write failed");
    buf.fill(0);
    block_client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await.expect("read failed");
    assert_eq!(buf, vec![11u8; 512]);

    // Flush takes a separate path.  Just make sure it succeeds.
    block_client.flush().await.expect("flush failed");

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_switch_partition() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, _task_handler) = start_driver(
        &mut harness,
        &scope,
        &handles,
        Some(Box::new(move |transfer_op| {
            let (partition, transfer) = match transfer_op {
                SubmittedTaskForTesting::Transfer(partition, transfer) => (partition, transfer),
                SubmittedTaskForTesting::DirectCmd => return,
            };
            let pattern = match partition {
                EmmcPartitionId::UserDataPartition => 0x11,
                EmmcPartitionId::BootPartition1 => 0x22,
                _ => panic!("Unexpected partition"),
            };
            let len = transfer.length();
            let buf = vec![pattern; len as usize];
            transfer.vmo().write(&buf, transfer.vmo_offset()).expect("vmo write failed");
        })),
    )
    .await;

    let user_client = connect_block_client(&started_driver, "user").await;
    let boot1_client = connect_block_client(&started_driver, "boot1").await;

    // Concurrently read from both partitions, and ensure the streams don't get crossed.  The
    // driver is responsible for blocking requests to one partition whilst the other is active,
    // so the requests should proceed serially (in some arbitrary interleaving).
    let make_requests = |client: RemoteBlockClient, pattern: u8| async move {
        for _ in 0..10 {
            let mut buf = vec![0u8; 512];
            client.read_at(MutableBufferSlice::from(&mut buf[..]), 0).await.expect("read failed");
            assert_eq!(buf, vec![pattern; 512]);
        }
    };

    futures::join!(make_requests(user_client, 0x11), make_requests(boot1_client, 0x22));

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_forwards_non_cq_interrupts() {
    let (mut harness, scope, mut handles) = setup();
    let (started_driver, _task_handler) = start_driver(&mut harness, &scope, &handles, None).await;

    // Generate a non-CQ interrupt
    let mut test_mmio = mmio::vmo::VmoMapping::map(
        0,
        0x2000,
        handles.sdhci_mmio.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
    )
    .unwrap();
    let mut sdhci_interrupt_status = SdhciInterruptStatusRegister::from_raw(0);
    sdhci_interrupt_status.set_command_complete(true);
    test_mmio.store32(SDHCI_IS_OFFSET, sdhci_interrupt_status.raw());
    handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    // Wait until the virtual interrupt is given to the delegator.
    let irq = handles.on_non_cq_interrupt_receiver.next().await.expect("Failed to wait for irq");

    // Trigger again while it's not acked.  It should not be delivered to CQHCI, because the CQHCI
    // driver shouldn't have acked it.
    handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    let first_irq_acked = Arc::new(Mutex::new(false));
    let next_interrupt = handles.on_non_cq_interrupt_receiver.next().then(|res| async {
        assert!(*first_irq_acked.lock());
        res
    });

    // Now ack the virtual interrupt.  The second IRQ should be delivered.
    {
        let mut acked = first_irq_acked.lock();
        let _ = irq.ack();
        *acked = true;
    }
    let irq2 = next_interrupt.await.expect("Failed to wait for second IRQ");
    test_mmio.store32(SDHCI_IS_OFFSET, 0);
    irq2.ack().unwrap();

    started_driver.stop_driver().await;
}

// TODO(https://fxbug.dev/42176727): The test is disabled until flakiness is resolved.
#[ignore]
#[fuchsia::test]
async fn test_error_recovery() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, task_handler) = start_driver(&mut harness, &scope, &handles, None).await;

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
        scope.spawn(async move {
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
        scope.spawn(async move {
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

    // Fail a task.  This should eventually trigger recovery.
    // Once recovery starts, any in-flight tasks will be cancelled, and any new tasks will buffer
    // up, so we should see at most 2 errors (one per client).
    task_handler.fail_next(1);
    on_error_listener.await;

    // Run a bit longer to let a few more tasks through, verifying that they can successfully
    // run.
    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    state.lock().abort = true;
    reader.await;
    flusher.await;

    assert!(state.lock().num_errors <= 2);

    started_driver.stop_driver().await;
}

#[fuchsia::test(threads = 4)]
async fn test_concurrency() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, _task_handler) = start_driver(
        &mut harness,
        &scope,
        &handles,
        Some(Box::new(move |transfer_op| {
            if let SubmittedTaskForTesting::Transfer(partition, transfer) = transfer_op {
                let len = transfer.length();
                let pattern = match partition {
                    EmmcPartitionId::UserDataPartition => {
                        if len == 512 {
                            0x11
                        } else {
                            0x22
                        }
                    }
                    EmmcPartitionId::BootPartition1 => 0x33,
                    _ => panic!("Unexpected partition"),
                };
                let buf = vec![pattern; len as usize];
                transfer.vmo().write(&buf, transfer.vmo_offset()).expect("vmo write failed");
            }
        })),
    )
    .await;
    let started_driver = Mutex::new(Some(started_driver));

    let proxy0 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "user");
    let proxy1 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "user");
    let proxy2 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "user");
    let proxy3 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "boot1");

    let user_client1 = RemoteBlockClient::new(proxy0).await.expect("failed to create block client");
    let user_client2 = RemoteBlockClient::new(proxy1).await.expect("failed to create block client");
    let user_client3 = RemoteBlockClient::new(proxy2).await.expect("failed to create block client");
    let boot1_client = RemoteBlockClient::new(proxy3).await.expect("failed to create block client");

    let do_io = |client: RemoteBlockClient, pattern: Vec<u8>| async move {
        for i in 0..100 {
            let mut buf = pattern.clone();
            client
                .read_at(MutableBufferSlice::from(&mut buf[..]), i * 512)
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
        do_io(user_client1, vec![0x11; 512]),
        do_io(user_client2, vec![0x22; 1024]),
        do_flush(user_client3),
        do_io(boot1_client, vec![0x33; 512])
    );

    let driver = started_driver.lock().take().unwrap();
    driver.stop_driver().await;
}

#[fuchsia::test(threads = 4)]
async fn test_shutdown_with_active_clients() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, _task_handler) = start_driver(&mut harness, &scope, &handles, None).await;
    let started_driver = Mutex::new(Some(started_driver));

    let proxy0 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "user");
    let proxy1 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "user");
    let proxy2 = connect_block_proxy(started_driver.lock().as_ref().unwrap(), "boot1");

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

    let io_fut = scope.spawn(do_io(user_client0));
    let flush_fut = scope.spawn(do_flush(user_client1));
    let io_fut2 = scope.spawn(do_io(boot1_client));

    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    let driver = started_driver.lock().take().unwrap();
    let stop_fut = driver.stop_driver();
    futures::join!(stop_fut, io_fut, flush_fut, io_fut2,);
}

#[fuchsia::test]
async fn test_rpmb() {
    let (mut harness, scope, handles) = setup();

    let rpmb_active = Arc::new(AtomicBool::new(false));
    let rpmb_active_clone = rpmb_active.clone();

    let (started_driver, _task_handler) = start_driver(
        &mut harness,
        &scope,
        &handles,
        Some(Box::new(move |_| {
            assert!(
                !rpmb_active_clone.load(Ordering::Relaxed),
                "task submitted while RPMB request is active!"
            );
        })),
    )
    .await;
    let started_driver = Mutex::new(Some(started_driver));

    let block_client = RemoteBlockClient::new(connect_block_proxy(
        started_driver.lock().as_ref().unwrap(),
        "user",
    ))
    .await
    .expect("failed to create block client");

    let rpmb_client = connect_rpmb_client(started_driver.lock().as_ref().unwrap(), &scope);

    let mut request_receiver = handles.rpmb_request_receiver;
    let rpmb_svc_fut = scope.spawn(async move {
        while let Some(responder) = futures::StreamExt::next(&mut request_receiver).await {
            rpmb_active.store(true, Ordering::Relaxed);
            fasync::Timer::new(std::time::Duration::from_micros(500)).await;
            rpmb_active.store(false, Ordering::Relaxed);
            let _ = responder.send(());
        }
    });

    let read_fut = scope.spawn(async move {
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
    });

    let rpmb_fut = scope.spawn(async move {
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
    });

    futures::join!(read_fut, rpmb_fut);

    let driver = started_driver.lock().take().unwrap();
    driver.stop_driver().await;

    rpmb_svc_fut.abort().await;
}

#[fuchsia::test]
async fn test_shutdown_with_active_dcmd() {
    let (mut harness, scope, handles) = setup();

    // Intercept when the DCMD is submitted.
    let (dcmd_tx, dcmd_rx) = futures::channel::oneshot::channel();
    let dcmd_tx = Arc::new(Mutex::new(Some(dcmd_tx)));

    let (started_driver, task_handler) = start_driver(
        &mut harness,
        &scope,
        &handles,
        Some(Box::new(move |transfer_op| {
            if let SubmittedTaskForTesting::DirectCmd = transfer_op {
                if let Some(tx) = dcmd_tx.lock().take() {
                    let _ = tx.send(());
                }
            }
        })),
    )
    .await;

    let block_client0 = connect_block_client(&started_driver, "user").await;
    let block_client1 = connect_block_client(&started_driver, "user").await;

    // Stall requests so the requests never complete.
    task_handler.stall(true);

    let flush0_task = scope.spawn(async move {
        block_client0.flush().await.expect_err("flush should fail");
    });
    let flush1_task = scope.spawn(async move {
        block_client1.flush().await.expect_err("flush should fail");
    });

    // Stop the driver while a DCMD is pending.
    dcmd_rx.await.unwrap();
    started_driver.stop_driver().await;

    futures::join!(flush0_task, flush1_task);
}

#[fuchsia::test]
async fn test_shutdown_with_blocked_transfers() {
    let (mut harness, scope, handles) = setup();
    let (started_driver, task_handler) = start_driver(&mut harness, &scope, &handles, None).await;

    // Stall requests so the requests never complete.
    task_handler.stall(true);

    let mut tasks = vec![];
    for _ in 0..32 {
        // Queue up 32 requests, which is one more than the available number of slots.
        let block_client = connect_block_client(&started_driver, "user").await;
        tasks.push(scope.spawn(async move {
            let mut buf = vec![0x00; 512];
            block_client
                .read_at(MutableBufferSlice::from(&mut buf[..]), 0)
                .await
                .expect_err("read should (eventually) fail");
        }));
    }

    fasync::Timer::new(std::time::Duration::from_millis(100)).await;
    started_driver.stop_driver().await;

    futures::future::join_all(tasks).await;
}
