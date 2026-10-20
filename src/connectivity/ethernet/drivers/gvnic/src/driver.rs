// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::abi::{
    AdminqCommand, AdminqOpcode, DeviceStatus, DriverStatus, GVNIC_ADMINQ_STATUS_PASSED,
    GVNIC_DEVICE_DESCRIPTOR_VERSION, GVNIC_DOORBELL_BAR, GVNIC_GQI_QPL_FORMAT, GVNIC_REGISTER_BAR,
    GVNIC_TX_DESC_TYPE_PKT, GVNIC_VERSION_STRING, GvnicAdminqConfigureDeviceResources,
    GvnicAdminqCreateRxQueue, GvnicAdminqCreateTxQueue, GvnicAdminqDescribeDevice,
    GvnicAdminqEntry, GvnicAdminqRegisterPageList, GvnicDeviceDescriptor, GvnicQueueResources,
    GvnicRxDataRingSlot, GvnicRxDesc, GvnicTxPktDesc, IrqDoorbellFlags,
    REG_ADMIN_QUEUE_BASE_ADDRESS_HI_OFFSET, REG_ADMIN_QUEUE_BASE_ADDRESS_LO_OFFSET,
    REG_ADMIN_QUEUE_COUNTER_OFFSET, REG_ADMIN_QUEUE_DOORBELL_OFFSET, REG_ADMIN_QUEUE_LENGTH_OFFSET,
    REG_ADMIN_QUEUE_PFN_OFFSET, REG_DEV_STATUS_OFFSET, REG_DRIVER_VERSION_OFFSET,
    REG_DRV_STATUS_OFFSET, REG_MAX_RX_QUEUES_OFFSET, REG_MAX_TX_QUEUES_OFFSET, RxFlags,
};
use crate::dma::{PageList, PageListId};
use dma_buffer::ContiguousBuffer;
use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceInstance, ServiceOffer,
};
use fdf_fidl::{DriverChannel, create_channel};
use fidl_next::fuchsia::create_channel as create_zircon_channel;
use fidl_next::{
    Client, ClientDispatcher, ClientEnd, Error as FidlError, IgnoreEvents, ProtocolError, Request,
    Responder, ServerEnd,
};
use fidl_next_fuchsia_hardware_network::{
    FRAME_FEATURES_RAW, FrameType, FrameTypeSupport, PortBaseInfo, PortClass, PortStatus,
    StatusFlags, TxFlags,
};
use fidl_next_fuchsia_hardware_network_driver::mac_addr::{GetAddress, GetFeatures, SetMode};
use fidl_next_fuchsia_hardware_network_driver::network_device_impl::{
    GetInfo as DeviceGetInfo, Init, PrepareVmo, QueueRxSpace, QueueTx, ReleaseVmo, Start, Stop,
};
use fidl_next_fuchsia_hardware_network_driver::network_port::{
    GetInfo as PortGetInfo, GetMac, GetPowerDependencyToken, GetStatus, SetActive,
};
use fidl_next_fuchsia_hardware_network_driver::{
    BufferMetadata, DeviceImplInfo, Features, MAX_RX_BUFFERS, MAX_TX_RESULTS, MacAddr,
    MacAddrServerHandler, NetworkDeviceIfc, NetworkDeviceImpl, NetworkDeviceImplServerHandler,
    NetworkPort, NetworkPortServerHandler, RxBuffer, RxBufferPart, RxSpaceBuffer,
    Service as NetdevServiceMarker, ServiceHandler as NetdevServiceHandler, SupportedMacFilterMode,
    TxResult,
};
use fidl_next_fuchsia_hardware_pci::{
    Bar, BarResult, Device as PciDevice, InterruptMode, Service as PciService,
};
use fidl_next_fuchsia_net::MacAddress;
use fuchsia_async::{OnInterrupt, Scope, ScopeHandle, Timer};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::{BoolProperty, Inspector, Node as InspectNode, Property};
use fuchsia_sync::Mutex;
use futures::StreamExt;
use log::{error, warn};
use mapped_vmo::Mapping;
use mmio::region::MmioRegion;
use mmio::vmo::{VmoMapping, VmoMemory};
use mmio::{Mmio, MmioExt};
use std::collections::VecDeque;
use std::mem::size_of;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{Ordering, fence};
use zerocopy::byteorder::network_endian::{U16, U32, U64};
use zx::{BootDuration, BootInstant, Bti, Rights, Status, Vmar, VmarFlags, system_get_page_size};

const CHILD_NODE_NAME: &str = "gvnic";
pub(crate) const PORT_ID: u8 = 0;
const ETH_HLEN: usize = 14;
const RX_HEADER_PADDING_BYTES: usize = 2;
const PACKET_BUFFER_SIZE_ALIGNMENT: usize = 2048;
const REQUIRED_MSIX_COUNT: u32 = 3;
const TX_IRQ_INDEX: u32 = 0;
const RX_IRQ_INDEX: u32 = 1;
const MGMT_IRQ_INDEX: u32 = 2;
const IRQ_DOORBELL_IDX_STRIDE: u32 = 16;
const NUM_IRQ_DOORBELL_IDXS: u32 = 2;
const TX_PAGE_LIST_ID: PageListId = PageListId(0);
const RX_PAGE_LIST_ID: PageListId = PageListId(1);
const MAX_VMO_IDS: usize = (u8::MAX as usize) + 1;

// 5 seconds is a generous upper bound for synchronous gVNIC control-plane operations (admin queue
// commands and device reset acknowledgement) to accommodate hypervisor scheduling delays on GCE VMs
// while still bounding how long driver start/stop can wait on an unresponsive device.
const POLL_TIMEOUT: BootDuration = BootDuration::from_seconds(5);
const POLL_INITIAL_BACKOFF: BootDuration = BootDuration::from_micros(1);
const POLL_MAX_BACKOFF: BootDuration = BootDuration::from_millis(10);

struct AdminQueue {
    buf: ContiguousBuffer,
    next_idx: u32,
}

impl AdminQueue {
    fn new(vmar: &Vmar, bti: &Bti, page_size: usize) -> Result<Self, Status> {
        Ok(Self { buf: ContiguousBuffer::new_with_vmar(bti, vmar, page_size)?, next_idx: 0 })
    }

    fn phys(&self) -> u64 {
        self.buf.phys() as u64
    }

    /// Unpins the admin queue DMA buffer, consuming the admin queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure the hardware is no longer accessing the admin queue.
    unsafe fn unpin(self) {
        // SAFETY: The caller guarantees hardware DMA to the admin queue has ceased.
        let _ = unsafe { self.buf.unpin() };
    }

