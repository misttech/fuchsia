// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::CqhciDriver;
use crate::command_queue::{CommandQueueHost, CommandQueueResources, SubmittedTaskForTesting};
use async_trait::async_trait;
use block_client::{BlockClient, BufferSlice, MutableBufferSlice, RemoteBlockClient};
use fdf::WeakDispatcher;
use fdf_component::testing::harness::{DriverUnderTest, TestHarness};
use fdf_component::{ServiceInstance, ServiceOffer};
use fdf_fidl::{DriverChannel, FidlExecutor};
use fidl_fuchsia_hardware_block_volume::{self as fvolume};
use fidl_fuchsia_storage_block as fblock;
use fidl_next_fuchsia_hardware_cqhci::{self as cqhci, CqhciHostInfo, EmmcPartitionId};
use fidl_next_fuchsia_hardware_rpmb as rpmb;
use fidl_next_fuchsia_hardware_sdmmc::{SdmmcHostCap, SdmmcHostInfo};
use fidl_next_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use sdmmc_spec::{
    CQHCI_CQ_IS_OFFSET, CQHCI_CQ_TCN_OFFSET, CQHCI_CQ_TDBR_OFFSET, CQHCI_CQ_TERRI_OFFSET,
    CqhciCqInterruptStatusRegister, CqhciCqTaskErrorRegister, Direction, EXT_CSD_BARRIER_SUPPORT,
    EXT_CSD_BARRIER_SUPPORT_MASK, EXT_CSD_CACHE_CTRL, EXT_CSD_CACHE_EN_MASK, SDHCI_IS_OFFSET,
    SdhciInterruptStatusRegister,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use zx::HandleBased as _;

struct MockRpmbServer {
    /// A stream which receives a new channel end each time an RPMB request is made.
    ///
    /// The request will hang until the test sends a message on the channel.
    request_sender: mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>,
}

impl MockRpmbServer {
    fn new(request_sender: mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>) -> Self {
        Self { request_sender }
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
        let (tx, rx) = futures::channel::oneshot::channel();
        let _ = self.request_sender.unbounded_send(tx);
        let _ = rx.await;
        responder.respond(()).await.ok();
    }
}

struct FakeCqhciHost {
    task_handler: Arc<FakeTaskHandler>,
    bti: fake_bti::FakeBti,
    /// A channel which receives a stream of all interrupts that the CQHCI driver delegates.
    /// Consumed when [`CommandQueueHost::initialize`] is called.
    /// The receiver end is made available to tests via [`TestHandles`].
    non_cq_interrupt_sink: Mutex<Option<mpsc::UnboundedSender<Arc<zx::VirtualInterrupt>>>>,
}

static INSTANCE: OnceLock<Arc<FakeCqhciHost>> = OnceLock::new();

impl FakeCqhciHost {
    pub fn global() -> Arc<Self> {
        INSTANCE.get().unwrap().clone()
    }

    fn register_global(self: Arc<Self>) {
        INSTANCE.set(self).map_err(|_| "Already registered").unwrap();
    }

    fn resources(&self) -> CommandQueueResources {
        CommandQueueResources {
            cqhci_mmio: Box::new(FakeMmio {
                state: self.task_handler.state.clone(),
                region_type: MmioRegionType::Cqhci,
            }),
            sdhci_mmio: Box::new(FakeMmio {
                state: self.task_handler.state.clone(),
                region_type: MmioRegionType::Sdhci,
            }),
            bti: self.bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            interrupt: self
                .task_handler
                .irq
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .unwrap()
                .into_handle()
                .into(),
        }
    }
}

/// A test implementation of [`CommandQueueHost`].
///
/// When running tests, we provide substitute MMIOs rather than using the real ones.
///
/// This is necessary for a few reasons:
/// - The real MMIO is backed by a memory-mapped VMO, and we can't easily simulate the W1S/W1C
///   semantics of the actual hardware registers.
/// - The test needs to synchronize with the driver as both sides update the MMIO, which is
///   challenging if the test and driver are both using their own separate memory-map of the same
///   VMO.  The easiest way to ensure synchronization is to have all updates go through the same
///   interface with appropriate locking.
///
/// We use a global singleton so that we can inject the instance into the driver while it is
/// starting up, without needing to weave the instance through the driver harness.
pub struct TestCommandQueueHost(Arc<FakeCqhciHost>);

impl TestCommandQueueHost {
    pub fn global() -> Box<dyn CommandQueueHost> {
        Box::new(TestCommandQueueHost(FakeCqhciHost::global())) as Box<dyn CommandQueueHost>
    }
}

#[async_trait]
impl CommandQueueHost for TestCommandQueueHost {
    async fn info(&self) -> Result<CqhciHostInfo, zx::Status> {
        let mut ext_csd = vec![0; 512];
        // Advertise all features, which make the driver take more interesting paths.
        ext_csd[EXT_CSD_CACHE_CTRL] = EXT_CSD_CACHE_EN_MASK;
        ext_csd[EXT_CSD_BARRIER_SUPPORT] = EXT_CSD_BARRIER_SUPPORT_MASK;
        Ok(CqhciHostInfo {
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
        })
    }

    async fn initialize(
        &self,
        virtual_interrupt: zx::VirtualInterrupt,
        virtual_irq_lifeline: zx::EventPair,
    ) -> Result<CommandQueueResources, zx::Status> {
        let hardware_irq = Arc::new(zx::VirtualInterrupt::from(
            self.0
                .task_handler
                .irq
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .unwrap()
                .into_handle(),
        ));
        let non_cq_interrupt_sink = self.0.non_cq_interrupt_sink.lock().take().unwrap();
        std::thread::Builder::new()
            .name("test-irq-delegator".to_owned())
            .spawn(move || {
                let port = zx::Port::create_with_opts(zx::PortOptions::BIND_TO_INTERRUPT);
                let mut exec = fasync::LocalExecutorBuilder::new().port(port).build();
                exec.run_singlethreaded(async move {
                    let _lifeline = virtual_irq_lifeline;
                    let mut virtual_irq_stream = Box::pin(fuchsia_async::OnInterrupt::new(
                        virtual_interrupt.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                    ));
                    let mut lifeline_waiter = fuchsia_async::OnSignals::new(
                        &_lifeline,
                        zx::Signals::EVENTPAIR_PEER_CLOSED,
                    )
                    .fuse();

                    use futures::{FutureExt as _, StreamExt as _};
                    loop {
                        futures::select! {
                            irq = virtual_irq_stream.next().fuse() => {
                                match irq.expect("Stream ended unexpectedly") {
                                    Ok(_) => {}
                                    Err(zx::Status::CANCELED) => break,
                                    Err(e) => panic!("Virtual interrupt failed: {:?}", e),
                                }
                                let _ = virtual_interrupt.ack();
                                non_cq_interrupt_sink.unbounded_send(hardware_irq.clone()).unwrap();
                            }
                            _ = &mut lifeline_waiter => {
                                break;
                            }
                        }
                    }
                });
            })
            .unwrap();

        Ok(self.0.resources())
    }

    async fn enable(&self) -> Result<(), zx::Status> {
        self.0.task_handler.state.lock().enabled = true;
        Ok(())
    }

    async fn disable(&self) -> Result<(), zx::Status> {
        self.0.task_handler.state.lock().enabled = false;
        Ok(())
    }

    fn on_task_submitted(&self, task: SubmittedTaskForTesting<'_>) {
        if let Some(hook) = self.0.task_handler.on_task_hook.as_ref() {
            hook(task);
        }
    }
}
struct Service {
    dispatcher: FidlExecutor<WeakDispatcher>,
    rpmb_request_sender: mpsc::UnboundedSender<futures::channel::oneshot::Sender<()>>,
}

impl cqhci::ServiceHandler for Service {
    fn cqhci(
        &self,
        _server_end: fidl_next::ServerEnd<fidl_next_fuchsia_hardware_cqhci::Cqhci, DriverChannel>,
    ) {
        unreachable!()
    }

    fn rpmb(&self, server_end: fidl_next::ServerEnd<fidl_next_fuchsia_hardware_rpmb::DriverRpmb>) {
        server_end
            .spawn_on(MockRpmbServer::new(self.rpmb_request_sender.clone()), &self.dispatcher);
    }
}

struct TestHandles {
    hardware_irq: zx::VirtualInterrupt,
    /// A receiver for non-CQ interrupts.  Each time the driver receives a non-CQ interrupt, it
    /// will forward it to the delegator, which in turn forwards the physical IRQ to this receiver
    /// to be acked by the test.  Note that the test MUST ack the interrupt, or the driver will
    /// never resume handling interrupts.
    non_cq_interrupt_receiver: mpsc::UnboundedReceiver<Arc<zx::VirtualInterrupt>>,
    /// A receiver for RPMB requests.  Each time the driver receives an RPMB request, it will send
    /// a message to this receiver, and will wait for the test to send a message back before
    /// continuing.
    rpmb_request_receiver: mpsc::UnboundedReceiver<futures::channel::oneshot::Sender<()>>,
}

struct FakeHardwareState {
    enabled: bool,
    cq_data: Vec<u32>,
    sd_data: Vec<u32>,
    requests_to_fail: u32,
    stall: bool,
    wake_event: event_listener::Event,
}

impl FakeHardwareState {
    fn load32(&self, region: MmioRegionType, offset: usize) -> u32 {
        let buffer = match region {
            MmioRegionType::Cqhci => &self.cq_data,
            MmioRegionType::Sdhci => &self.sd_data,
        };
        buffer[offset / 4]
    }

    fn store32(&mut self, region: MmioRegionType, offset: usize, value: u32) {
        let buffer = match region {
            MmioRegionType::Cqhci => &mut self.cq_data,
            MmioRegionType::Sdhci => &mut self.sd_data,
        };
        buffer[offset / 4] = value;
    }
}

#[derive(Clone, Copy)]
enum MmioRegionType {
    Cqhci,
    Sdhci,
}

/// A fake MMIO implementation which emulates hardware register behaviour, and notifies
/// FakeTaskHandler when tasks are submitted.
struct FakeMmio {
    state: Arc<Mutex<FakeHardwareState>>,
    region_type: MmioRegionType,
}

impl FakeMmio {
    fn buffer<'a>(&self, state: &'a FakeHardwareState) -> &'a [u32] {
        match self.region_type {
            MmioRegionType::Cqhci => &state.cq_data,
            MmioRegionType::Sdhci => &state.sd_data,
        }
    }

    fn buffer_mut<'a>(&self, state: &'a mut FakeHardwareState) -> &'a mut Vec<u32> {
        match self.region_type {
            MmioRegionType::Cqhci => &mut state.cq_data,
            MmioRegionType::Sdhci => &mut state.sd_data,
        }
    }
}

