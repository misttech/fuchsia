// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod abi;
mod dma;
mod driver;

use crate::driver::GvnicDriver;
use fdf_component::driver_register;

driver_register!(GvnicDriver);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{
        AdminqOpcode, DeviceStatus, DriverStatus, GVNIC_ADMINQ_STATUS_PASSED, GVNIC_DOORBELL_BAR,
        GVNIC_GQI_QPL_FORMAT, GVNIC_MSIX_BAR, GVNIC_REGISTER_BAR, GVNIC_TX_DESC_TYPE_PKT,
        GvnicAdminqConfigureDeviceResources, GvnicAdminqCreateRxQueue, GvnicAdminqCreateTxQueue,
        GvnicAdminqDescribeDevice, GvnicAdminqEntry, GvnicAdminqRegisterPageList,
        GvnicDeviceDescriptor, GvnicQueueResources, GvnicRegisters, GvnicRxDesc, GvnicTxPktDesc,
        IrqDoorbellFlags, REG_ADMIN_QUEUE_COUNTER_OFFSET, REG_DEV_STATUS_OFFSET,
    };
    use crate::driver::PORT_ID;
    use diagnostics_assertions::assert_data_tree;
    use fake_bti::FakeBti;
    use fake_pci::FakePci;
    use fdf::AsyncDispatcher;
    use fdf_component::ServiceInstance;
    use fdf_component::testing::harness::{DriverUnderTest, TestHarness};
    use fdf_fidl::{DriverChannel, FidlExecutor, create_channel};
    use fidl_fuchsia_inspect::{InspectSinkRequest, InspectSinkRequestStream, TreeProxy};
    use fidl_next::{Client, ClientEnd, Request, Responder, ServerEnd};
    use fidl_next_fuchsia_hardware_network::{
        FRAME_FEATURES_RAW, FrameType, FrameTypeSupport, MacFilterMode, PortClass, PortStatus,
        StatusFlags, TxFlags,
    };
    use fidl_next_fuchsia_hardware_network_driver::network_device_ifc::{
        AddPort, CompleteRx, CompleteTx, DelegateRxLease, PortStatusChanged, RemovePort,
        RequestRxSpace, UpdateRxBufferParams,
    };
    use fidl_next_fuchsia_hardware_network_driver::{
        BufferMetadata, BufferRegion, NetworkDeviceIfc, NetworkDeviceIfcServerHandler,
        NetworkDeviceImpl, NetworkPort, RxBuffer, RxSpaceBuffer, Service as NetdevServiceMarker,
        SupportedMacFilterMode, TxBuffer, TxPartialCsumMetadata, TxResult,
    };
    use fidl_next_fuchsia_hardware_pci::InterruptMode;
    use fidl_next_fuchsia_net::MacAddress;
    use fuchsia_async::{Scope, Timer};
    use fuchsia_component::server::ServiceFs;
    use fuchsia_inspect::reader::{DiagnosticsHierarchy, read};
    use fuchsia_sync::Mutex;
    use futures::StreamExt;
    use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
    use mmio::region::MmioRegion;
    use mmio::vmo::{VmoMapping, VmoMemory};
    use mmio::{Mmio, MmioExt};
    use std::collections::HashMap;
    use std::mem::size_of;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use zerocopy::byteorder::network_endian::{U16, U32, U64};
    use zerocopy::{FromBytes, IntoBytes};
    use zx::sys::zx_paddr_t;
    use zx::{BootInstant, CachePolicy, Rights, VirtualInterrupt, Vmo, system_get_page_size};

    const TEST_MTU: u16 = 1500;
    const TEST_ROUNDED_MTU: u16 = 2048;
    const TEST_QUEUE_ENTRIES: u16 = 64;
    const TEST_PAGES_PER_QPL: u16 = 64;
    const TEST_COUNTERS: u16 = 16;
    const TEST_MAX_TX_QUEUES: u32 = 8;
    const TEST_MAX_RX_QUEUES: u32 = 8;
    const TEST_MAC: [u8; 6] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
    const TX_DOORBELL_IDX: u32 = 1;
    const RX_DOORBELL_IDX: u32 = 2;
    const TX_IRQ_DOORBELL_IDX: u32 = 5;
    const RX_IRQ_DOORBELL_IDX: u32 = 6;
    const TX_COUNTER_IDX: u32 = 3;
    const RX_COUNTER_IDX: u32 = 4;

    #[derive(Default, Clone)]
    struct EmulatedDeviceState {
        counter_array_phys: usize,
        irq_db_phys: usize,
        page_lists: HashMap<u32, Vec<usize>>,
        tx_ring_phys: usize,
        tx_qpl_id: u32,
        rx_desc_ring_phys: usize,
        rx_data_ring_phys: usize,
        rx_qpl_id: u32,
        rx_packet_buffer_size: u16,
    }

    struct InspectSinkServer {
        rx: UnboundedReceiver<InspectSinkRequestStream>,
        tree: Option<TreeProxy>,
    }

    struct FakeGvnicEnv {
        fake_pci: FakePci,
        fake_bti: Arc<FakeBti>,
        reg_mmio: MmioRegion<VmoMemory>,
        doorbell_mmio: MmioRegion<VmoMemory>,
        tx_irq: Arc<VirtualInterrupt>,
        rx_irq: Arc<VirtualInterrupt>,
        mgmt_irq: Arc<VirtualInterrupt>,
        state: Arc<Mutex<EmulatedDeviceState>>,
        inspect_tx: UnboundedSender<InspectSinkRequestStream>,
        inspect_sink: futures::lock::Mutex<InspectSinkServer>,
        stop_flag: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    fn read_mmio_regs(mmio: &MmioRegion<VmoMemory>) -> GvnicRegisters {
        let mut buf = [0u8; size_of::<GvnicRegisters>()];
        for (i, chunk) in buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *chunk = mmio.load32(i * 4).to_ne_bytes();
        }
        GvnicRegisters::read_from_bytes(&buf).expect("parse regs")
    }

    fn write_mmio_regs(mmio: &mut MmioRegion<VmoMemory>, regs: &GvnicRegisters) {
        for (i, &chunk) in regs.as_bytes().as_chunks::<4>().0.iter().enumerate() {
            mmio.store32(i * 4, u32::from_ne_bytes(chunk));
        }
    }

    impl FakeGvnicEnv {
        fn new() -> Self {
            Self::with_initial_dev_status(DeviceStatus::DEVICE_IS_RESET)
        }

        fn with_initial_dev_status(initial_dev_status: DeviceStatus) -> Self {
            let page_size = system_get_page_size() as usize;
            let fake_bti = Arc::new(FakeBti::create().expect("create fake bti"));
            let paddrs: Vec<zx_paddr_t> = (1..=2048).map(|i| i * page_size).collect();
            fake_bti.set_paddrs(&paddrs);

            let fake_pci = FakePci::new();
            let bti_dup = fake_bti.duplicate_handle(Rights::SAME_RIGHTS).expect("dup fake bti");
            fake_pci.add_bti(0, bti_dup);

            fake_pci.create_vmo_bar(GVNIC_REGISTER_BAR, page_size as u64);
            fake_pci.create_vmo_bar(GVNIC_MSIX_BAR, page_size as u64);
            fake_pci.create_vmo_bar(GVNIC_DOORBELL_BAR, page_size as u64);

            let reg_vmo = fake_pci.vmo_bar(GVNIC_REGISTER_BAR).expect("bar 0 vmo");
            let doorbell_vmo = fake_pci.vmo_bar(GVNIC_DOORBELL_BAR).expect("bar 2 vmo");
            reg_vmo.set_cache_policy(CachePolicy::UnCachedDevice).expect("set bar 0 cache policy");
            doorbell_vmo
                .set_cache_policy(CachePolicy::UnCachedDevice)
                .expect("set bar 2 cache policy");

            let worker_reg_vmo =
                reg_vmo.duplicate_handle(Rights::SAME_RIGHTS).expect("dup reg vmo");
            let mut reg_mmio = VmoMapping::map(0, page_size, reg_vmo).expect("map bar 0");
            let doorbell_mmio = VmoMapping::map(0, page_size, doorbell_vmo).expect("map bar 2");

            let initial_regs = GvnicRegisters {
                dev_status: U32::new(initial_dev_status.bits()),
                max_tx_queues: U32::new(TEST_MAX_TX_QUEUES),
                max_rx_queues: U32::new(TEST_MAX_RX_QUEUES),
                ..Default::default()
            };
            write_mmio_regs(&mut reg_mmio, &initial_regs);

            let tx_irq = fake_pci.add_msix_interrupt();
            let rx_irq = fake_pci.add_msix_interrupt();
            let mgmt_irq = fake_pci.add_msix_interrupt();

            let state = Arc::new(Mutex::new(EmulatedDeviceState::default()));
            let (inspect_tx, inspect_rx) = mpsc::unbounded();
            let inspect_sink =
                futures::lock::Mutex::new(InspectSinkServer { rx: inspect_rx, tree: None });
            let stop_flag = Arc::new(AtomicBool::new(false));

            let worker_bti = Arc::clone(&fake_bti);
            let worker_state = Arc::clone(&state);
            let worker_stop = Arc::clone(&stop_flag);

            let worker = thread::spawn(move || {
                Self::device_loop(worker_reg_vmo, worker_bti, worker_state, worker_stop);
            });

            Self {
                fake_pci,
                fake_bti,
                reg_mmio,
                doorbell_mmio,
                tx_irq,
                rx_irq,
                mgmt_irq,
                state,
                inspect_tx,
                inspect_sink,
                stop_flag,
                worker: Some(worker),
            }
        }

        async fn read_inspect(&self) -> DiagnosticsHierarchy {
            let mut sink = self.inspect_sink.lock().await;
            let tree = match &sink.tree {
                Some(tree) => tree.clone(),
                None => {
                    let mut stream = sink.rx.next().await.expect("inspect sink connection");
                    let Some(Ok(InspectSinkRequest::Publish { payload, .. })) = stream.next().await
                    else {
                        panic!("expected InspectSink::Publish request");
                    };
                    let tree = payload.tree.expect("inspect tree").into_proxy();
                    sink.tree.insert(tree).clone()
                }
            };
            read(&tree).await.expect("read inspect hierarchy")
        }

        fn read_regs(&self) -> GvnicRegisters {
            read_mmio_regs(&self.reg_mmio)
        }

        fn read_doorbell(&self, index: u32) -> u32 {
            let offset = (index as usize) * 4;
            self.doorbell_mmio.load::<U32>(offset).get()
        }

        fn device_loop(
            reg_vmo: Vmo,
            bti: Arc<FakeBti>,
            state: Arc<Mutex<EmulatedDeviceState>>,
            stop_flag: Arc<AtomicBool>,
        ) {
            let page_size = system_get_page_size() as usize;
            let mut reg_mmio = VmoMapping::map(0, page_size, reg_vmo).expect("map worker reg vmo");
            while !stop_flag.load(Ordering::Relaxed) {
                let mut regs = read_mmio_regs(&reg_mmio);

                let drv_status = DriverStatus::from_bits_truncate(regs.drv_status.get());
                let dev_status = DeviceStatus::from_bits_truncate(regs.dev_status.get());
                if drv_status == DriverStatus::RESET {
                    if !dev_status.contains(DeviceStatus::DEVICE_IS_RESET) {
                        regs.dev_status = U32::new(DeviceStatus::DEVICE_IS_RESET.bits());
                        regs.admin_queue_counter = U32::ZERO;
                        regs.admin_queue_doorbell = U32::ZERO;
                        write_mmio_regs(&mut reg_mmio, &regs);
                    }
                } else if drv_status.contains(DriverStatus::RUN) || regs.admin_queue_pfn.get() != 0
                {
                    if dev_status.contains(DeviceStatus::DEVICE_IS_RESET) {
                        regs.dev_status = U32::new(DeviceStatus::LINK_STATUS.bits());
                        reg_mmio.store(REG_DEV_STATUS_OFFSET, regs.dev_status);
                    }
                    let doorbell = regs.admin_queue_doorbell.get();
                    let counter = regs.admin_queue_counter.get();
                    if doorbell > counter {
                        let base_addr = (u64::from(regs.admin_queue_base_address_hi.get()) << 32)
                            | u64::from(regs.admin_queue_base_address_lo.get());
                        let admin_phys = if base_addr != 0 {
                            base_addr as usize
                        } else {
                            (regs.admin_queue_pfn.get() as usize) * page_size
                        };
                        let pinned = bti.get_pinned_vmo();

                        for idx in counter..doorbell {
                            let entry_phys =
                                admin_phys + (idx as usize) * size_of::<GvnicAdminqEntry>();
                            let mut entry: GvnicAdminqEntry =
                                pinned.read(entry_phys).expect("read adminq entry");
                            match entry.opcode.get() {
                                x if x == AdminqOpcode::DescribeDevice as u32 => {
                                    let cmd: GvnicAdminqDescribeDevice = entry.read_payload();
                                    let descr = GvnicDeviceDescriptor {
                                        tx_queue_entries: U16::new(TEST_QUEUE_ENTRIES),
                                        rx_queue_entries: U16::new(TEST_QUEUE_ENTRIES),
                                        mtu: U16::new(TEST_MTU),
                                        counters: U16::new(TEST_COUNTERS),
                                        tx_pages_per_qpl: U16::new(TEST_PAGES_PER_QPL),
                                        rx_pages_per_qpl: U16::new(TEST_PAGES_PER_QPL),
                                        mac: TEST_MAC,
                                        ..Default::default()
                                    };
                                    pinned
                                        .write(cmd.device_descriptor_addr.get() as usize, descr)
                                        .expect("write device descriptor");
                                }
                                x if x == AdminqOpcode::ConfigureDeviceResources as u32 => {
                                    let cmd: GvnicAdminqConfigureDeviceResources =
                                        entry.read_payload();
                                    assert_eq!(cmd.queue_format, GVNIC_GQI_QPL_FORMAT);
                                    let irq_db_phys = cmd.irq_db_addr.get() as usize;
                                    pinned
                                        .write(irq_db_phys, U32::new(TX_IRQ_DOORBELL_IDX))
                                        .expect("write tx irq db idx");
                                    pinned
                                        .write(
                                            irq_db_phys + 16 * size_of::<u32>(),
                                            U32::new(RX_IRQ_DOORBELL_IDX),
                                        )
                                        .expect("write rx irq db idx");
                                    let mut st = state.lock();
                                    st.counter_array_phys = cmd.counter_array.get() as usize;
                                    st.irq_db_phys = irq_db_phys;
                                }
                                x if x == AdminqOpcode::RegisterPageList as u32 => {
                                    let cmd: GvnicAdminqRegisterPageList = entry.read_payload();
                                    let num_pages = cmd.num_pages.get() as usize;
                                    let list_phys = cmd.page_address_list_addr.get() as usize;
                                    let mut pages = Vec::with_capacity(num_pages);
                                    for p in 0..num_pages {
                                        let addr: U64 = pinned
                                            .read(list_phys + p * size_of::<U64>())
                                            .expect("read page paddr");
                                        pages.push(addr.get() as usize);
                                    }
                                    state.lock().page_lists.insert(cmd.page_list_id.get(), pages);
                                }
                                x if x == AdminqOpcode::CreateTxQueue as u32 => {
                                    let cmd: GvnicAdminqCreateTxQueue = entry.read_payload();
                                    let q_res = GvnicQueueResources {
                                        db_index: U32::new(TX_DOORBELL_IDX),
                                        counter_index: U32::new(TX_COUNTER_IDX),
                                        ..Default::default()
                                    };
                                    pinned
                                        .write(cmd.queue_resources_addr.get() as usize, q_res)
                                        .expect("write tx queue resources");
                                    let mut st = state.lock();
                                    st.tx_ring_phys = cmd.tx_ring_addr.get() as usize;
                                    st.tx_qpl_id = cmd.queue_page_list_id.get();
                                }
                                x if x == AdminqOpcode::CreateRxQueue as u32 => {
                                    let cmd: GvnicAdminqCreateRxQueue = entry.read_payload();
                                    let q_res = GvnicQueueResources {
                                        db_index: U32::new(RX_DOORBELL_IDX),
                                        counter_index: U32::new(RX_COUNTER_IDX),
                                        ..Default::default()
                                    };
                                    pinned
                                        .write(cmd.queue_resources_addr.get() as usize, q_res)
                                        .expect("write rx queue resources");
                                    let mut st = state.lock();
                                    st.rx_desc_ring_phys = cmd.rx_desc_ring_addr.get() as usize;
                                    st.rx_data_ring_phys = cmd.rx_data_ring_addr.get() as usize;
                                    st.rx_qpl_id = cmd.queue_page_list_id.get();
                                    st.rx_packet_buffer_size = cmd.packet_buffer_size.get();
                                }
                                opcode => panic!("Unexpected adminq opcode {opcode}"),
                            }
                            entry.status = U32::new(GVNIC_ADMINQ_STATUS_PASSED);
                            pinned.write(entry_phys, entry).expect("write adminq status");
                        }
                        regs.admin_queue_counter = U32::new(doorbell);
                        reg_mmio.store(REG_ADMIN_QUEUE_COUNTER_OFFSET, regs.admin_queue_counter);
                    }
                }
                thread::sleep(Duration::from_micros(50));
            }
        }
    }

    impl Drop for FakeGvnicEnv {
        fn drop(&mut self) {
            self.stop_flag.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    struct FakeNetdevIfc {
        added_ports: UnboundedSender<(u8, ClientEnd<NetworkPort, DriverChannel>)>,
        status_changes: UnboundedSender<(u8, PortStatus)>,
        completed_tx: UnboundedSender<Vec<TxResult>>,
        completed_rx: UnboundedSender<Vec<RxBuffer>>,
    }

    impl NetworkDeviceIfcServerHandler<DriverChannel> for FakeNetdevIfc {
        async fn port_status_changed(
            &mut self,
            request: Request<PortStatusChanged, DriverChannel>,
        ) {
            let payload = request.payload();
            let _ = self.status_changes.unbounded_send((payload.id, payload.new_status));
        }

        async fn add_port(
            &mut self,
            request: Request<AddPort, DriverChannel>,
            responder: Responder<AddPort, DriverChannel>,
        ) {
            let payload = request.payload();
            let _ = self.added_ports.unbounded_send((payload.id, payload.port));
            let _ = responder.respond(Ok(())).await;
        }

        async fn remove_port(&mut self, _request: Request<RemovePort, DriverChannel>) {}

        async fn complete_rx(&mut self, request: Request<CompleteRx, DriverChannel>) {
            let _ = self.completed_rx.unbounded_send(request.payload().rx);
        }

        async fn complete_tx(&mut self, request: Request<CompleteTx, DriverChannel>) {
            let _ = self.completed_tx.unbounded_send(request.payload().tx);
        }

        async fn delegate_rx_lease(&mut self, _request: Request<DelegateRxLease, DriverChannel>) {}

        async fn update_rx_buffer_params(
            &mut self,
            _request: Request<UpdateRxBufferParams, DriverChannel>,
            responder: Responder<UpdateRxBufferParams, DriverChannel>,
        ) {
            let _ = responder.respond(()).await;
        }

        async fn request_rx_space(&mut self, _request: Request<RequestRxSpace, DriverChannel>) {}
    }

    struct NetdevTestClient {
        dispatcher: FidlExecutor<AsyncDispatcher>,
        impl_client: Client<NetworkDeviceImpl, DriverChannel>,
        ifc_handler: Option<FakeNetdevIfc>,
        port_rx: UnboundedReceiver<(u8, ClientEnd<NetworkPort, DriverChannel>)>,
        status_rx: UnboundedReceiver<(u8, PortStatus)>,
        tx_done_rx: UnboundedReceiver<Vec<TxResult>>,
        rx_done_rx: UnboundedReceiver<Vec<RxBuffer>>,
    }

    impl NetdevTestClient {
        fn connect(started_driver: &DriverUnderTest<'_, GvnicDriver>) -> Self {
            let dispatcher = FidlExecutor::from(started_driver.harness().dispatcher());
            let service: ServiceInstance<NetdevServiceMarker> = started_driver
                .driver_outgoing()
                .service()
                .connect_next()
                .expect("connect netdev service");
            let (impl_client_end, impl_server_end): (
                ClientEnd<NetworkDeviceImpl, DriverChannel>,
                ServerEnd<NetworkDeviceImpl, DriverChannel>,
            ) = create_channel();
            service.network_device_impl(impl_server_end).expect("connect network_device_impl");
            let impl_client = impl_client_end.spawn_on(&dispatcher);

            let (port_tx, port_rx) = mpsc::unbounded();
            let (status_tx, status_rx) = mpsc::unbounded();
            let (tx_done_tx, tx_done_rx) = mpsc::unbounded();
            let (rx_done_tx, rx_done_rx) = mpsc::unbounded();

            Self {
                dispatcher,
                impl_client,
                ifc_handler: Some(FakeNetdevIfc {
                    added_ports: port_tx,
                    status_changes: status_tx,
                    completed_tx: tx_done_tx,
                    completed_rx: rx_done_tx,
                }),
                port_rx,
                status_rx,
                tx_done_rx,
                rx_done_rx,
            }
        }

        async fn init(&mut self) {
            let (ifc_client_end, ifc_server_end): (
                ClientEnd<NetworkDeviceIfc, DriverChannel>,
                ServerEnd<NetworkDeviceIfc, DriverChannel>,
            ) = create_channel();
            ifc_server_end
                .spawn_on(self.ifc_handler.take().expect("ifc_handler"), &self.dispatcher);
            assert_eq!(self.impl_client.init(ifc_client_end).await.expect("init").s, Ok(()));
        }

        async fn init_and_start(&mut self) {
            self.init().await;
            assert_eq!(self.impl_client.start().await.expect("start").s, Ok(()));
        }
    }

    fn make_harness(env: &FakeGvnicEnv, scope: &Scope) -> TestHarness<GvnicDriver> {
        let mut service_fs = ServiceFs::new();
        let inspect_tx = env.inspect_tx.clone();
        service_fs.dir("svc").add_fidl_service(move |stream: InspectSinkRequestStream| {
            let _ = inspect_tx.unbounded_send(stream);
        });
        let offer = env.fake_pci.serve(&mut service_fs, scope.to_handle(), "default");
        TestHarness::<GvnicDriver>::new().set_driver_incoming(service_fs).add_offer(offer)
    }

    #[fuchsia::test]
    async fn bind_and_stop() {
        let env = FakeGvnicEnv::new();
        let scope = Scope::new_with_name("test-pci");
        let mut harness = make_harness(&env, &scope);
        let started_driver = harness.start_driver().await.expect("start gvnic driver");

        assert_eq!(env.fake_pci.bus_master_enabled(), Some(true));
        assert_eq!(env.fake_pci.interrupt_mode(), InterruptMode::MsiX);
        assert_eq!(env.fake_pci.requested_irq_count(), 3);
        assert_ne!(env.fake_bti.info().expect("bti info during").pmo_count, 0);

        let regs = env.read_regs();
        assert_eq!(regs.drv_status.get(), DriverStatus::RUN.bits());
        assert_eq!(regs.admin_queue_pfn.get(), 0);
        assert_ne!(regs.admin_queue_base_address_lo.get(), 0);
        assert_eq!(usize::from(regs.admin_queue_length.get()), system_get_page_size() as usize);

        let st = env.state.lock().clone();
        assert_ne!(st.counter_array_phys, 0);
        assert_ne!(st.irq_db_phys, 0);
        assert_eq!(st.page_lists.len(), 2);
        assert_ne!(st.tx_ring_phys, 0);
        assert_ne!(st.rx_desc_ring_phys, 0);
        assert_ne!(st.rx_data_ring_phys, 0);
        assert_eq!(st.rx_packet_buffer_size, TEST_ROUNDED_MTU);

        assert_data_tree!(env.read_inspect().await, root: {
            is_started: true,
            mtu: u64::from(TEST_MTU),
            max_tx_queues: u64::from(TEST_MAX_TX_QUEUES),
            max_rx_queues: u64::from(TEST_MAX_RX_QUEUES),
        });

        let driver = started_driver.get_driver().expect("driver");
        driver.reset_card().await.expect("reset card");
        assert_data_tree!(env.read_inspect().await, root: {
            is_started: false,
            mtu: u64::from(TEST_MTU),
            max_tx_queues: u64::from(TEST_MAX_TX_QUEUES),
            max_rx_queues: u64::from(TEST_MAX_RX_QUEUES),
        });

        started_driver.stop_driver().await;

        let regs_after = env.read_regs();
        assert_eq!(regs_after.drv_status.get(), DriverStatus::RESET.bits());
        assert_eq!(regs_after.admin_queue_pfn.get(), 0);
        assert_eq!(env.fake_bti.info().expect("bti info after").pmo_count, 0);
    }

    #[fuchsia::test]
    async fn bind_and_stop_legacy_pfn() {
        let env = FakeGvnicEnv::with_initial_dev_status(DeviceStatus::LINK_STATUS);
        let scope = Scope::new_with_name("test-pci");
        let mut harness = make_harness(&env, &scope);
        let started_driver = harness.start_driver().await.expect("start gvnic driver");

        let regs = env.read_regs();
        assert_eq!(regs.drv_status.get(), 0);
        assert_ne!(regs.admin_queue_pfn.get(), 0);
        assert_eq!(regs.admin_queue_base_address_lo.get(), 0);
        assert_eq!(regs.admin_queue_length.get(), 0);

        started_driver.stop_driver().await;

        let regs_after = env.read_regs();
        assert_eq!(regs_after.admin_queue_pfn.get(), 0);
    }

    #[fuchsia::test]
    async fn device_port_and_mac_info() {
        let mut env = FakeGvnicEnv::new();
        let scope = Scope::new_with_name("test-pci");
        let mut harness = make_harness(&env, &scope);
        let started_driver = harness.start_driver().await.expect("start gvnic driver");
        let mut client = NetdevTestClient::connect(&started_driver);

        let info = client.impl_client.get_info().await.expect("get_info").info;
        assert_eq!(info.device_features, Some(0));
        assert_eq!(info.tx_depth, Some(TEST_QUEUE_ENTRIES));
        assert_eq!(info.rx_depth, Some(TEST_QUEUE_ENTRIES));
        assert_eq!(info.rx_threshold, Some(TEST_QUEUE_ENTRIES / 2));
        assert_eq!(info.max_buffer_parts, Some(1));
        assert_eq!(info.max_buffer_length, Some(u32::from(TEST_ROUNDED_MTU)));
        assert_eq!(info.buffer_alignment, Some(1));
        assert_eq!(info.min_rx_buffer_length, Some(u32::from(TEST_ROUNDED_MTU)));
        assert_eq!(info.min_tx_buffer_length, Some(0));
        assert_eq!(info.tx_head_length, Some(0));
        assert_eq!(info.tx_tail_length, Some(0));

        client.init().await;
        let (port_id, port_client_end) = client.port_rx.next().await.expect("port");
        assert_eq!(port_id, PORT_ID);
        let port_client = port_client_end.spawn_on(&client.dispatcher);

        let port_info = port_client.get_info().await.expect("port get_info").info;
        assert_eq!(port_info.port_class, Some(PortClass::Ethernet));
        assert_eq!(port_info.rx_types, Some(vec![FrameType::Ethernet]));
        assert_eq!(
            port_info.tx_types,
            Some(vec![FrameTypeSupport {
                type_: FrameType::Ethernet,
                features: FRAME_FEATURES_RAW,
                supported_flags: TxFlags::empty(),
            }])
        );

        let status = port_client.get_status().await.expect("port get_status").status;
        assert_eq!(status.flags, Some(StatusFlags::ONLINE));
        assert_eq!(status.mtu, Some(u32::from(TEST_MTU)));

        // Simulate link status going offline and back online via management interrupt.
        env.reg_mmio.store(REG_DEV_STATUS_OFFSET, U32::ZERO);
        env.mgmt_irq.trigger(BootInstant::get()).expect("trigger mgmt irq");
        let (status_port_id, new_status) = client.status_rx.next().await.expect("status offline");
        assert_eq!(status_port_id, PORT_ID);
        assert_eq!(new_status.flags, Some(StatusFlags::empty()));
        assert_eq!(new_status.mtu, Some(u32::from(TEST_MTU)));

        env.reg_mmio.store(REG_DEV_STATUS_OFFSET, U32::new(DeviceStatus::LINK_STATUS.bits()));
        env.mgmt_irq.trigger(BootInstant::get()).expect("trigger mgmt irq");
        let (status_port_id, new_status) = client.status_rx.next().await.expect("status online");
        assert_eq!(status_port_id, PORT_ID);
        assert_eq!(new_status.flags, Some(StatusFlags::ONLINE));
        assert_eq!(new_status.mtu, Some(u32::from(TEST_MTU)));

        port_client.set_active(true).await.expect("set_active");
        port_client.removed().await.expect("removed");

        let mac_client = port_client
            .get_mac()
            .await
            .expect("get_mac")
            .mac_ifc
            .expect("mac client end")
            .spawn_on(&client.dispatcher);
        let addr_resp = mac_client.get_address().await.expect("get_address");
        assert_eq!(addr_resp.mac, MacAddress { octets: TEST_MAC });

        let feat_resp = mac_client.get_features().await.expect("get_features");
        assert_eq!(feat_resp.features.multicast_filter_count, Some(0));
        assert_eq!(feat_resp.features.supported_modes, Some(SupportedMacFilterMode::PROMISCUOUS));
        mac_client
            .set_mode(MacFilterMode::Promiscuous, &[] as &[MacAddress])
            .await
            .expect("set_mode");

        assert_eq!(client.impl_client.start().await.expect("start call").s, Ok(()));
        assert_eq!(env.read_doorbell(RX_DOORBELL_IDX), u32::from(TEST_QUEUE_ENTRIES));
        client.impl_client.stop().await.expect("stop call");

        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn send_packets() {
        let env = FakeGvnicEnv::new();
        let scope = Scope::new_with_name("test-pci");
        let mut harness = make_harness(&env, &scope);
        let started_driver = harness.start_driver().await.expect("start gvnic driver");
        let mut client = NetdevTestClient::connect(&started_driver);
        client.init_and_start().await;

        let vmo_id: u8 = 42;
        let data_vmo = Vmo::create(4096).expect("create data vmo");
        let vmo_dup = data_vmo.duplicate_handle(Rights::SAME_RIGHTS).expect("dup vmo");
        assert_eq!(
            client.impl_client.prepare_vmo(vmo_id, vmo_dup).await.expect("prepare_vmo").s,
            Ok(())
        );

        let payload = [0xab_u8; 64];
        data_vmo.write(&payload, 128).expect("write payload to vmo");

        let tx_buf = TxBuffer {
            id: 77,
            data: vec![BufferRegion { vmo: vmo_id, offset: 128, length: 64 }],
            meta: BufferMetadata { port: PORT_ID, flags: 0, frame_type: FrameType::Ethernet },
            head_length: 0,
            tail_length: 0,
            partial_csum_metadata: TxPartialCsumMetadata { start: 0, offset: 0 },
        };
        client.impl_client.queue_tx(&[tx_buf]).await.expect("queue_tx");

        while env.read_doorbell(TX_DOORBELL_IDX) != 1 {
            Timer::new(Duration::from_millis(1)).await;
        }

        let st = env.state.lock().clone();
        let pinned = env.fake_bti.get_pinned_vmo();
        let desc: GvnicTxPktDesc = pinned.read(st.tx_ring_phys).expect("read tx desc");
        assert_eq!(desc.type_flags, GVNIC_TX_DESC_TYPE_PKT);
        assert_eq!(desc.descriptor_count, 1);
        assert_eq!(desc.len.get(), 64);
        assert_eq!(desc.seg_len.get(), 64);
        assert_eq!(desc.seg_addr.get(), 0);

        let tx_pages = st.page_lists.get(&st.tx_qpl_id).expect("tx qpl");
        let sent_bytes = pinned.read_bytes(tx_pages[0], 64).expect("read tx qpl bytes");
        assert_eq!(sent_bytes, payload);

        // Simulate hardware completing the TX packet and firing TX MSI-X interrupt.
        pinned
            .write(
                st.counter_array_phys + (TX_COUNTER_IDX as usize) * size_of::<U32>(),
                U32::new(1),
            )
            .expect("write tx counter");
        env.tx_irq.trigger(BootInstant::get()).expect("trigger tx irq");

        let completed = client.tx_done_rx.next().await.expect("complete_tx");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, 77);
        assert_eq!(completed[0].status, Ok(()));
        assert_eq!(
            env.read_doorbell(TX_IRQ_DOORBELL_IDX),
            (IrqDoorbellFlags::ACK | IrqDoorbellFlags::EVENT).bits()
        );

        client.impl_client.release_vmo(vmo_id).await.expect("release_vmo");
        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn receive_packet() {
        let env = FakeGvnicEnv::new();
        let scope = Scope::new_with_name("test-pci");
        let mut harness = make_harness(&env, &scope);
        let started_driver = harness.start_driver().await.expect("start gvnic driver");
        let mut client = NetdevTestClient::connect(&started_driver);
        client.init_and_start().await;

        let vmo_id: u8 = 9;
        let data_vmo = Vmo::create(4096).expect("create data vmo");
        let vmo_dup = data_vmo.duplicate_handle(Rights::SAME_RIGHTS).expect("dup vmo");
        assert_eq!(
            client.impl_client.prepare_vmo(vmo_id, vmo_dup).await.expect("prepare_vmo").s,
            Ok(())
        );

        let rx_space = RxSpaceBuffer {
            id: 99,
            region: BufferRegion { vmo: vmo_id, offset: 256, length: 2048 },
        };
        client.impl_client.queue_rx_space([rx_space]).await.expect("queue_rx_space");
        let _ = client.impl_client.get_info().await.expect("sync via get_info");

        let st = env.state.lock().clone();
        let pinned = env.fake_bti.get_pinned_vmo();
        let rx_pages = st.page_lists.get(&st.rx_qpl_id).expect("rx qpl");

        // Write 2-byte padding + 64-byte frame into RX page list slot 0.
        let expected_frame = [0xcd_u8; 64];
        let mut padded_frame = vec![0u8, 0u8];
        padded_frame.extend_from_slice(&expected_frame);
        pinned.write_bytes(rx_pages[0], &padded_frame).expect("write rx qpl frame");

        // Write RX descriptor at index 0 with sequence number 1 and length 66 (64 + 2).
        let rx_desc =
            GvnicRxDesc { length: U16::new(66), flags_seq: U16::new(1), ..Default::default() };
        pinned.write(st.rx_desc_ring_phys, rx_desc).expect("write rx desc");

        // Update RX counter to 1 and trigger RX MSI-X interrupt.
        pinned
            .write(
                st.counter_array_phys + (RX_COUNTER_IDX as usize) * size_of::<U32>(),
                U32::new(1),
            )
            .expect("write rx counter");
        env.rx_irq.trigger(BootInstant::get()).expect("trigger rx irq");

        let rx_completed = client.rx_done_rx.next().await.expect("complete_rx");
        assert_eq!(rx_completed.len(), 1);
        assert_eq!(rx_completed[0].meta.port, PORT_ID);
        assert_eq!(rx_completed[0].meta.frame_type, FrameType::Ethernet);
        assert_eq!(rx_completed[0].data.len(), 1);
        assert_eq!(rx_completed[0].data[0].id, 99);
        assert_eq!(rx_completed[0].data[0].offset, 0);
        assert_eq!(rx_completed[0].data[0].length, 64);

        let mut received_bytes = [0u8; 64];
        data_vmo.read(&mut received_bytes, 256).expect("read vmo rx bytes");
        assert_eq!(received_bytes, expected_frame);
        assert_eq!(env.read_doorbell(RX_DOORBELL_IDX), u32::from(TEST_QUEUE_ENTRIES) + 1);
        assert_eq!(
            env.read_doorbell(RX_IRQ_DOORBELL_IDX),
            (IrqDoorbellFlags::ACK | IrqDoorbellFlags::EVENT).bits()
        );

        client.impl_client.release_vmo(vmo_id).await.expect("release_vmo");
        started_driver.stop_driver().await;
    }
}