    async fn execute<C: AdminqCommand>(
        &mut self,
        reg_mmio: &mut MmioRegion<VmoMemory>,
        cmd: &C,
    ) -> Result<(), Status> {
        self.execute_raw(reg_mmio, C::OPCODE, cmd.as_bytes()).await
    }

    async fn execute_raw(
        &mut self,
        reg_mmio: &mut MmioRegion<VmoMemory>,
        opcode: AdminqOpcode,
        payload: &[u8],
    ) -> Result<(), Status> {
        let max_entries = self.buf.size() / size_of::<GvnicAdminqEntry>();
        let idx = (self.next_idx as usize) % max_entries;
        self.buf.write_element(idx, &GvnicAdminqEntry::new_command(opcode, payload));
        self.next_idx = self.next_idx.wrapping_add(1);
        let doorbell_target = self.next_idx;

        // Ensure the command descriptor writes to the admin queue DMA buffer are visible before the
        // MMIO doorbell write notifies the device.
        fence(Ordering::Release);
        reg_mmio.store(REG_ADMIN_QUEUE_DOORBELL_OFFSET, U32::new(doorbell_target));

        poll_until(|| {
            reg_mmio.load::<U32>(REG_ADMIN_QUEUE_COUNTER_OFFSET).get() == doorbell_target
        })
        .await?;
        // Ensure the MMIO completion counter read is ordered before reading the completion status
        // that the device wrote to the admin queue DMA buffer.
        fence(Ordering::Acquire);

        let completed: GvnicAdminqEntry = self.buf.read_element(idx);
        if completed.status.get() != GVNIC_ADMINQ_STATUS_PASSED {
            error!(
                "Admin queue command (0x{:x}) at index {idx} failed with status 0x{:x}",
                opcode as u32,
                completed.status.get()
            );
            return Err(Status::INTERNAL);
        }
        Ok(())
    }
}

struct TxRingState {
    resources: ContiguousBuffer,
    ring: ContiguousBuffer,
    len: u16,
    buffer_size: u16,
    next_free: u16,
    doorbell: u32,
    counter_val: u32,
    pending_space: VecDeque<u32>,
}

impl TxRingState {
    /// Unpins the TX queue resources and descriptor ring DMA buffers, consuming the ring state.
    ///
    /// # Safety
    ///
    /// The caller must ensure the hardware is no longer accessing the TX ring.
    unsafe fn unpin(self) {
        // SAFETY: The caller guarantees hardware DMA to the TX ring has ceased.
        unsafe {
            let _ = self.resources.unpin();
            let _ = self.ring.unpin();
        }
    }
}

struct RxRingState {
    resources: ContiguousBuffer,
    desc_ring: ContiguousBuffer,
    data_ring: ContiguousBuffer,
    doorbell: u32,
    counter_val: u32,
    pending_space: VecDeque<RxSpaceBuffer>,
}

impl RxRingState {
    /// Unpins the RX queue resources, descriptor ring, and data ring DMA buffers, consuming the
    /// ring state.
    ///
    /// # Safety
    ///
    /// The caller must ensure the hardware is no longer accessing the RX ring.
    unsafe fn unpin(self) {
        // SAFETY: The caller guarantees hardware DMA to the RX ring has ceased.
        unsafe {
            let _ = self.resources.unpin();
            let _ = self.desc_ring.unpin();
            let _ = self.data_ring.unpin();
        }
    }
}

struct GvnicDmaResources {
    admin_queue: AdminQueue,
    counter_array: ContiguousBuffer,
    irq_doorbell_idx_page: ContiguousBuffer,
    tx_page_list: PageList,
    rx_page_list: PageList,
    tx_ring: TxRingState,
    rx_ring: RxRingState,
}

impl GvnicDmaResources {
    /// Unpins all DMA buffers owned by the driver, consuming the DMA resource set.
    ///
    /// # Safety
    ///
    /// The caller must ensure the hardware has been reset and is no longer performing DMA.
    unsafe fn unpin(self) {
        // SAFETY: The caller guarantees hardware DMA is quiesced.
        unsafe {
            self.admin_queue.unpin();
            let _ = self.counter_array.unpin();
            let _ = self.irq_doorbell_idx_page.unpin();
            self.tx_page_list.unpin();
            self.rx_page_list.unpin();
            self.tx_ring.unpin();
            self.rx_ring.unpin();
        }
    }
}

struct GvnicInner {
    reg_mmio: MmioRegion<VmoMemory>,
    doorbell_mmio: MmioRegion<VmoMemory>,
    vmar: Vmar,
    bti: Bti,
    dev_descr: GvnicDeviceDescriptor,
    tx_ring_len: u16,
    rx_ring_len: u16,
    rounded_mtu: u16,
    dma: Option<GvnicDmaResources>,
    vmo_map: [Option<Mapping>; MAX_VMO_IDS],
    netdev_ifc: Option<Client<NetworkDeviceIfc, DriverChannel>>,
    netdev_started: bool,
    is_started_metric: BoolProperty,
}

pub(crate) struct GvnicDriver {
    inner: Arc<Mutex<GvnicInner>>,
    _node: Node,
    scope: Scope,
}

struct NetdevService {
    inner: Arc<Mutex<GvnicInner>>,
    scope: ScopeHandle,
}

impl NetdevServiceHandler for NetdevService {
    fn network_device_impl(&self, server_end: ServerEnd<NetworkDeviceImpl, DriverChannel>) {
        let (tx_cap, rx_cap) = {
            let inner = self.inner.lock();
            (usize::from(inner.tx_ring_len), usize::from(inner.rx_ring_len))
        };
        server_end.spawn_on(
            NetworkDeviceImplServer {
                inner: Arc::clone(&self.inner),
                scope: self.scope.clone(),
                tx_results: Vec::with_capacity(tx_cap),
                rx_buffers: Vec::with_capacity(rx_cap),
            },
            &self.scope,
        );
    }
}

impl Driver for GvnicDriver {
    const NAME: &str = "gvnic";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let pci_client = Self::connect_pci(&context).await?;
        let vmar = context.vmar().duplicate_handle(Rights::SAME_RIGHTS)?;
        let node = context.take_node()?;
        let inspector = Inspector::default();
        let scope = Scope::new_with_name("gvnic-driver");
        let scope_handle = scope.to_handle();

        let _ = context.publish_inspect(&inspector, scope_handle.clone());

        let inner =
            Self::bind_and_init(pci_client, vmar, inspector.root(), scope_handle.clone()).await?;