impl mmio::Mmio for FakeMmio {
    fn len(&self) -> usize {
        self.buffer(&self.state.lock()).len() * 4
    }
    fn align_offset(&self, _align: usize) -> usize {
        0
    }
    fn write_barrier(&self) {}

    fn try_load8(&self, _offset: usize) -> Result<u8, mmio::MmioError> {
        unreachable!()
    }
    fn try_load16(&self, _offset: usize) -> Result<u16, mmio::MmioError> {
        unreachable!()
    }
    fn try_load64(&self, _offset: usize) -> Result<u64, mmio::MmioError> {
        unreachable!()
    }

    fn try_load32(&self, offset: usize) -> Result<u32, mmio::MmioError> {
        let state = self.state.lock();
        let buf = self.buffer(&state);
        Ok(buf[offset / 4])
    }

    fn load8(&self, _offset: usize) -> u8 {
        unreachable!()
    }
    fn load16(&self, _offset: usize) -> u16 {
        unreachable!()
    }
    fn load64(&self, _offset: usize) -> u64 {
        unreachable!()
    }
    fn load32(&self, offset: usize) -> u32 {
        self.try_load32(offset).unwrap()
    }

    fn try_store8(&mut self, _offset: usize, _value: u8) -> Result<(), mmio::MmioError> {
        unreachable!()
    }
    fn try_store16(&mut self, _offset: usize, _value: u16) -> Result<(), mmio::MmioError> {
        unreachable!()
    }
    fn try_store64(&mut self, _offset: usize, _value: u64) -> Result<(), mmio::MmioError> {
        unreachable!()
    }