        let mut outgoing = ServiceFs::new();
        let offer = ServiceOffer::<NetdevServiceMarker>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                NetdevService { inner: Arc::clone(&inner), scope: scope_handle },
            )
            .build_driver_offer();

        let child_args = NodeBuilder::new(CHILD_NODE_NAME).add_offer(offer).build();
        node.add_child(child_args).await.map_err(|err| {
            error!("Failed to add child node: {err:?}");
            Status::INTERNAL
        })?;

        context.serve_outgoing(&mut outgoing)?;
        scope.spawn_local(outgoing.collect());

        Ok(Self { inner, _node: node, scope })
    }

    async fn stop(&self) {
        self.scope.clone().cancel().await;
        if let Err(err) = self.reset_card().await {
            error!("Failed to reset card on stop: {err:?}");
        }
    }
}

trait ResultExt<T> {
    fn domain_error_to_internal(self) -> Result<T, Status>;
}

impl<T> ResultExt<T> for Result<Result<T, Status>, FidlError> {
    fn domain_error_to_internal(self) -> Result<T, Status> {
        self.map_err(|_| Status::INTERNAL)?
    }
}

fn extract_mmio_bar(bar: Bar) -> Result<MmioRegion<VmoMemory>, Status> {
    let BarResult::Vmo(vmo) = bar.result else {
        return Err(Status::WRONG_TYPE);
    };
    let size = usize::try_from(bar.size).map_err(|_| Status::OUT_OF_RANGE)?;
    VmoMapping::map(0, size, vmo)
}

async fn poll_until(mut cond: impl FnMut() -> bool) -> Result<(), Status> {
    let deadline = BootInstant::after(POLL_TIMEOUT);
    let mut wait = POLL_INITIAL_BACKOFF;
    while !cond() {
        if BootInstant::get() >= deadline {
            return Err(Status::TIMED_OUT);
        }
        Timer::new(BootInstant::after(wait)).await;
        wait = (wait * 2).min(POLL_MAX_BACKOFF);
    }
    Ok(())
}

fn is_reset(mmio: &MmioRegion<VmoMemory>) -> bool {
    DeviceStatus::from_bits_truncate(mmio.load::<U32>(REG_DEV_STATUS_OFFSET).get())
        .contains(DeviceStatus::DEVICE_IS_RESET)
}

fn start_reset_mmio(reg_mmio: &mut MmioRegion<VmoMemory>) -> Option<usize> {
    if reg_mmio.load::<U32>(REG_ADMIN_QUEUE_PFN_OFFSET) != U32::ZERO {
        reg_mmio.store(REG_ADMIN_QUEUE_PFN_OFFSET, U32::ZERO);
        Some(REG_ADMIN_QUEUE_PFN_OFFSET)
    } else if reg_mmio.load::<U32>(REG_DRV_STATUS_OFFSET) != U32::ZERO && !is_reset(reg_mmio) {
        reg_mmio.store(REG_DRV_STATUS_OFFSET, U32::new(DriverStatus::RESET.bits()));
        Some(REG_DRV_STATUS_OFFSET)
    } else {
        None
    }
}

fn is_reset_done(reg_mmio: &MmioRegion<VmoMemory>, reg_offset: usize) -> bool {
    if reg_offset == REG_ADMIN_QUEUE_PFN_OFFSET {
        reg_mmio.load::<U32>(REG_ADMIN_QUEUE_PFN_OFFSET) == U32::ZERO
    } else {
        is_reset(reg_mmio)
    }
}

async fn complete_tx_batched(ifc: &Client<NetworkDeviceIfc, DriverChannel>, results: &[TxResult]) {
    for chunk in results.chunks(MAX_TX_RESULTS as usize) {
        let _ = ifc.complete_tx(chunk).await;
    }
}

async fn complete_rx_batched(ifc: &Client<NetworkDeviceIfc, DriverChannel>, buffers: &[RxBuffer]) {
    for chunk in buffers.chunks(MAX_RX_BUFFERS as usize) {
        let _ = ifc.complete_rx(chunk).await;
    }
}

fn empty_rx_buffer(id: u32) -> RxBuffer {
    RxBuffer {
        meta: BufferMetadata { port: PORT_ID, frame_type: FrameType::Ethernet, flags: 0 },
        data: vec![RxBufferPart { id, offset: 0, length: 0 }],
        full_csums_verified: 0,
    }
}

impl GvnicInner {
    fn write_doorbell(&mut self, index: u32, value: u32) {
        let offset = (index as usize) * size_of::<u32>();
        // Ensure packet payload and descriptor ring writes to DMA memory are visible before the
        // MMIO doorbell write notifies the device.
        fence(Ordering::Release);
        self.doorbell_mmio.store(offset, U32::new(value));
    }

    fn read_counter(&self, index: usize) -> u32 {
        let Some(dma) = &self.dma else {
            return 0;
        };
        let val = dma.counter_array.read_element::<U32>(index).get();
        // Ensure reading the device-updated completion counter from DMA memory is ordered before
        // reading completed TX/RX descriptors and packet payloads from DMA memory.
        fence(Ordering::Acquire);
        val
    }

    fn ack_irq_doorbell(&mut self, irq_index: u32) {
        let idx_offset = (irq_index * IRQ_DOORBELL_IDX_STRIDE) as usize;
        let Some(dma) = &self.dma else {
            return;
        };
        let irq_db_index = dma.irq_doorbell_idx_page.read_element::<U32>(idx_offset).get();
        self.write_doorbell(irq_db_index, (IrqDoorbellFlags::ACK | IrqDoorbellFlags::EVENT).bits());
    }

    fn port_status(&self) -> PortStatus {
        let dev_status = DeviceStatus::from_bits_truncate(
            self.reg_mmio.load::<U32>(REG_DEV_STATUS_OFFSET).get(),
        );
        let flags = if dev_status.contains(DeviceStatus::LINK_STATUS) {
            StatusFlags::ONLINE
        } else {
            StatusFlags::empty()
        };
        PortStatus { flags: Some(flags), mtu: Some(u32::from(self.dev_descr.mtu.get())) }
    }

    fn process_tx_completions(
        &mut self,
        results: &mut Vec<TxResult>,
    ) -> Option<Client<NetworkDeviceIfc, DriverChannel>> {
        if !self.netdev_started {
            return None;
        }
        let res: GvnicQueueResources = self.dma.as_ref()?.tx_ring.resources.read_element(0);
        let counter = self.read_counter(res.counter_index.get() as usize);
        let dma = self.dma.as_mut()?;
        while dma.tx_ring.counter_val != counter {
            let Some(id) = dma.tx_ring.pending_space.pop_front() else {
                break;
            };
            results.push(TxResult { id, status: Ok(()) });
            dma.tx_ring.counter_val = dma.tx_ring.counter_val.wrapping_add(1);
        }
        if !results.is_empty() { self.netdev_ifc.clone() } else { None }
    }