    fn try_store32(&mut self, offset: usize, value: u32) -> Result<(), mmio::MmioError> {
        let mut state = self.state.lock();
        let enabled = state.enabled;
        let buf = self.buffer_mut(&mut state);

        let idx = offset / 4;
        match (self.region_type, offset) {
            (MmioRegionType::Cqhci, offset) if offset == CQHCI_CQ_TDBR_OFFSET as usize => {
                assert!(enabled, "Doorbell rung while CQHCI is disabled");
                // Notify the FakeTaskHandler on doorbell ring
                let current = buf[idx];
                let new_val = current | value;
                buf[idx] = new_val;
                state.wake_event.notify(usize::MAX);
                Ok(())
            }
            (MmioRegionType::Cqhci, offset) if offset == CQHCI_CQ_IS_OFFSET as usize => {
                // Emulate W1C semantics
                let current = buf[idx];
                let cleared = current & !value;
                buf[idx] = cleared;
                Ok(())
            }
            (MmioRegionType::Sdhci, offset) if offset == SDHCI_IS_OFFSET as usize => {
                // Emulate W1C semantics
                let current = buf[idx];
                let cleared = current & !value;
                buf[idx] = cleared;
                Ok(())
            }
            _ => {
                buf[idx] = value;
                Ok(())
            }
        }
    }

    fn store8(&mut self, _offset: usize, _value: u8) {
        unreachable!()
    }
    fn store16(&mut self, _offset: usize, _value: u16) {
        unreachable!()
    }
    fn store64(&mut self, _offset: usize, _value: u64) {
        unreachable!()
    }
    fn store32(&mut self, offset: usize, value: u32) {
        self.try_store32(offset, value).unwrap();
    }
}

struct FakeTaskHandler {
    irq: zx::VirtualInterrupt,
    state: Arc<Mutex<FakeHardwareState>>,
    on_task_hook: Option<Box<dyn Fn(SubmittedTaskForTesting<'_>) + Send + Sync>>,
}

impl FakeTaskHandler {
    fn new(
        on_task_hook: Option<Box<dyn Fn(SubmittedTaskForTesting<'_>) + Send + Sync>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            irq: zx::Interrupt::create_virtual().unwrap(),
            state: Arc::new(Mutex::new(FakeHardwareState {
                enabled: false,
                cq_data: vec![0; 0x400],
                sd_data: vec![0; 0x400],
                requests_to_fail: 0,
                stall: false,
                wake_event: event_listener::Event::new(),
            })),
            on_task_hook,
        })
    }

    fn spawn(self: &Arc<Self>, scope: &fasync::Scope) -> fasync::JoinHandle<()> {
        let this = self.clone();
        scope.spawn(async move {
            loop {
                if let Some(l) = this.poll().await {
                    l.await;
                }
            }
        })
    }

    fn fail_next(&self, count: u32) {
        self.state.lock().requests_to_fail = count;
    }

    fn stall(&self, stall: bool) {
        let mut state = self.state.lock();
        state.stall = stall;
        if !stall {
            state.wake_event.notify(usize::MAX);
        }
    }