    fn process_rx_completions(
        &mut self,
        rx_buffers: &mut Vec<RxBuffer>,
    ) -> Option<Client<NetworkDeviceIfc, DriverChannel>> {
        let res: GvnicQueueResources = self.dma.as_ref()?.rx_ring.resources.read_element(0);
        let counter = self.read_counter(res.counter_index.get() as usize);
        let mut packets_processed: u32 = 0;
        let ring_len = usize::from(self.rx_ring_len);
        let rounded_mtu = usize::from(self.rounded_mtu);
        let netdev_started = self.netdev_started;

        let dma = self.dma.as_mut()?;
        while dma.rx_ring.counter_val != counter {
            let slot_idx = (dma.rx_ring.counter_val as usize) % ring_len;
            dma.rx_ring.counter_val = dma.rx_ring.counter_val.wrapping_add(1);
            packets_processed = packets_processed.wrapping_add(1);

            let desc: GvnicRxDesc = dma.rx_ring.desc_ring.read_element(slot_idx);
            let flags = RxFlags::from_bits_truncate(desc.flags_seq.get());
            let raw_len = usize::from(desc.length.get());
            if flags.intersects(RxFlags::ERR | RxFlags::CONT)
                || raw_len <= RX_HEADER_PADDING_BYTES
                || raw_len > rounded_mtu
            {
                warn!("Dropping invalid RX packet (flags=0x{:x}, len={raw_len})", flags.bits());
                continue;
            }

            if !netdev_started {
                continue;
            }
            let Some(space) = dma.rx_ring.pending_space.pop_front() else {
                warn!("No buffer space, dropping RX packet");
                continue;
            };

            let payload_len = raw_len - RX_HEADER_PADDING_BYTES;
            let space_len = usize::try_from(space.region.length).unwrap_or(0);
            let dst_offset = usize::try_from(space.region.offset).unwrap_or(usize::MAX);
            let Some(vmo) = self.vmo_map[usize::from(space.region.vmo)].as_mut() else {
                warn!("Dropping RX packet: unknown VMO id {}", space.region.vmo);
                rx_buffers.push(empty_rx_buffer(space.id));
                continue;
            };
            if payload_len > space_len {
                warn!(
                    "Received packet ({payload_len} bytes) will not fit in buffer space ({space_len} bytes), dropping packet"
                );
                rx_buffers.push(empty_rx_buffer(space.id));
                continue;
            }

            let qpl_offset = rounded_mtu * slot_idx + RX_HEADER_PADDING_BYTES;
            if let Err(err) = dma.rx_page_list.copy_to(qpl_offset, vmo, dst_offset, payload_len) {
                warn!("Dropping RX packet: failed to copy to VMO: {err:?}");
                rx_buffers.push(empty_rx_buffer(space.id));
                continue;
            }

            rx_buffers.push(RxBuffer {
                meta: BufferMetadata { port: PORT_ID, frame_type: FrameType::Ethernet, flags: 0 },
                data: vec![RxBufferPart {
                    id: space.id,
                    offset: 0,
                    length: u32::try_from(payload_len).unwrap_or(0),
                }],
                full_csums_verified: 0,
            });
        }

        if packets_processed > 0 {
            dma.rx_ring.doorbell = dma.rx_ring.doorbell.wrapping_add(packets_processed);
            let db_idx = res.db_index.get();
            let db_val = dma.rx_ring.doorbell;
            self.write_doorbell(db_idx, db_val);
        }

        if !rx_buffers.is_empty() { self.netdev_ifc.clone() } else { None }
    }
}

impl GvnicDriver {
    pub(crate) async fn reset_card(&self) -> Result<(), Status> {
        let (dma, reset_reg) = {
            let mut inner = self.inner.lock();
            let Some(dma) = inner.dma.take() else {
                return Ok(());
            };
            inner.netdev_started = false;
            inner.netdev_ifc = None;
            inner.is_started_metric.set(false);
            (dma, start_reset_mmio(&mut inner.reg_mmio))
        };
        if let Some(reg_offset) = reset_reg {
            poll_until(|| is_reset_done(&self.inner.lock().reg_mmio, reg_offset)).await?;
        }
        let inner = self.inner.lock();
        // SAFETY: The device reset has completed, so hardware DMA is quiesced and all pinned
        // buffers and quarantined PMTs can be safely unpinned/released.
        unsafe {
            dma.unpin();
            inner.bti.release_quarantine()?;
        }
        Ok(())
    }

    async fn connect_pci(context: &DriverContext) -> Result<Client<PciDevice>, Status> {
        for instance in ["default", "pci"] {
            let Ok(service): Result<ServiceInstance<PciService>, _> =
                context.incoming.service().instance(instance).connect_next()
            else {
                continue;
            };
            let (client_end, server_end) = create_zircon_channel();
            if service.device(server_end).is_err() {
                continue;
            }
            let client = client_end.spawn();
            match client.get_device_info().await {
                Ok(_) => return Ok(client),
                Err(FidlError::Protocol(ProtocolError::PeerClosed)) => continue,
                Err(err) => {
                    error!("Failed to query PCI device info: {err:?}");
                    return Err(Status::INTERNAL);
                }
            }
        }
        Err(Status::NOT_FOUND)
    }