    async fn poll(&self) -> Option<event_listener::EventListener> {
        let mut state = self.state.lock();
        let listener = state.wake_event.listen();
        if state.stall {
            return Some(listener);
        }
        let doorbell = state.load32(MmioRegionType::Cqhci, CQHCI_CQ_TDBR_OFFSET as usize);
        if doorbell == 0 {
            return Some(listener);
        }
        // Consume one bit at a time.  This simplifies the implementation of the fake.
        let task_id = doorbell.trailing_zeros();
        let mask = 1 << task_id;
        let new_dbr = doorbell & !mask;
        state.store32(MmioRegionType::Cqhci, CQHCI_CQ_TDBR_OFFSET as usize, new_dbr);

        let mut sdhci_irq_status = SdhciInterruptStatusRegister::from_raw(0);
        let current_cq_is = state.load32(MmioRegionType::Cqhci, CQHCI_CQ_IS_OFFSET as usize);
        let mut cqhci_irq_status = CqhciCqInterruptStatusRegister::from_raw(current_cq_is);
        sdhci_irq_status.set_cqhci_interrupt(true);

        if state.requests_to_fail > 0 {
            state.requests_to_fail -= 1;
            cqhci_irq_status.set_response_error_detected(true);
            let mut terri = CqhciCqTaskErrorRegister(0);
            terri.set_response_mode_error_task_id(task_id as u8);
            terri.set_response_mode_error_fields_valid(true);
            state.store32(MmioRegionType::Cqhci, CQHCI_CQ_TERRI_OFFSET as usize, terri.0);
        } else {
            cqhci_irq_status.set_task_complete(true);
            let current_tcn = state.load32(MmioRegionType::Cqhci, CQHCI_CQ_TCN_OFFSET as usize);
            let new_tcn = current_tcn | mask;
            state.store32(MmioRegionType::Cqhci, CQHCI_CQ_TCN_OFFSET as usize, new_tcn);
        }

        state.store32(MmioRegionType::Cqhci, CQHCI_CQ_IS_OFFSET as usize, cqhci_irq_status.raw());
        state.store32(MmioRegionType::Sdhci, SDHCI_IS_OFFSET as usize, sdhci_irq_status.raw());

        self.irq.trigger(zx::BootInstant::get()).expect("failed to trigger interrupt");

        None
    }
}