    async fn bind_and_init(
        pci: Client<PciDevice>,
        vmar: Vmar,
        inspect_root: &InspectNode,
        scope: ScopeHandle,
    ) -> Result<Arc<Mutex<GvnicInner>>, Status> {
        // Enable bus mastering, obtain BTI 0, and map BAR 0 (registers) and BAR 2 (doorbells).
        pci.set_bus_mastering(true).await.domain_error_to_internal()?;
        let bti = pci.get_bti(0).await.domain_error_to_internal()?.bti;

        let bar0 = pci.get_bar(GVNIC_REGISTER_BAR).await.domain_error_to_internal()?.result;
        let mut reg_mmio = extract_mmio_bar(bar0)?;

        let bar2 = pci.get_bar(GVNIC_DOORBELL_BAR).await.domain_error_to_internal()?.result;
        let doorbell_mmio = extract_mmio_bar(bar2)?;

        // Reset card and write driver version string byte-by-byte to REG_DRIVER_VERSION_OFFSET.
        if let Some(reg_offset) = start_reset_mmio(&mut reg_mmio) {
            poll_until(|| is_reset_done(&reg_mmio, reg_offset)).await?;
        }
        // SAFETY: The device reset has completed, so hardware DMA is quiesced and any quarantined
        // PMTs can be safely released.
        unsafe {
            bti.release_quarantine()?;
        }
        for &byte in GVNIC_VERSION_STRING {
            reg_mmio.store8(REG_DRIVER_VERSION_OFFSET, byte);
        }

        // Create and configure the Admin Queue, then enable card.
        let page_size = system_get_page_size() as usize;
        let mut admin_queue = AdminQueue::new(&vmar, &bti, page_size)?;
        if is_reset(&reg_mmio) {
            let aq_len_u16 = u16::try_from(page_size).map_err(|_| Status::OUT_OF_RANGE)?;
            reg_mmio.store(REG_ADMIN_QUEUE_LENGTH_OFFSET, U16::new(aq_len_u16));
            reg_mmio.store(
                REG_ADMIN_QUEUE_BASE_ADDRESS_HI_OFFSET,
                U32::new((admin_queue.phys() >> 32) as u32),
            );
            reg_mmio.store(
                REG_ADMIN_QUEUE_BASE_ADDRESS_LO_OFFSET,
                U32::new((admin_queue.phys() & 0xffff_ffff) as u32),
            );
            reg_mmio.store(REG_DRV_STATUS_OFFSET, U32::new(DriverStatus::RUN.bits()));
        } else {
            let pfn = u32::try_from(admin_queue.phys() / (page_size as u64))
                .map_err(|_| Status::OUT_OF_RANGE)?;
            reg_mmio.store(REG_ADMIN_QUEUE_PFN_OFFSET, U32::new(pfn));
        }

        // Enable MSI-X interrupts (1 TX, 1 RX, 1 management = 3 total).
        let irq_modes = pci.get_interrupt_modes().await.map_err(|_| Status::INTERNAL)?.modes;
        if u32::from(irq_modes.msix_count) < REQUIRED_MSIX_COUNT {
            return Err(Status::NOT_SUPPORTED);
        }
        pci.set_interrupt_mode(InterruptMode::MsiX, REQUIRED_MSIX_COUNT)
            .await
            .domain_error_to_internal()?;
        let tx_irq = pci.map_interrupt(TX_IRQ_INDEX).await.domain_error_to_internal()?.interrupt;
        let rx_irq = pci.map_interrupt(RX_IRQ_INDEX).await.domain_error_to_internal()?.interrupt;
        let mgmt_irq =
            pci.map_interrupt(MGMT_IRQ_INDEX).await.domain_error_to_internal()?.interrupt;

        // Query device descriptor via Admin Queue using a temporary scratch page.
        let dev_descr: GvnicDeviceDescriptor = {
            let scratch_page = ContiguousBuffer::new_with_vmar(&bti, &vmar, page_size)?;
            let describe_cmd = GvnicAdminqDescribeDevice {
                device_descriptor_addr: U64::new(scratch_page.phys() as u64),
                device_descriptor_version: U32::new(GVNIC_DEVICE_DESCRIPTOR_VERSION),
                available_length: U32::new(
                    u32::try_from(scratch_page.size()).map_err(|_| Status::OUT_OF_RANGE)?,
                ),
            };
            // Note: If `execute` fails or times out, we intentionally do not unpin `scratch_page`
            // because the device may still be performing DMA to it; dropping `scratch_page` without
            // unpinning quarantines its PMT on the BTI until the next card reset and
            // `bti.release_quarantine()`.
            admin_queue.execute(&mut reg_mmio, &describe_cmd).await?;
            let descr = scratch_page.read_element(0);
            // SAFETY: `DescribeDevice` has completed synchronously on the admin queue, so the
            // device will no longer access `scratch_page`.
            let _ = unsafe { scratch_page.unpin() };
            descr
        };

        // Compute rounded MTU and allocate counter array, IRQ doorbell index page, TX/RX page
        // lists, and TX/RX rings.
        let rounded_mtu = u16::try_from(
            (usize::from(dev_descr.mtu.get()) + ETH_HLEN + RX_HEADER_PADDING_BYTES)
                .next_multiple_of(PACKET_BUFFER_SIZE_ALIGNMENT),
        )
        .map_err(|_| Status::OUT_OF_RANGE)?;

        let num_counters = usize::from(dev_descr.counters.get()).max(1);
        let counter_array =
            ContiguousBuffer::new_with_vmar(&bti, &vmar, size_of::<U32>() * num_counters)?;
        let irq_doorbell_idx_page = ContiguousBuffer::new_with_vmar(&bti, &vmar, page_size)?;

        let tx_ring_len = dev_descr.tx_queue_entries.get();
        let rx_ring_len = dev_descr.rx_queue_entries.get();
        if tx_ring_len == 0 || rx_ring_len == 0 {
            return Err(Status::INVALID_ARGS);
        }

        let min_tx_pages =
            (usize::from(tx_ring_len) * usize::from(rounded_mtu)).div_ceil(page_size);
        let tx_qpl_pages =
            u32::try_from(usize::from(dev_descr.tx_pages_per_qpl.get()).max(min_tx_pages))
                .map_err(|_| Status::OUT_OF_RANGE)?;
        let min_rx_pages =
            (usize::from(rx_ring_len) * usize::from(rounded_mtu)).div_ceil(page_size);
        let rx_qpl_pages =
            u32::try_from(usize::from(dev_descr.rx_pages_per_qpl.get()).max(min_rx_pages))
                .map_err(|_| Status::OUT_OF_RANGE)?;

        let tx_page_list = PageList::new(&vmar, &bti, TX_PAGE_LIST_ID, tx_qpl_pages)?;
        let rx_page_list = PageList::new(&vmar, &bti, RX_PAGE_LIST_ID, rx_qpl_pages)?;

        let tx_resources =
            ContiguousBuffer::new_with_vmar(&bti, &vmar, size_of::<GvnicQueueResources>())?;
        let tx_ring_buf = ContiguousBuffer::new_with_vmar(
            &bti,
            &vmar,
            size_of::<GvnicTxPktDesc>() * usize::from(tx_ring_len),
        )?;
        let tx_ring = TxRingState {
            resources: tx_resources,
            ring: tx_ring_buf,
            len: tx_ring_len,
            buffer_size: rounded_mtu,
            next_free: 0,
            doorbell: 0,
            counter_val: 0,
            pending_space: VecDeque::with_capacity(usize::from(tx_ring_len)),
        };

        let rx_resources =
            ContiguousBuffer::new_with_vmar(&bti, &vmar, size_of::<GvnicQueueResources>())?;
        let rx_desc_ring = ContiguousBuffer::new_with_vmar(
            &bti,
            &vmar,
            size_of::<GvnicRxDesc>() * usize::from(rx_ring_len),
        )?;
        let rx_data_ring = ContiguousBuffer::new_with_vmar(
            &bti,
            &vmar,
            size_of::<GvnicRxDataRingSlot>() * usize::from(rx_ring_len),
        )?;
        for i in 0..usize::from(rx_ring_len) {
            rx_data_ring.write_element(
                i,
                &GvnicRxDataRingSlot { qpl_offset: U64::new(u64::from(rounded_mtu) * (i as u64)) },
            );
        }
        let rx_ring = RxRingState {
            resources: rx_resources,
            desc_ring: rx_desc_ring,
            data_ring: rx_data_ring,
            doorbell: u32::from(rx_ring_len),
            counter_val: 0,
            pending_space: VecDeque::with_capacity(usize::from(rx_ring_len)),
        };

        let max_tx_queues = reg_mmio.load::<U32>(REG_MAX_TX_QUEUES_OFFSET).get();
        let max_rx_queues = reg_mmio.load::<U32>(REG_MAX_RX_QUEUES_OFFSET).get();
        let is_started_metric = inspect_root.create_bool("is_started", true);
        inspect_root.record_uint("mtu", u64::from(dev_descr.mtu.get()));
        inspect_root.record_uint("max_tx_queues", u64::from(max_tx_queues));
        inspect_root.record_uint("max_rx_queues", u64::from(max_rx_queues));

        // Submit initialization Admin Queue commands: ConfigureDeviceResources, RegisterPageList
        // (TX & RX), CreateTxQueue, and CreateRxQueue.
        admin_queue
            .execute(
                &mut reg_mmio,
                &GvnicAdminqConfigureDeviceResources {
                    counter_array: U64::new(counter_array.phys() as u64),
                    irq_db_addr: U64::new(irq_doorbell_idx_page.phys() as u64),
                    num_counters: U32::new(u32::from(dev_descr.counters.get())),
                    num_irq_dbs: U32::new(NUM_IRQ_DOORBELL_IDXS),
                    irq_db_stride: U32::new((size_of::<u32>() as u32) * IRQ_DOORBELL_IDX_STRIDE),
                    ntfy_blk_msix_base_idx: U32::ZERO,
                    queue_format: GVNIC_GQI_QPL_FORMAT,
                },
            )
            .await?;

        for pl in [&tx_page_list, &rx_page_list] {
            admin_queue
                .execute(
                    &mut reg_mmio,
                    &GvnicAdminqRegisterPageList {
                        page_list_id: pl.id().into(),
                        num_pages: U32::new(pl.length()),
                        page_address_list_addr: U64::new(pl.paddr_table_phys()),
                        page_size: U64::new(page_size as u64),
                    },
                )
                .await?;
        }

        admin_queue
            .execute(
                &mut reg_mmio,
                &GvnicAdminqCreateTxQueue {
                    queue_id: U32::ZERO,
                    reserved: U32::ZERO,
                    queue_resources_addr: U64::new(tx_ring.resources.phys() as u64),
                    tx_ring_addr: U64::new(tx_ring.ring.phys() as u64),
                    queue_page_list_id: tx_page_list.id().into(),
                    ntfy_id: U32::new(TX_IRQ_INDEX),
                    tx_comp_ring_addr: U64::ZERO,
                    tx_ring_size: dev_descr.tx_queue_entries,
                },
            )
            .await?;

        admin_queue
            .execute(
                &mut reg_mmio,
                &GvnicAdminqCreateRxQueue {
                    queue_id: U32::ZERO,
                    index: U32::ZERO,
                    reserved: U32::ZERO,
                    ntfy_id: U32::new(RX_IRQ_INDEX),
                    queue_resources_addr: U64::new(rx_ring.resources.phys() as u64),
                    rx_desc_ring_addr: U64::new(rx_ring.desc_ring.phys() as u64),
                    rx_data_ring_addr: U64::new(rx_ring.data_ring.phys() as u64),
                    queue_page_list_id: rx_page_list.id().into(),
                    rx_ring_size: dev_descr.rx_queue_entries,
                    packet_buffer_size: U16::new(rounded_mtu),
                },
            )
            .await?;

        let inner = Arc::new(Mutex::new(GvnicInner {
            reg_mmio,
            doorbell_mmio,
            vmar,
            bti,
            dev_descr,
            tx_ring_len,
            rx_ring_len,
            rounded_mtu,
            dma: Some(GvnicDmaResources {
                admin_queue,
                counter_array,
                irq_doorbell_idx_page,
                tx_page_list,
                rx_page_list,
                tx_ring,
                rx_ring,
            }),
            vmo_map: [const { None }; MAX_VMO_IDS],
            netdev_ifc: None,
            netdev_started: false,
            is_started_metric,
        }));

        // Spawn TX, RX, and management interrupt handler tasks on the driver scope.
        let mut tx_irq_stream = Box::pin(OnInterrupt::new(tx_irq));
        let tx_inner = Arc::clone(&inner);
        let mut tx_results = Vec::with_capacity(usize::from(tx_ring_len));
        scope.spawn(async move {
            while let Some(Ok(_)) = tx_irq_stream.as_mut().next().await {
                let _ = Pin::get_ref(tx_irq_stream.as_ref()).as_ref().ack();
                tx_results.clear();
                let ifc = {
                    let mut inner = tx_inner.lock();
                    inner.ack_irq_doorbell(TX_IRQ_INDEX);
                    inner.process_tx_completions(&mut tx_results)
                };
                if let Some(ifc) = ifc {
                    complete_tx_batched(&ifc, &tx_results).await;
                }
            }
        });

        let mut rx_irq_stream = Box::pin(OnInterrupt::new(rx_irq));
        let rx_inner = Arc::clone(&inner);
        let mut rx_buffers = Vec::with_capacity(usize::from(rx_ring_len));
        scope.spawn(async move {
            while let Some(Ok(_)) = rx_irq_stream.as_mut().next().await {
                let _ = Pin::get_ref(rx_irq_stream.as_ref()).as_ref().ack();
                rx_buffers.clear();
                let ifc = {
                    let mut inner = rx_inner.lock();
                    inner.ack_irq_doorbell(RX_IRQ_INDEX);
                    inner.process_rx_completions(&mut rx_buffers)
                };
                if let Some(ifc) = ifc {
                    complete_rx_batched(&ifc, &rx_buffers).await;
                }
            }
        });

        let mut mgmt_irq_stream = Box::pin(OnInterrupt::new(mgmt_irq));
        let mgmt_inner = Arc::clone(&inner);
        scope.spawn(async move {
            while let Some(Ok(_)) = mgmt_irq_stream.as_mut().next().await {
                let _ = Pin::get_ref(mgmt_irq_stream.as_ref()).as_ref().ack();
                let status_update = {
                    let inner = mgmt_inner.lock();
                    inner.netdev_ifc.clone().map(|ifc| (ifc, inner.port_status()))
                };
                if let Some((ifc, status)) = status_update {
                    let _ = ifc.port_status_changed(PORT_ID, status).await;
                }
            }
        });

        Ok(inner)
    }
}

struct NetworkDeviceImplServer {
    inner: Arc<Mutex<GvnicInner>>,
    scope: ScopeHandle,
    tx_results: Vec<TxResult>,
    rx_buffers: Vec<RxBuffer>,
}

impl NetworkDeviceImplServerHandler<DriverChannel> for NetworkDeviceImplServer {
    async fn init(
        &mut self,
        request: Request<Init, DriverChannel>,
        responder: Responder<Init, DriverChannel>,
    ) {
        if self.inner.lock().netdev_ifc.is_some() {
            let _ = responder.respond(Err(Status::ALREADY_BOUND)).await;
            return;
        }

        let ifc_dispatcher = ClientDispatcher::new(request.payload().iface);
        let ifc_client = ifc_dispatcher.client();
        self.scope.spawn(async move {
            let _ = ifc_dispatcher.run(IgnoreEvents).await;
        });

        let (port_client, port_server): (
            ClientEnd<NetworkPort, DriverChannel>,
            ServerEnd<NetworkPort, DriverChannel>,
        ) = create_channel();
        let port_handler =
            NetworkPortServer { inner: Arc::clone(&self.inner), scope: self.scope.clone() };
        port_server.spawn_on(port_handler, &self.scope);

        let status = match ifc_client.add_port(PORT_ID, port_client).await {
            Ok(resp) => resp.status,
            Err(_) => Err(Status::INTERNAL),
        };
        if status.is_ok() {
            self.inner.lock().netdev_ifc = Some(ifc_client);
        }
        let _ = responder.respond(status).await;
    }