fn setup(
    on_task_hook: Option<Box<dyn Fn(SubmittedTaskForTesting<'_>) + Send + Sync>>,
) -> (TestHarness<CqhciDriver>, fasync::Scope, TestHandles) {
    let (non_cq_interrupt_sink, non_cq_interrupt_receiver) = mpsc::unbounded();
    let (rpmb_request_sender, rpmb_request_receiver) = mpsc::unbounded();
    let scope = fasync::Scope::new_with_name("test");
    let mut service_fs = ServiceFs::new();
    let mut harness = TestHarness::<CqhciDriver>::new();

    let instance = Arc::new(FakeCqhciHost {
        task_handler: FakeTaskHandler::new(on_task_hook),
        bti: fake_bti::FakeBti::create().unwrap(),
        non_cq_interrupt_sink: Mutex::new(Some(non_cq_interrupt_sink)),
    });
    instance.task_handler.spawn(&scope);
    FakeCqhciHost::register_global(instance.clone());

    let test_handles = TestHandles {
        hardware_irq: instance.task_handler.irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        non_cq_interrupt_receiver,
        rpmb_request_receiver,
    };

    let offers = [ServiceOffer::<cqhci::Service>::new_next()
        .add_named_next(
            &mut service_fs,
            "default",
            Service {
                dispatcher: FidlExecutor::from(harness.dispatcher().clone()),
                rpmb_request_sender,
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

#[fuchsia::test]
async fn test_driver_lifecycle() {
    let (mut harness, _scope, _handles) = setup(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_io() {
    let data = Arc::new(Mutex::new(vec![0u8; 65536]));
    let data_clone = data.clone();
    let (mut harness, _scope, _handles) = setup(Some(Box::new(move |transfer_op| {
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
    })));
    let started_driver = harness.start_driver().await.expect("failed to start driver");

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
    let (mut harness, _scope, _handles) = setup(Some(Box::new(move |transfer_op| {
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
    })));
    let started_driver = harness.start_driver().await.expect("failed to start driver");

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
    let (mut harness, scope, mut handles) = setup(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let task_handler = FakeCqhciHost::global().task_handler.clone();

    // Generate a non-CQ interrupt
    {
        let mut state = task_handler.state.lock();
        let mut sdhci_interrupt_status = SdhciInterruptStatusRegister::from_raw(0);
        sdhci_interrupt_status.set_command_complete(true);
        state.store32(
            MmioRegionType::Sdhci,
            SDHCI_IS_OFFSET as usize,
            sdhci_interrupt_status.raw(),
        );
    }
    handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    // Wait until the first non-CQ interrupt is delivered to the delegator.
    let irq = handles.non_cq_interrupt_receiver.next().await.expect("Failed to wait for irq");

    // Trigger again while it's not acked.  It should not be delivered to CQHCI, because the CQHCI
    // driver shouldn't have acked it.
    handles.hardware_irq.trigger(zx::BootInstant::get()).unwrap();

    let first_irq_acked = Arc::new(Mutex::new(false));
    let first_irq_acked_clone = first_irq_acked.clone();
    let (mut next_irq_tx, mut next_irq_rx) = mpsc::channel(0);
    scope.spawn(async move {
        let irq =
            handles.non_cq_interrupt_receiver.next().await.expect("Failed to wait for second IRQ");
        assert!(*first_irq_acked_clone.lock());
        next_irq_tx.send(irq).await.unwrap();
    });

    // Now ack the virtual interrupt.  The second IRQ should be delivered.
    {
        let mut acked = first_irq_acked.lock();
        let _ = irq.ack();
        *acked = true;
    }
    let irq2 = next_irq_rx.next().await.expect("Failed to wait for second IRQ");
    {
        let mut state = task_handler.state.lock();
        state.store32(MmioRegionType::Sdhci, SDHCI_IS_OFFSET as usize, 0);
    }
    let _ = irq2.ack();

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_error_recovery() {
    let (mut harness, scope, _handles) = setup(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let task_handler = FakeCqhciHost::global().task_handler.clone();

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
    let (mut harness, _scope, _handles) = setup(Some(Box::new(move |transfer_op| {
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
    })));
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
    let (mut harness, scope, _handles) = setup(None);
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
    let rpmb_active = Arc::new(AtomicBool::new(false));
    let rpmb_active_clone = rpmb_active.clone();
    let (mut harness, scope, handles) = setup(Some(Box::new(move |_| {
        assert!(
            !rpmb_active_clone.load(Ordering::Relaxed),
            "task submitted while RPMB request is active!"
        );
    })));

    let started_driver = harness.start_driver().await.expect("failed to start driver");
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
    // Intercept when the DCMD is submitted.
    let (dcmd_tx, dcmd_rx) = futures::channel::oneshot::channel();
    let dcmd_tx = Arc::new(Mutex::new(Some(dcmd_tx)));

    let (mut harness, scope, _handles) = setup(Some(Box::new(move |transfer_op| {
        if let SubmittedTaskForTesting::DirectCmd = transfer_op {
            if let Some(tx) = dcmd_tx.lock().take() {
                let _ = tx.send(());
            }
        }
    })));

    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let task_handler = FakeCqhciHost::global().task_handler.clone();

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
    let (mut harness, scope, _handles) = setup(None);
    let started_driver = harness.start_driver().await.expect("failed to start driver");
    let task_handler = FakeCqhciHost::global().task_handler.clone();

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