    async fn start(&mut self, responder: Responder<Start, DriverChannel>) {
        let status = {
            let mut inner = self.inner.lock();
            if inner.netdev_started {
                Err(Status::ALREADY_BOUND)
            } else if let Some(dma) = &inner.dma {
                let res: GvnicQueueResources = dma.rx_ring.resources.read_element(0);
                let db_idx = res.db_index.get();
                let db_val = dma.rx_ring.doorbell;
                inner.netdev_started = true;
                inner.write_doorbell(db_idx, db_val);
                Ok(())
            } else {
                Err(Status::BAD_STATE)
            }
        };
        let _ = responder.respond(status).await;
    }

    async fn stop(&mut self, responder: Responder<Stop, DriverChannel>) {
        self.tx_results.clear();
        self.rx_buffers.clear();
        let ifc = {
            let mut inner = self.inner.lock();
            inner.netdev_started = false;

            if let Some(dma) = &mut inner.dma {
                self.tx_results.extend(
                    dma.tx_ring
                        .pending_space
                        .drain(..)
                        .map(|id| TxResult { id, status: Err(Status::UNAVAILABLE) }),
                );
                dma.tx_ring.counter_val = dma.tx_ring.doorbell;

                self.rx_buffers.extend(
                    dma.rx_ring.pending_space.drain(..).map(|space| empty_rx_buffer(space.id)),
                );
            }

            inner.netdev_ifc.clone()
        };

        if let Some(ifc) = ifc {
            complete_tx_batched(&ifc, &self.tx_results).await;
            complete_rx_batched(&ifc, &self.rx_buffers).await;
        }
        let _ = responder.respond(()).await;
    }

    async fn get_info(&mut self, responder: Responder<DeviceGetInfo, DriverChannel>) {
        let info = {
            let inner = self.inner.lock();
            let rounded_mtu = u32::from(inner.rounded_mtu);
            DeviceImplInfo {
                device_features: Some(0),
                tx_depth: Some(inner.tx_ring_len),
                rx_depth: Some(inner.rx_ring_len),
                rx_threshold: Some(inner.rx_ring_len / 2),
                max_buffer_parts: Some(1),
                max_buffer_length: Some(rounded_mtu),
                buffer_alignment: Some(1),
                min_rx_buffer_length: Some(rounded_mtu),
                min_tx_buffer_length: Some(0),
                tx_head_length: Some(0),
                tx_tail_length: Some(0),
                ..Default::default()
            }
        };
        let _ = responder.respond(info).await;
    }

    async fn queue_tx(&mut self, request: Request<QueueTx, DriverChannel>) {
        let buffers = request.payload().buffers;
        self.tx_results.clear();
        let ifc = {
            let inner = &mut *self.inner.lock();
            if !inner.netdev_started || inner.dma.is_none() {
                self.tx_results.extend(
                    buffers
                        .into_iter()
                        .map(|b| TxResult { id: b.id, status: Err(Status::UNAVAILABLE) }),
                );
                inner.netdev_ifc.clone()
            } else {
                let mut doorbell_write = None;
                if let Some(dma) = &mut inner.dma {
                    let tx_buf_size = usize::from(dma.tx_ring.buffer_size);
                    let tx_ring_len = dma.tx_ring.len;
                    let mut queued_any = false;

                    for tx in buffers {
                        if tx.data.len() != 1 {
                            self.tx_results
                                .push(TxResult { id: tx.id, status: Err(Status::INVALID_ARGS) });
                            continue;
                        }
                        if dma.tx_ring.pending_space.len() >= usize::from(tx_ring_len) {
                            self.tx_results
                                .push(TxResult { id: tx.id, status: Err(Status::NO_RESOURCES) });
                            continue;
                        }

                        let region = &tx.data[0];
                        let len = usize::try_from(region.length).unwrap_or(usize::MAX);
                        let vmo_offset = usize::try_from(region.offset).unwrap_or(usize::MAX);
                        let Some(vmo) = inner.vmo_map[usize::from(region.vmo)].as_ref() else {
                            self.tx_results
                                .push(TxResult { id: tx.id, status: Err(Status::NOT_FOUND) });
                            continue;
                        };
                        if len > tx_buf_size {
                            self.tx_results
                                .push(TxResult { id: tx.id, status: Err(Status::OUT_OF_RANGE) });
                            continue;
                        }

                        let slot_idx = usize::from(dma.tx_ring.next_free);
                        let qpl_offset = tx_buf_size * slot_idx;
                        if let Err(status) =
                            dma.tx_page_list.copy_from(qpl_offset, vmo, vmo_offset, len)
                        {
                            self.tx_results.push(TxResult { id: tx.id, status: Err(status) });
                            continue;
                        }

                        let len_u16 = U16::new(u16::try_from(len).unwrap_or(0));
                        let pkt_desc = GvnicTxPktDesc {
                            type_flags: GVNIC_TX_DESC_TYPE_PKT,
                            checksum_offset: 0,
                            l4_offset: 0,
                            descriptor_count: 1,
                            len: len_u16,
                            seg_len: len_u16,
                            seg_addr: U64::new(qpl_offset as u64),
                        };
                        dma.tx_ring.ring.write_element(slot_idx, &pkt_desc);

                        dma.tx_ring.next_free = (dma.tx_ring.next_free + 1) % tx_ring_len;
                        dma.tx_ring.doorbell = dma.tx_ring.doorbell.wrapping_add(1);
                        dma.tx_ring.pending_space.push_back(tx.id);
                        queued_any = true;
                    }

                    if queued_any {
                        let res: GvnicQueueResources = dma.tx_ring.resources.read_element(0);
                        doorbell_write = Some((res.db_index.get(), dma.tx_ring.doorbell));
                    }
                }

                if let Some((db_idx, db_val)) = doorbell_write {
                    inner.write_doorbell(db_idx, db_val);
                }

                inner.process_tx_completions(&mut self.tx_results)
            }
        };
        if let Some(ifc) = ifc {
            complete_tx_batched(&ifc, &self.tx_results).await;
        }
    }

    async fn queue_rx_space(&mut self, request: Request<QueueRxSpace, DriverChannel>) {
        let buffers = request.payload().buffers;
        self.rx_buffers.clear();
        let ifc = {
            let mut inner = self.inner.lock();
            if !inner.netdev_started || inner.dma.is_none() {
                self.rx_buffers.extend(buffers.into_iter().map(|b| empty_rx_buffer(b.id)));
                inner.netdev_ifc.clone()
            } else {
                if let Some(dma) = &mut inner.dma {
                    dma.rx_ring.pending_space.extend(buffers);
                }
                inner.process_rx_completions(&mut self.rx_buffers)
            }
        };
        if let Some(ifc) = ifc {
            complete_rx_batched(&ifc, &self.rx_buffers).await;
        }
    }

    async fn prepare_vmo(
        &mut self,
        request: Request<PrepareVmo, DriverChannel>,
        responder: Responder<PrepareVmo, DriverChannel>,
    ) {
        let payload = request.payload();
        let status = {
            let mut inner = self.inner.lock();
            if inner.vmo_map[usize::from(payload.id)].is_some() {
                Err(Status::ALREADY_BOUND)
            } else {
                payload
                    .vmo
                    .get_size()
                    .and_then(|size| usize::try_from(size).map_err(|_| Status::OUT_OF_RANGE))
                    .and_then(|size| {
                        Mapping::create_from_vmo_with_vmar(
                            &payload.vmo,
                            &inner.vmar,
                            size,
                            VmarFlags::PERM_READ | VmarFlags::PERM_WRITE,
                        )
                    })
                    .map(|mapped| {
                        inner.vmo_map[usize::from(payload.id)] = Some(mapped);
                    })
            }
        };
        let _ = responder.respond(status).await;
    }

    async fn release_vmo(
        &mut self,
        request: Request<ReleaseVmo, DriverChannel>,
        responder: Responder<ReleaseVmo, DriverChannel>,
    ) {
        let id = request.payload().id;
        self.inner.lock().vmo_map[usize::from(id)] = None;
        let _ = responder.respond(()).await;
    }
}

struct NetworkPortServer {
    inner: Arc<Mutex<GvnicInner>>,
    scope: ScopeHandle,
}

impl NetworkPortServerHandler<DriverChannel> for NetworkPortServer {
    async fn get_info(&mut self, responder: Responder<PortGetInfo, DriverChannel>) {
        let info = PortBaseInfo {
            port_class: Some(PortClass::Ethernet),
            rx_types: Some(vec![FrameType::Ethernet]),
            tx_types: Some(vec![FrameTypeSupport {
                type_: FrameType::Ethernet,
                features: FRAME_FEATURES_RAW,
                supported_flags: TxFlags::empty(),
            }]),
        };
        let _ = responder.respond(&info).await;
    }

    async fn get_status(&mut self, responder: Responder<GetStatus, DriverChannel>) {
        let status = self.inner.lock().port_status();
        let _ = responder.respond(&status).await;
    }

    async fn set_active(&mut self, _request: Request<SetActive, DriverChannel>) {}

    async fn get_mac(&mut self, responder: Responder<GetMac, DriverChannel>) {
        let mac = self.inner.lock().dev_descr.mac;
        let (mac_client, mac_server): (
            ClientEnd<MacAddr, DriverChannel>,
            ServerEnd<MacAddr, DriverChannel>,
        ) = create_channel();
        mac_server.spawn_on(MacAddrServer { mac }, &self.scope);
        let _ = responder.respond(Some(mac_client)).await;
    }

    async fn removed(&mut self) {}

    async fn get_power_dependency_token(
        &mut self,
        responder: Responder<GetPowerDependencyToken, DriverChannel>,
    ) {
        let _ = responder.respond(None::<zx::Event>).await;
    }
}

struct MacAddrServer {
    mac: [u8; 6],
}

impl MacAddrServerHandler<DriverChannel> for MacAddrServer {
    async fn get_address(&mut self, responder: Responder<GetAddress, DriverChannel>) {
        let addr = MacAddress { octets: self.mac };
        let _ = responder.respond(&addr).await;
    }

    async fn get_features(&mut self, responder: Responder<GetFeatures, DriverChannel>) {
        let features = Features {
            multicast_filter_count: Some(0),
            supported_modes: Some(SupportedMacFilterMode::PROMISCUOUS),
        };
        let _ = responder.respond(&features).await;
    }

    async fn set_mode(
        &mut self,
        _request: Request<SetMode, DriverChannel>,
        responder: Responder<SetMode, DriverChannel>,
    ) {
        let _ = responder.respond(()).await;
    }
}
