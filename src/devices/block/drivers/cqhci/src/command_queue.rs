// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZero;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use anyhow::Context as _;
use async_trait::async_trait;
use block_server::RequestId;
use event_listener::{Event, Listener as _};
use fdf_fidl::DriverChannel;
use fidl_fuchsia_storage_block as fblock;
use fidl_next_fuchsia_hardware_cqhci::{self as cqhci, EmmcPartitionId};
use fidl_next_fuchsia_hardware_rpmb as rpmb;
use fidl_next_fuchsia_hardware_sdmmc as sdmmc;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use log::{debug, error, info, trace, warn};
use mmio::Mmio;
use sdmmc_spec::{
    CQHCI_CQ_CAP_OFFSET, CQHCI_CQ_CFG_OFFSET, CQHCI_CQ_CRA_OFFSET, CQHCI_CQ_CRDCT_OFFSET,
    CQHCI_CQ_CRI_OFFSET, CQHCI_CQ_CRYPTO_CAP_OFFSET, CQHCI_CQ_CRYPTO_NQDUN_OFFSET,
    CQHCI_CQ_CRYPTO_NQIE_OFFSET, CQHCI_CQ_CRYPTO_NQIS_OFFSET, CQHCI_CQ_CRYPTO_NQP_OFFSET,
    CQHCI_CQ_CTL_OFFSET, CQHCI_CQ_DPT_OFFSET, CQHCI_CQ_DQS_OFFSET, CQHCI_CQ_HCCAP_OFFSET,
    CQHCI_CQ_HCCFG_OFFSET, CQHCI_CQ_IC_OFFSET, CQHCI_CQ_IS_OFFSET, CQHCI_CQ_ISGE_OFFSET,
    CQHCI_CQ_ISTE_OFFSET, CQHCI_CQ_RMEM_OFFSET, CQHCI_CQ_SSC1_OFFSET, CQHCI_CQ_SSC2_OFFSET,
    CQHCI_CQ_TCN_OFFSET, CQHCI_CQ_TDBR_OFFSET, CQHCI_CQ_TDLBA_OFFSET, CQHCI_CQ_TDLBAU_OFFSET,
    CQHCI_CQ_TDPE_OFFSET, CQHCI_CQ_TERRI_OFFSET, CQHCI_CQ_VER_OFFSET,
    CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS,
    CQHCI_TASK_DESCRIPTOR_LIST_SIZE, CqhciCqCapsRegister, CqhciCqCfgRegister, CqhciCqCtlRegister,
    CqhciCqInterruptCoalescingRegister, CqhciCqInterruptSignalEnableRegister,
    CqhciCqInterruptStatusEnableRegister, CqhciCqInterruptStatusRegister,
    CqhciCqSendStatusConfiguration2Register, CqhciCqTaskErrorRegister, CqhciCryptoRegisterSnapshot,
    CqhciRegisterSnapshot, Direction, EXT_CSD_BARRIER_EN, EXT_CSD_BARRIER_ENABLED,
    EXT_CSD_BARRIER_SUPPORT, EXT_CSD_BARRIER_SUPPORT_MASK, EXT_CSD_CACHE_CTRL,
    EXT_CSD_CACHE_EN_MASK, EXT_CSD_CACHE_FLUSH_POLICY, EXT_CSD_CACHE_FLUSH_POLICY_FIFO,
    EXT_CSD_FLUSH_CACHE, EXT_CSD_FLUSH_CACHE_FLUSH, EXT_CSD_GENERIC_CMD6_TIME,
    EXT_CSD_PARTITION_ACCESS_MASK, EXT_CSD_PARTITION_CONFIG, EXT_CSD_PARTITON_SWITCH_TIME,
    EXT_CSD_SEC_FEATURE_SUPPORT, EXT_CSD_SEC_FEATURE_SUPPORT_SEC_GB_CL_EN, EXT_CSD_SIZE,
    MMC_BLOCK_SIZE, MMC_ERASE_DISCARD_ARG, MmcCommand, MmcSendStatusResponse, SDHCI_IS_OFFSET,
    SDHCI_ISGE_OFFSET, SDHCI_ISTE_OFFSET, SdhciInterruptSignalEnableRegister,
    SdhciInterruptStatusEnableRegister, SdhciInterruptStatusRegister,
};

use crate::dma_buffer::{ContiguousDmaBuffer, DiscontiguousDmaBuffer, DmaBuffer};
use crate::transfer_manager::{Transfer, TransferManager, TransferOptions};

const IRQ_PORT_IRQ_KEY: u64 = 1;
const IRQ_PORT_LIFELINE_KEY: u64 = 2;
const IRQ_PORT_VIRTUAL_IRQ_ACKED_KEY: u64 = 3;

/// Trait wrapper for fuchsia.hardware.cqhci.Cqhci.
#[async_trait]
pub trait CommandQueueHost: Send + Sync {
    /// Returns information about the CQHCI host.
    async fn info(&self) -> Result<cqhci::CqhciHostInfo, zx::Status>;
    /// Initializes command queueing.  Must be called at most once.
    async fn initialize(
        &self,
        virtual_interrupt: zx::VirtualInterrupt,
        virtual_irq_lifeline: zx::EventPair,
    ) -> Result<CommandQueueResources, zx::Status>;
    /// Enables command queueing.  Must not be called before [`Self::initialize`].
    async fn enable(&self) -> Result<(), zx::Status>;
    /// Disables command queueing.  Must not be called before [`Self::enable`].  The queue can be
    /// later re-enabled by calling [`Self::enable`] again.
    async fn disable(&self) -> Result<(), zx::Status>;
}

#[async_trait]
impl CommandQueueHost for fidl_next::Client<cqhci::Cqhci> {
    async fn info(&self) -> Result<cqhci::CqhciHostInfo, zx::Status> {
        self.host_info()
            .await
            .map_err(|err| {
                error!(err:?; "FIDL error");
                zx::Status::INTERNAL
            })?
            .map(|response| response.info)
    }

    async fn initialize(
        &self,
        virtual_interrupt: zx::VirtualInterrupt,
        virtual_irq_lifeline: zx::EventPair,
    ) -> Result<CommandQueueResources, zx::Status> {
        let sdmmc::CqhciInitializeCommandQueueingResponse {
            cqhci_mmio,
            cqhci_mmio_offset,
            sdhci_mmio,
            sdhci_mmio_offset,
            bti,
            interrupt,
        } = self
            .initialize_command_queueing(virtual_interrupt, virtual_irq_lifeline)
            .await
            .map_err(|err| {
                error!(err:?; "FIDL error");
                zx::Status::INTERNAL
            })?
            .map_err(|err| {
                error!(err:?; "Failed to initialize CQHCI");
                zx::Status::from_raw(err)
            })?;
        let cqhci_mmio = {
            let vmo_len = cqhci_mmio.get_size()?;
            let m = mmio::vmo::VmoMapping::map(
                cqhci_mmio_offset as usize,
                vmo_len as usize,
                cqhci_mmio,
            )?;
            Box::new(m) as Box<dyn Mmio + Send + Sync>
        };
        let sdhci_mmio = {
            let vmo_len = sdhci_mmio.get_size()?;
            let m = mmio::vmo::VmoMapping::map(
                sdhci_mmio_offset as usize,
                vmo_len as usize,
                sdhci_mmio,
            )?;
            Box::new(m) as Box<dyn Mmio + Send + Sync>
        };
        Ok(CommandQueueResources { cqhci_mmio, sdhci_mmio, bti, interrupt })
    }

    async fn enable(&self) -> Result<(), zx::Status> {
        self.enable_cqhci()
            .await
            .map_err(|err| {
                error!(err:?; "FIDL error");
                zx::Status::INTERNAL
            })?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    async fn disable(&self) -> Result<(), zx::Status> {
        self.disable_cqhci()
            .await
            .map_err(|err| {
                error!(err:?; "FIDL error");
                zx::Status::INTERNAL
            })?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }
}

pub struct CommandQueueResources {
    pub cqhci_mmio: Box<dyn Mmio + Send + Sync>,
    pub sdhci_mmio: Box<dyn Mmio + Send + Sync>,
    pub bti: zx::Bti,
    pub interrupt: zx::Interrupt,
}

pub trait TaskStatusReceiver: Send + Sync + 'static {
    /// A callback to invoke upon task completion.
    fn complete(&self, request_id: RequestId, status: zx::Status);
}

impl<T: TaskStatusReceiver + ?Sized> TaskStatusReceiver for Weak<T> {
    fn complete(&self, request_id: RequestId, status: zx::Status) {
        if let Some(r) = self.upgrade() {
            r.complete(request_id, status);
        }
    }
}

/// Helper to complete a request if the receiver is still running.
fn complete_request(
    receiver: Option<Arc<dyn TaskStatusReceiver>>,
    request_id: RequestId,
    status: zx::Status,
) {
    debug!("Complete {request_id:?}: {status:?}");
    if let Some(receiver) = receiver {
        receiver.complete(request_id, status);
    }
}

#[derive(Debug)]
struct PendingTask {
    request_id: RequestId,
    partition: EmmcPartitionId,
    transfer: Transfer,
    trace_flow_id: Option<NonZero<u64>>,
}

impl PendingTask {
    /// Completes the task.  Must be called after the hardware will no longer access the transfer
    /// region.
    ///
    /// # SAFETY
    ///
    /// This MUST be called when the hardware will no longer access the memory pointed to by
    /// `transfer` (either before it was submitted, or after it completes).
    unsafe fn complete(self, status_receiver: Weak<dyn TaskStatusReceiver>, status: zx::Status) {
        // Order is important.  We have to:
        // 1. Invalidate CPU caches (so the transferred data is visible to the client),
        // 2. Call [`Transfer::unpin`], which unpins the pages, then
        // 3. Call the completer (which may send a response to the client).
        let Self { request_id, transfer, trace_flow_id, .. } = self;
        fuchsia_trace::duration!("sdmmc", "cqhci::complete_transfer",
            "slot" => transfer.tdl_slot() as u64,
            "op" => transfer.opcode(),
            "status" => status.into_raw());
        if let Some(trace_flow_id) = trace_flow_id {
            fuchsia_trace::flow_step!(
                "storage",
                "cqhci::complete_transfer",
                trace_flow_id.get().into()
            );
        }
        transfer.cache_invalidate();
        // SAFETY: By the caller's contract.
        unsafe { transfer.unpin() };
        complete_request(status_receiver.upgrade(), request_id, status);
    }
}

#[derive(Default)]
struct PendingTasks {
    tasks: [Option<PendingTask>; CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS - 1],
    num_tasks: usize,
    dcmd_status: Option<zx::Status>,
}

impl PendingTasks {
    fn is_empty(&self) -> bool {
        self.num_tasks == 0
    }

    fn add_task(&mut self, slot_id: u8, task: PendingTask) {
        assert!(self.tasks[slot_id as usize].is_none());
        self.tasks[slot_id as usize] = Some(task);
        self.num_tasks += 1;
    }

    fn take_task(&mut self, slot_id: u8, event: &Event) -> Option<PendingTask> {
        let task = std::mem::take(&mut self.tasks[slot_id as usize]);
        if task.is_some() {
            let needs_wake = self.num_tasks == 1 || self.num_tasks == self.tasks.len();
            self.num_tasks -= 1;
            if needs_wake {
                event.notify(usize::MAX);
            }
        }
        task
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// CQE is enabled.  This is a prerequisite for submitting any task to hardware.
    Enabled,

    /// CQE is disabled, a.k.a. shut-down.  This is not the same as when the command queueing engine
    /// is disabled whilst processing RPMB requests (in that case, the state will be `Enabled`).
    Disabled,

    /// CQE is suspended (for power).
    Suspended,
}

/// AsyncTask encapsulates asynchronous tasks that need to run with exclusive access
/// to the command queue.  Concrete implementations will typically want to implement
/// Drop to clean up in the case that tasks are dropped.
#[async_trait]
trait AsyncTask: Send + 'static {
    /// Called to execute the task.
    async fn run(self: Box<Self>, cq: CommandQueueExcl);
}

/// Returns an asynchronous task that runs `func`.  `callback` will be called with
/// the result.
fn into_async_task<Fut: Future<Output = Result<(), zx::Status>> + Send + 'static>(
    func: impl FnOnce(CommandQueueExcl) -> Fut + Send + 'static,
    callback: impl FnOnce(Result<(), zx::Status>) + Send + 'static,
) -> impl AsyncTask {
    struct Wrapper<F, C: FnOnce(Result<(), zx::Status>)> {
        func: Option<F>,
        callback: Option<C>,
    }

    #[async_trait]
    impl<
        F: FnOnce(CommandQueueExcl) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), zx::Status>> + Send + 'static,
        C: FnOnce(Result<(), zx::Status>) + Send + 'static,
    > AsyncTask for Wrapper<F, C>
    {
        async fn run(mut self: Box<Self>, cq: CommandQueueExcl) {
            (self.callback.take().unwrap())((self.func.take().unwrap())(cq).await);
        }
    }

    impl<F, C: FnOnce(Result<(), zx::Status>)> Drop for Wrapper<F, C> {
        fn drop(&mut self) {
            if let Some(cb) = self.callback.take() {
                cb(Err(zx::Status::CANCELED));
            }
        }
    }

    Wrapper { func: Some(func), callback: Some(callback) }
}

struct Inner {
    state: State,
    /// Whether recovery is required due to a hardware error.
    needs_recovery: bool,
    /// Whether the CQE is shutting down.  All in-flight tasks will be canceled and no new requests
    /// shall be submitted.
    shutting_down: bool,
    /// Whether requests are blocked (typically because an async task is currently running).
    blocked: bool,
    /// The queue of asynchronous tasks.
    async_task_queue: VecDeque<Box<dyn AsyncTask>>,
    /// An event which is used to wake up various tasks when the queue state changes.
    /// Specifically, the event is fired in all of the following scenarios (and possibly more):
    /// - When `state` changes.
    /// - When transfers or dcmds complete.
    /// - During shutdown.
    event: Event,
    /// The currently active partition.  Switching this requires blocking the queue and executing a
    /// switch DCMD; see [`CommandQueueExclExcl::switch_and_submit`].
    active_partition: Option<EmmcPartitionId>,
    /// In-flight tasks.
    pending_tasks: PendingTasks,
    cqhci_mmio: Box<dyn Mmio + Send + Sync>,
    sdhci_mmio: Box<dyn Mmio + Send + Sync>,
    /// Runs async tasks in a loop.
    async_task_loop: Option<fasync::Task<()>>,
    /// Drop to signal the SDHCI driver to resume handling physical interrupts.
    virtual_irq_lifeline: Option<zx::EventPair>,
    /// Handles IRQ events.
    irq_thread: Option<JoinHandle<()>>,
    /// Drop to shut down the IRQ thread.
    irq_lifeline: Option<zx::EventPair>,
    partition_status_receivers: BTreeMap<EmmcPartitionId, Weak<dyn TaskStatusReceiver>>,
}

impl Inner {
    /// If `true`, then new tasks should be rejected.
    ///
    /// Note that returning true doesn't mean that the task can be submitted yet.  The caller must
    /// also check [`Self::should_wait_to_submit_tasks`].
    fn should_reject_tasks(&self) -> bool {
        self.shutting_down || self.state == State::Disabled
    }

    /// If `true`, then the caller should wait before attempting to submit the new task.
    fn should_wait_to_submit_tasks(&self) -> bool {
        !self.async_task_queue.is_empty()
            || self.state != State::Enabled
            || self.blocked
            || self.needs_recovery
    }

    fn snapshot_regs(&self, capabilities: &CqhciCqCapsRegister) -> CqhciRegisterSnapshot {
        let cqhci_mmio = &self.cqhci_mmio;
        CqhciRegisterSnapshot {
            ver: cqhci_mmio.load32(CQHCI_CQ_VER_OFFSET),
            caps: cqhci_mmio.load32(CQHCI_CQ_CAP_OFFSET),
            cfg: cqhci_mmio.load32(CQHCI_CQ_CFG_OFFSET),
            ctl: cqhci_mmio.load32(CQHCI_CQ_CTL_OFFSET),
            is: cqhci_mmio.load32(CQHCI_CQ_IS_OFFSET),
            iste: cqhci_mmio.load32(CQHCI_CQ_ISTE_OFFSET),
            isge: cqhci_mmio.load32(CQHCI_CQ_ISGE_OFFSET),
            tdlba: cqhci_mmio.load32(CQHCI_CQ_TDLBA_OFFSET),
            tdlbau: cqhci_mmio.load32(CQHCI_CQ_TDLBAU_OFFSET),
            dbr: cqhci_mmio.load32(CQHCI_CQ_TDBR_OFFSET),
            tcn: cqhci_mmio.load32(CQHCI_CQ_TCN_OFFSET),
            dqs: cqhci_mmio.load32(CQHCI_CQ_DQS_OFFSET),
            dpt: cqhci_mmio.load32(CQHCI_CQ_DPT_OFFSET),
            tdpe: cqhci_mmio.load32(CQHCI_CQ_TDPE_OFFSET),
            ssc1: cqhci_mmio.load32(CQHCI_CQ_SSC1_OFFSET),
            ssc2: cqhci_mmio.load32(CQHCI_CQ_SSC2_OFFSET),
            rmem: cqhci_mmio.load32(CQHCI_CQ_RMEM_OFFSET),
            terri: cqhci_mmio.load32(CQHCI_CQ_TERRI_OFFSET),
            cri: cqhci_mmio.load32(CQHCI_CQ_CRI_OFFSET),
            cra: cqhci_mmio.load32(CQHCI_CQ_CRA_OFFSET),
            hccap: cqhci_mmio.load32(CQHCI_CQ_HCCAP_OFFSET),
            hccfg: cqhci_mmio.load32(CQHCI_CQ_HCCFG_OFFSET),
            crypto: capabilities.crypto_support().then(|| CqhciCryptoRegisterSnapshot {
                crnqp: cqhci_mmio.load32(CQHCI_CQ_CRYPTO_NQP_OFFSET),
                crnqdun: cqhci_mmio.load32(CQHCI_CQ_CRYPTO_NQDUN_OFFSET),
                crnqis: cqhci_mmio.load32(CQHCI_CQ_CRYPTO_NQIS_OFFSET),
                crnqie: cqhci_mmio.load32(CQHCI_CQ_CRYPTO_NQIE_OFFSET),
                crcap: cqhci_mmio.load32(CQHCI_CQ_CRYPTO_CAP_OFFSET),
            }),
        }
    }

    fn dump_regs(&self, capabilities: &CqhciCqCapsRegister) {
        info!("{:?}", self.snapshot_regs(capabilities));
    }

    /// Enables SDHCI interrupts and the selected set of CQHCI interrupts.
    fn enable_interrupts(&mut self, cqhci_interrupts: CqhciCqInterruptSignalEnableRegister) {
        self.cqhci_mmio.write_barrier();

        self.sdhci_mmio
            .store32(SDHCI_ISGE_OFFSET, SdhciInterruptSignalEnableRegister::enabled().raw());
        self.sdhci_mmio
            .store32(SDHCI_ISTE_OFFSET, SdhciInterruptStatusEnableRegister::enabled().raw());
        self.cqhci_mmio.write_barrier();

        self.cqhci_mmio.store32(CQHCI_CQ_ISGE_OFFSET, cqhci_interrupts.raw());
        self.cqhci_mmio.store32(
            CQHCI_CQ_ISTE_OFFSET,
            CqhciCqInterruptStatusEnableRegister::from_raw(cqhci_interrupts.raw()).raw(),
        );
        self.cqhci_mmio.write_barrier();
    }

    /// Disables SDHCI and CQHCI interrupts.
    fn disable_interrupts(&mut self) {
        self.cqhci_mmio.write_barrier();

        self.cqhci_mmio
            .store32(CQHCI_CQ_ISGE_OFFSET, CqhciCqInterruptSignalEnableRegister::disabled().raw());
        self.cqhci_mmio
            .store32(CQHCI_CQ_ISTE_OFFSET, CqhciCqInterruptStatusEnableRegister::disabled().raw());
        self.cqhci_mmio.write_barrier();

        self.sdhci_mmio
            .store32(SDHCI_ISGE_OFFSET, SdhciInterruptSignalEnableRegister::disabled().raw());
        self.sdhci_mmio
            .store32(SDHCI_ISTE_OFFSET, SdhciInterruptStatusEnableRegister::disabled().raw());
        self.cqhci_mmio.write_barrier();
    }

    /// Unhalts the Command Queue Engine, allowing it to process commands.
    ///
    /// This should only be called after [`CommandQueue::halt`], although it is idempotent.
    fn unhalt(&mut self) {
        self.cqhci_mmio.store32(CQHCI_CQ_CTL_OFFSET, 0);
        self.enable_interrupts(CqhciCqInterruptSignalEnableRegister::enabled());
    }

    /// Submits a transfer to hardware.
    fn submit_transfer(&mut self, tdl_slot: u8, task: PendingTask) {
        debug_assert_eq!(self.state, State::Enabled);
        trace!("Submitting transfer {tdl_slot}");
        // Execute a write barrier, so the transfer descriptor's contents are visible *before* we
        // ring the doorbell.
        self.cqhci_mmio.write_barrier();
        self.cqhci_mmio.store32(CQHCI_CQ_TDBR_OFFSET, 1u32 << tdl_slot);
        self.pending_tasks.add_task(tdl_slot, task);
    }

    /// Fills `output` with additional tasks based on the slots identified by `completed_mask`.
    ///
    /// If a DCMD was completed, signals its event immediately.
    fn take_complete(
        &mut self,
        mut completed_mask: u32,
        status: zx::Status,
        output: &mut CompletedTasks,
    ) {
        while completed_mask > 0 {
            let slot = completed_mask.trailing_zeros() as u8;
            if slot == CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT {
                self.pending_tasks.dcmd_status = Some(status);
                self.event.notify(usize::MAX);
            } else if let Some(task) = self.pending_tasks.take_task(slot, &self.event) {
                let Some(receiver) = self.partition_status_receivers.get(&task.partition) else {
                    panic!("No receiver was registered for partition {:?}", task.partition);
                };
                output.add(task, receiver.clone(), status);
            }
            completed_mask &= !(1 << slot);
        }
    }

    fn get_request_completer(
        &self,
        partition: EmmcPartitionId,
    ) -> Option<Arc<dyn TaskStatusReceiver>> {
        let Some(receiver) = self.partition_status_receivers.get(&partition) else {
            panic!("No receiver was registered for partition {:?}", partition);
        };
        receiver.upgrade()
    }

    /// Submits an async task to the command queue.
    fn submit_async_task(&mut self, task: impl AsyncTask) {
        if self.should_reject_tasks() {
            // Tasks need to handle drop to return errors as needed.
            return;
        }
        debug_assert!(self.async_task_loop.is_some());

        self.async_task_queue.push_back(Box::new(task));
        if self.async_task_queue.len() == 1 {
            self.event.notify_additional(usize::MAX);
        }
    }
}

/// Collects a list of completed tasks.
///
/// The caller must later call [`CompletedTasks::complete`] to complete the tasks and unpin their
/// memory.
#[derive(Default)]
struct CompletedTasks {
    tasks: [Option<(PendingTask, Weak<dyn TaskStatusReceiver>, zx::Status)>;
        CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS - 1],
    count: usize,
}

impl CompletedTasks {
    fn add(
        &mut self,
        task: PendingTask,
        receiver: Weak<dyn TaskStatusReceiver>,
        status: zx::Status,
    ) {
        self.tasks[self.count] = Some((task, receiver, status));
        self.count += 1;
    }

    /// Completes all tasks in the list.
    ///
    /// SAFETY: The caller must ensure that all of the tasks are no longer in-flight in hardware.
    unsafe fn complete(mut self) {
        for entry in &mut self.tasks[..self.count] {
            // Unwrap OK since we only add tasks via [`CompletedTasks::add`]
            let (task, receiver, status) = entry.take().unwrap();
            // SAFETY: Contract of the caller.
            unsafe { task.complete(receiver, status) };
        }
        self.count = 0;
    }
}

impl Drop for CompletedTasks {
    fn drop(&mut self) {
        // Complete requests (so we don't block clients forever), but don't unpin the buffers, since
        // we don't know for sure that the requests are done.
        if self.count > 0 {
            warn!("CompletedTasks dropped without completing!  Pinned buffers will be leaked.");
        }
        for entry in &mut self.tasks[..self.count] {
            // Unwrap OK since we only add tasks via [`CompletedTasks::add`]
            let (task, receiver, status) = entry.take().unwrap();
            task.transfer.cache_invalidate();
            complete_request(receiver.upgrade(), task.request_id, status);
        }
    }
}

/// Switch to the target partition and submit a single transfer request.
///
/// The switch and transfer are combined into a single atomic task, to avoid the possibility of
/// another switch happening after the switch completes but before the transfer is submitted.
struct SwitchAndSubmitTask {
    partition: EmmcPartitionId,
    task: Option<PendingTask>,
    receiver: Weak<dyn TaskStatusReceiver>,
}

#[async_trait]
impl AsyncTask for SwitchAndSubmitTask {
    async fn run(mut self: Box<Self>, mut cq: CommandQueueExcl) {
        debug!("switch_partition {:?}", self.partition);
        let partition_config_value = cq.ext_csd[EXT_CSD_PARTITION_CONFIG]
            & EXT_CSD_PARTITION_ACCESS_MASK
            | self.partition as u8;
        let res = cq.do_switch(EXT_CSD_PARTITION_CONFIG, partition_config_value).await;
        let mut inner = cq.inner.lock();
        if res.is_ok() {
            inner.active_partition = Some(self.partition);
            let task = self.task.take().unwrap();
            let tdl = task.transfer.tdl_slot();
            inner.submit_transfer(tdl, task);
        } else {
            let Some(receiver) = inner.partition_status_receivers.get(&self.partition) else {
                panic!("No receiver was registered for partition {:?}", self.partition);
            };
            let task = self.task.take().unwrap();
            // SAFETY: We never submitted the transfer.
            unsafe { task.complete(receiver.clone(), res.clone().into()) };
        }
        debug!("switch_partition {:?} done: {res:?}", self.partition);
    }
}

impl Drop for SwitchAndSubmitTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // SAFETY: We never submitted the transfer.
            unsafe { task.complete(self.receiver.clone(), zx::Status::CANCELED) };
        }
    }
}

struct RecoveryTask;

#[async_trait]
impl AsyncTask for RecoveryTask {
    async fn run(self: Box<Self>, mut cq: CommandQueueExcl) {
        if let Err(error) = cq.run_recovery().await {
            warn!(error:?; "Recovery failed");
        }
    }
}

/// A guard representing exclusive access to the command queue.
///
/// Only one of this struct may exist at any time, so the caller has unique access to modify the
/// state of the command queue while holding this struct.
///
/// This guard serves two purposes:
///
/// 1. Methods on this struct indicate that no additional transfers or tasks will be submitted for
///    the duration of the method.
/// 2. The guard automatically unblocks the queue on drop.
struct CommandQueueExcl {
    queue: Arc<CommandQueue>,
}

impl CommandQueueExcl {
    fn new(queue: Arc<CommandQueue>, inner: &mut Inner) -> Self {
        inner.blocked = true;
        Self { queue }
    }
}

impl std::ops::Deref for CommandQueueExcl {
    type Target = CommandQueue;

    fn deref(&self) -> &Self::Target {
        self.queue.as_ref()
    }
}

impl Drop for CommandQueueExcl {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        inner.blocked = false;
        inner.event.notify(usize::MAX);
    }
}

impl CommandQueueExcl {
    /// Enables the hardware.  NOTE: This does *not* change `state`.
    async fn enable(&mut self) -> Result<(), zx::Status> {
        self.host.enable().await?;
        {
            let mut inner = self.inner.lock();

            // After the `host.enable()` above, the active partition should always be the user data
            // partition.
            inner.active_partition = Some(EmmcPartitionId::UserDataPartition);

            // Disable CQ so we can configure it
            let mut cqcfg =
                CqhciCqCfgRegister::from_bits_retain(inner.cqhci_mmio.load32(CQHCI_CQ_CFG_OFFSET));
            if cqcfg.contains(CqhciCqCfgRegister::CQE_ENABLE) {
                cqcfg.remove(CqhciCqCfgRegister::CQE_ENABLE);
                inner.cqhci_mmio.store32(CQHCI_CQ_CFG_OFFSET, cqcfg.bits());
            }

            // Configure Task Descriptor size and DCMD in CQCFG Register.
            cqcfg.insert(CqhciCqCfgRegister::TASK_DESC_128);
            cqcfg.insert(CqhciCqCfgRegister::DCMD_ENABLE);
            if self.capabilities.crypto_support() {
                cqcfg.insert(CqhciCqCfgRegister::CRYPTO_ENABLE);
            }
            inner.cqhci_mmio.store32(CQHCI_CQ_CFG_OFFSET, cqcfg.bits());

            // Configure CQTDLBA and CQTDLBAU to point to the memory location allocated to the TDL
            // in host memory
            let tdl_paddr = self.transfer_manager.tdl_address();
            let (tdl_paddr_hi, tdl_paddr_lo) = ((tdl_paddr >> 32) as u32, tdl_paddr as u32);
            inner.cqhci_mmio.store32(CQHCI_CQ_TDLBA_OFFSET, tdl_paddr_lo);
            inner.cqhci_mmio.store32(CQHCI_CQ_TDLBAU_OFFSET, tdl_paddr_hi);

            // Ack any interrupts which are asserted
            let is = inner.cqhci_mmio.load32(CQHCI_CQ_IS_OFFSET);
            inner.cqhci_mmio.store32(CQHCI_CQ_IS_OFFSET, is);
            inner.cqhci_mmio.store32(
                CQHCI_CQ_ISGE_OFFSET,
                CqhciCqInterruptSignalEnableRegister::disabled().raw(),
            );
            inner.cqhci_mmio.store32(
                CQHCI_CQ_ISTE_OFFSET,
                CqhciCqInterruptStatusEnableRegister::disabled().raw(),
            );

            inner.cqhci_mmio.store32(
                CQHCI_CQ_SSC2_OFFSET,
                CqhciCqSendStatusConfiguration2Register::from_rca(self.rca).raw(),
            );

            // Disable interrupt coalescing
            inner
                .cqhci_mmio
                .store32(CQHCI_CQ_IC_OFFSET, CqhciCqInterruptCoalescingRegister::disabled().raw());

            // Issue a write barrier so the new configuration is visible to hardware before we
            // enable CQE.
            inner.cqhci_mmio.write_barrier();

            let mut cqcfg =
                CqhciCqCfgRegister::from_bits_retain(inner.cqhci_mmio.load32(CQHCI_CQ_CFG_OFFSET));
            cqcfg.insert(CqhciCqCfgRegister::CQE_ENABLE);
            inner.cqhci_mmio.store32(CQHCI_CQ_CFG_OFFSET, cqcfg.bits());
            inner.unhalt();
            inner.event.notify(usize::MAX);
        }
        debug!("CQHCI enabled");
        Ok(())
    }

    /// Disables the hardware.  NOTE: This does *not* change `state`.
    async fn disable(&mut self) {
        self.halt().await;
        {
            let mut inner = self.inner.lock();
            inner.disable_interrupts();
            let mut cqcfg =
                CqhciCqCfgRegister::from_bits_retain(inner.cqhci_mmio.load32(CQHCI_CQ_CFG_OFFSET));
            cqcfg.remove(CqhciCqCfgRegister::CQE_ENABLE);
            inner.cqhci_mmio.store32(CQHCI_CQ_CFG_OFFSET, cqcfg.bits());
            // Issue a write barrier so the CQE is disabled before we issue the commands to disable
            // command queueing mode in the underlying driver.
            inner.cqhci_mmio.write_barrier();
            inner.event.notify(usize::MAX);
        }
        let _ = self.host.disable().await;
        debug!("CQHCI disabled");
    }

    /// Halts the CQE, preventing it from processing any further commands.
    async fn halt(&mut self) {
        {
            let mut inner = self.inner.lock();
            inner.disable_interrupts();
            let mut ctl =
                CqhciCqCtlRegister::from_bits_retain(inner.cqhci_mmio.load32(CQHCI_CQ_CTL_OFFSET));
            ctl.insert(CqhciCqCtlRegister::HALT);
            inner.cqhci_mmio.store32(CQHCI_CQ_CTL_OFFSET, ctl.bits());
            inner.cqhci_mmio.write_barrier();
        }
        // Per JESD84-B51A Annex B, poll until the HALT bit is set.  There is also a HAC interrupt
        // bit which can be used, but since we use this for recovery and the device may be in a bad
        // state, polling is safer.
        const HALT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(500);

        // NOTE: for tests, the deadline is disabled because it can cause test flakes.
        const HALT_DEADLINE: std::time::Duration = std::time::Duration::from_millis(100);

        let start = std::time::Instant::now();
        loop {
            let ctl = CqhciCqCtlRegister::from_bits_retain(
                self.inner.lock().cqhci_mmio.load32(CQHCI_CQ_CTL_OFFSET),
            );
            if ctl.contains(CqhciCqCtlRegister::HALT) {
                break;
            } else if cfg!(not(test)) && start.elapsed() >= HALT_DEADLINE {
                warn!("Failed to halt CQE after deadline; assuming stalled");
                break;
            }
            fasync::Timer::new(HALT_POLL_INTERVAL).await;
        }
        trace!("Halted");
    }

    /// Executes a DCMD, waiting for its completion.
    async fn execute_dcmd(
        &mut self,
        command: MmcCommand,
        command_arg: u32,
    ) -> Result<u32, zx::Status> {
        let mut dcmd = None;
        loop {
            let listener = {
                let mut inner = self.inner.lock();

                // The queue must be empty for CMD6 (see JESD84-B51B 6.6.39.1).
                assert!(command != MmcCommand::Switch || inner.pending_tasks.is_empty());

                match &mut dcmd {
                    Some(_) if inner.pending_tasks.dcmd_status.is_some() => {
                        // The command is complete, check its status.
                        let status = inner.pending_tasks.dcmd_status.take().unwrap();
                        trace!("Dcmd {command:?} completed: {status:?}");
                        return match status.into() {
                            Ok(()) => Ok(inner.cqhci_mmio.load32(CQHCI_CQ_CRDCT_OFFSET)),
                            Err(status) => Err(status),
                        };
                    }
                    Some(_) => {
                        // The command is in flight, wait for it to complete.
                        inner.event.listen()
                    }
                    None => {
                        // Submit the command.
                        if let Some(slot) = self.transfer_manager.acquire_dcmd_slot() {
                            self.transfer_manager.prepare_dcmd(&slot, command, command_arg)?;
                            dcmd = Some(slot);
                            trace!("Submitting dcmd {command:?}");
                            // Execute a write barrier, so the descriptor's contents are visible
                            // *before* we ring the doorbell.
                            inner.cqhci_mmio.write_barrier();
                            inner.cqhci_mmio.store32(
                                CQHCI_CQ_TDBR_OFFSET,
                                1u32 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT,
                            );
                            inner.pending_tasks.dcmd_status = None;
                        }
                        inner.event.listen()
                    }
                }
            };
            listener.await;
        }
    }

    /// Executes an CMD6 (SWITCH) and queries the command status with CMD13 (SEND_STATUS).
    async fn do_switch(&mut self, index: usize, value: u8) -> Result<(), zx::Status> {
        self.execute_dcmd(
            MmcCommand::Switch,
            3u32 << 24 // write byte
                | (index as u32) << 16 | (value as u32) << 8,
        )
        .await?;
        // These fields defines a maximum timeout value for CMD6 in tens of milliseconds.  There
        // does not appear to be any other way to check the status of CMD6, so just sleep for the
        // maximum required time before issuing CMD13.
        let switch_time = match index {
            EXT_CSD_FLUSH_CACHE => 0,
            EXT_CSD_PARTITION_CONFIG => {
                let mut switch_time = self.ext_csd[EXT_CSD_PARTITON_SWITCH_TIME];
                if switch_time == 0 {
                    switch_time = self.ext_csd[EXT_CSD_GENERIC_CMD6_TIME];
                }
                switch_time
            }
            _ => self.ext_csd[EXT_CSD_GENERIC_CMD6_TIME],
        };
        if switch_time > 0 {
            trace!("Wait for switch {}ms", 10 * switch_time as u64);
            fasync::Timer::new(std::time::Duration::from_millis(10 * switch_time as u64)).await;
        }
        let status = MmcSendStatusResponse::from_bits_retain(
            self.execute_dcmd(MmcCommand::SendStatus, (self.rca as u32) << 16).await?,
        );
        if status.contains(MmcSendStatusResponse::SWITCH_ERR) {
            warn!("Switch error detected, idx {index} val {value} st {status:?}");
            return Err(zx::Status::IO);
        }
        Ok(())
    }

    async fn initialize_inner(&mut self) -> Result<(), zx::Status> {
        debug!("initializing");
        self.enable().await.inspect_err(|err| {
            error!(err:?; "Failed to enable CQE");
        })?;

        if self.supports_barriers() {
            // Ensure barriers are enabled
            info!("Barriers supported");
            self.do_switch(EXT_CSD_BARRIER_EN, EXT_CSD_BARRIER_ENABLED).await.inspect_err(
                |err| {
                    error!(err:?; "Failed to enable barriers");
                },
            )?;
        }
        if self.supports_trim() {
            info!("TRIM enabled");
        }
        if self.cache_enabled() {
            let fifo = if self.cache_policy_fifo() { "FIFO" } else { "non-FIFO" };
            info!("Cache enabled, policy {fifo}");
        }
        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), zx::Status> {
        let res = self.initialize_inner().await;
        if res.is_err() {
            // Make sure we clean up if initialization fails.
            let _ = self.shutdown().await;
        }
        res
    }

    async fn shutdown(&mut self) -> Result<(), zx::Status> {
        debug!("shutting down cqhci");
        self.disable().await;

        // Complete all tasks.  Normally self gets done by the IRQ thread which we're about to shut
        // down.
        debug!("Cancelling all tasks");
        {
            let mut completed_tasks = CompletedTasks::default();
            self.inner.lock().take_complete(u32::MAX, zx::Status::CANCELED, &mut completed_tasks);
            // SAFETY: CQE is disabled.
            unsafe { completed_tasks.complete() }
        }

        // Stop handling interrupts.  To avoid races, shut our IRQ thread down first, then destroy
        // the virtual interrupt to signal to the server to start handling the physical IRQ again.
        debug!("Joining IRQ thread");
        let irq_thread = {
            let mut inner = self.inner.lock();
            inner.irq_lifeline = None;
            inner.irq_thread.take().unwrap()
        };
        if let Err(err) = fasync::unblock(move || irq_thread.join()).await {
            warn!(err:?; "Failed to join irq thread");
        }
        debug!("IRQ thread joined.");
        let mut inner = self.inner.lock();
        inner.virtual_irq_lifeline = None;
        inner.async_task_queue.clear();
        Ok(())
    }

    async fn rpmb_request(&mut self, request: rpmb::Request) -> Result<(), zx::Status> {
        // The RPMB partition can only be accessed while command queueing is disabled.
        debug!("rpmb request {request:?}");
        self.disable().await;
        let res = self.rpmb.request(request).await.map_err(|_| zx::Status::INTERNAL).flatten();
        if let Err(err) = self.enable().await {
            error!(err:?; "Failed to re-enable CQE!");
            Err(zx::Status::IO)
        } else {
            res
        }
    }

    async fn trim(
        &mut self,
        partition: EmmcPartitionId,
        block_offset: u64,
        block_count: u32,
    ) -> Result<(), zx::Status> {
        if block_count == 0 {
            return Ok(());
        }
        let Ok(block_offset) = u32::try_from(block_offset) else {
            log::warn!("Trim block offset too large; CQHCI trim only supports 32-bit offsets");
            return Err(zx::Status::INVALID_ARGS);
        };
        let Some(end_offset) = block_offset.checked_add(block_count - 1) else {
            log::warn!("Trim end offset overflow");
            return Err(zx::Status::INVALID_ARGS);
        };
        debug!("Trim {block_offset:x} {block_count:x} {partition:?}");

        if self.inner.lock().active_partition != Some(partition) {
            let partition_config_value = self.ext_csd[EXT_CSD_PARTITION_CONFIG]
                & EXT_CSD_PARTITION_ACCESS_MASK
                | partition as u8;
            self.do_switch(EXT_CSD_PARTITION_CONFIG, partition_config_value).await?;
            self.inner.lock().active_partition = Some(partition);
        }

        self.execute_dcmd(MmcCommand::EraseGroupStart, block_offset).await.map(|_| ())?;
        self.execute_dcmd(MmcCommand::EraseGroupEnd, end_offset).await.map(|_| ())?;
        self.execute_dcmd(MmcCommand::Erase, MMC_ERASE_DISCARD_ARG).await.map(|_| ())
    }

    async fn run_recovery(&mut self) -> Result<(), zx::Status> {
        self.inner.lock().dump_regs(&self.capabilities);
        self.disable().await;
        {
            let mut completed_tasks = CompletedTasks::default();
            let mut inner = self.inner.lock();
            inner.take_complete(u32::MAX, zx::Status::IO, &mut completed_tasks);
            // SAFETY: CQE is disabled.
            unsafe {
                completed_tasks.complete();
            }
            // Reset `needs_recovery` now in case the interrupt handler finds another error.
            inner.needs_recovery = false;
        }
        let res = match self.enable().await {
            Ok(_) => {
                info!("Recovery complete");
                Ok(())
            }
            Err(error) => {
                error!(error:?; "Failed to re-enable CQE!");
                Err(zx::Status::BAD_STATE)
            }
        };
        {
            // Whether we succeeded or not, notify other tasks that we're done.
            let mut inner = self.inner.lock();
            if res.is_err() {
                inner.state = State::Disabled;
            }
            inner.event.notify(usize::MAX);
        }
        res
    }
}

enum SubmitResult {
    Done(zx::Status),
    ShouldWait(event_listener::EventListener),
}

pub struct CommandQueue {
    inner: Mutex<Inner>,
    host: Box<dyn CommandQueueHost>,
    rpmb: fidl_next::Client<rpmb::DriverRpmb, DriverChannel>,
    capabilities: CqhciCqCapsRegister,
    ext_csd: [u8; EXT_CSD_SIZE],
    rca: u16,
    transfer_manager: Arc<TransferManager>,
}

impl CommandQueue {
    fn supports_barriers(&self) -> bool {
        self.ext_csd[EXT_CSD_BARRIER_SUPPORT] & EXT_CSD_BARRIER_SUPPORT_MASK > 0
    }

    fn supports_trim(&self) -> bool {
        self.ext_csd[EXT_CSD_SEC_FEATURE_SUPPORT] & EXT_CSD_SEC_FEATURE_SUPPORT_SEC_GB_CL_EN > 0
    }

    fn cache_enabled(&self) -> bool {
        self.ext_csd[EXT_CSD_CACHE_CTRL] & EXT_CSD_CACHE_EN_MASK > 0
    }

    fn cache_policy_fifo(&self) -> bool {
        self.ext_csd[EXT_CSD_CACHE_FLUSH_POLICY] & EXT_CSD_CACHE_FLUSH_POLICY_FIFO > 0
    }

    pub fn device_flags(&self) -> fblock::DeviceFlag {
        let mut flags = fblock::DeviceFlag::empty();
        if self.supports_trim() {
            flags |= fblock::DeviceFlag::TRIM_SUPPORT;
        }
        if self.supports_barriers() {
            flags |= fblock::DeviceFlag::BARRIER_SUPPORT;
        }
        flags
    }

    /// Initializes command queueing.
    ///
    /// `host_info` is updated to reflect the maximum transfer size supported.
    pub async fn initialize(
        vmar: zx::Vmar,
        host: Box<dyn CommandQueueHost>,
        rpmb: fidl_next::Client<rpmb::DriverRpmb, DriverChannel>,
        host_info: &mut cqhci::CqhciHostInfo,
    ) -> anyhow::Result<Arc<Self>> {
        let virtual_interrupt = zx::Interrupt::create_virtual()?;
        let virtual_interrupt_clone =
            virtual_interrupt.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let (virtual_irq_lifeline_peer, virtual_irq_lifeline) = zx::EventPair::create();

        let CommandQueueResources { cqhci_mmio, sdhci_mmio, bti, interrupt } = host
            .initialize(virtual_interrupt_clone, virtual_irq_lifeline_peer)
            .await
            .context("Failed to initialize")?;

        let version = cqhci_mmio.load32(CQHCI_CQ_VER_OFFSET);
        let capabilities = CqhciCqCapsRegister(cqhci_mmio.load32(CQHCI_CQ_CAP_OFFSET));
        info!("Initializing CQHCI.  Version {version:04x} caps {capabilities:?}");

        // Allocate DMA buffers.
        let vmar_duplicate =
            vmar.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate vmar")?;
        let tdl_buffer =
            ContiguousDmaBuffer::new(vmar_duplicate, &bti, CQHCI_TASK_DESCRIPTOR_LIST_SIZE)
                .context("Failed to create TDL DMA buffer")?;
        debug!("Allocated TDL buffer {} @ 0x{:x}", tdl_buffer.size(), tdl_buffer.phys_address());

        let (extra_descriptors_buffer_size, max_transfer_blocks) =
            TransferManager::extra_descriptors_dimensions();
        let extra_descriptors_buffer =
            DiscontiguousDmaBuffer::new(vmar, &bti, extra_descriptors_buffer_size)
                .context("Failed to create descriptor DMA buffer")?;
        host_info.sdmmc_host_info.max_transfer_size = max_transfer_blocks * MMC_BLOCK_SIZE as u32;
        debug!(
            "Allocated extra descriptors buffer {}.  Max xfer size {} bytes",
            extra_descriptors_buffer_size, host_info.sdmmc_host_info.max_transfer_size,
        );

        let transfer_manager =
            Arc::new(TransferManager::new(tdl_buffer, extra_descriptors_buffer, bti));

        let (irq_lifeline, irq_lifeline2) = zx::EventPair::create();

        let this = Arc::new(Self {
            inner: Mutex::new(Inner {
                state: State::Disabled,
                needs_recovery: false,
                shutting_down: false,
                blocked: false,
                async_task_queue: VecDeque::new(),
                event: Event::new(),
                active_partition: None,
                pending_tasks: PendingTasks::default(),
                cqhci_mmio,
                sdhci_mmio,
                async_task_loop: None,
                virtual_irq_lifeline: Some(virtual_irq_lifeline),
                irq_thread: None,
                irq_lifeline: Some(irq_lifeline),
                partition_status_receivers: BTreeMap::new(),
            }),
            host,
            rpmb,
            capabilities,
            ext_csd: host_info.ext_csd.clone().try_into().map_err(|_| zx::Status::INVALID_ARGS)?,
            rca: host_info.rca,
            transfer_manager,
        });

        // Handle interrupts.  Note that we need to set up the port to listen to interrupt before we
        // proceed with initialization, to avoid race conditions resulting in losing an IRQ.
        let port = zx::Port::create_with_opts(zx::PortOptions::BIND_TO_INTERRUPT);
        interrupt.bind_port(&port, IRQ_PORT_IRQ_KEY).context("Failed to bind IRQ to port")?;
        irq_lifeline2
            .wait_async(
                &port,
                IRQ_PORT_LIFELINE_KEY,
                zx::Signals::EVENTPAIR_PEER_CLOSED,
                zx::WaitAsyncOpts::empty(),
            )
            .context("Failed to wait on lifeline")?;
        let this_clone = this.clone();
        let irq_thread = std::thread::Builder::new()
            .name("cqhci-irq".to_owned())
            .spawn(move || {
                if let Err(err) =
                    fuchsia_scheduler::set_role_for_this_thread("fuchsia.devices.sdhci.irq")
                {
                    warn!("Failed to set IRQ thread role: {err:?}");
                }
                irq_thread_main(
                    Arc::downgrade(&this_clone),
                    port,
                    interrupt,
                    irq_lifeline2,
                    virtual_interrupt,
                )
            })
            .context("Failed to create IRQ thread")?;

        // Run the rest of the initialization.
        // Note that we cannot simply use [`Self::submit_async_task`] here, as that checks if the
        // queue is enabled and rejects tasks otherwise.
        let mut this_excl = {
            let mut inner = this.inner.lock();
            inner.irq_thread = Some(irq_thread);
            CommandQueueExcl::new(this.clone(), &mut inner)
        };
        this_excl.initialize().await.context("Failed to initialize CQE")?;
        let this_clone = this.clone();
        {
            let mut inner = this.inner.lock();
            inner.async_task_loop = Some(fasync::Task::spawn(async move {
                this_clone.async_task_loop().await;
            }));
            inner.state = State::Enabled;
            inner.event.notify(usize::MAX);
        }
        Ok(this)
    }

    /// Shuts down the CQE and any associated background tasks.
    ///
    /// The command queue will reject any future requests.
    pub async fn shutdown(self: &Arc<Self>) {
        let async_task_loop = {
            let mut inner = self.inner.lock();
            inner.shutting_down = true;
            inner.event.notify(usize::MAX);
            inner.async_task_loop.take()
        };
        if let Some(async_task_loop) = async_task_loop {
            debug!("Waiting for async task loop to complete");
            async_task_loop.await;
        }
    }

    /// Unpins any memory pinned by the command queue.
    ///
    /// This must be called after [`CommandQueue::shutdown`], and when there are no remaining
    /// references to the CommandQueue.
    pub fn unpin_buffers(self: Arc<Self>) {
        assert!(
            self.inner.lock().state == State::Disabled,
            "CommandQueue must be shutdown before unpinning buffers"
        );
        if let Ok(manager) = Arc::try_unwrap(self)
            .map_err(|_| ())
            .and_then(|this| Arc::try_unwrap(this.transfer_manager).map_err(|_| ()))
        {
            // SAFETY: CQHCI is disabled, so it should be safe to unpin memory.
            unsafe {
                manager.unpin_buffers();
            }
        } else {
            // This indicates a logic bug.  Log it and proceed without unpinning, which is safer
            // than prematurely unpinning.
            error!("Failed to unpin buffers: outstanding references exist");
        }
    }

    /// Registers the completion callback for the given partition.
    /// Must be called exactly once for each partition for which requests will be submitted.
    pub fn register_partition(
        &self,
        partition: EmmcPartitionId,
        receiver: Weak<dyn TaskStatusReceiver>,
    ) {
        assert!(self.inner.lock().partition_status_receivers.insert(partition, receiver).is_none());
    }

    fn try_submit_transfer(
        &self,
        partition: EmmcPartitionId,
        request_id: RequestId,
        direction: Direction,
        block_offset: u32,
        block_count: u32,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        options: TransferOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> SubmitResult {
        if options.queue_barrier && (self.cache_enabled() && !self.cache_policy_fifo()) {
            // TODO(https://fxbug.dev/490483833): If the device is not FIFO, we can't get away with
            // just using a queue barrier.  We will also need to issue an actual barrier command to
            // the MMC.
            warn!("Barriers on non-FIFO devices are not supported");
            return SubmitResult::Done(zx::Status::NOT_SUPPORTED);
        }
        let slot = match self.transfer_manager.acquire_transfer_slot() {
            Some(s) => s,
            None => {
                let inner = self.inner.lock();
                if inner.should_reject_tasks() {
                    return SubmitResult::Done(zx::Status::UNAVAILABLE);
                } else {
                    return SubmitResult::ShouldWait(inner.event.listen());
                }
            }
        };
        // Prepare the transfer now.  We might have to drop it (unpinning buffers and releasing the
        // slot we've acquired) if the CQE is blocked.  This should be rare, and it enables us to
        // prepare the transfer outside of the lock context of Inner, which reduces contention on
        // the fast path.
        let transfer = match self.transfer_manager.prepare_transfer(
            slot,
            vmo.clone(),
            vmo_offset,
            block_offset,
            block_count,
            direction,
            options,
        ) {
            Ok(transfer) => scopeguard::guard(transfer, |t| {
                // SAFETY: We never submitted the transfer.
                unsafe { t.unpin() }
            }),
            Err(status) => return SubmitResult::Done(status),
        };

        let mut inner = self.inner.lock();
        if inner.should_reject_tasks() {
            return SubmitResult::Done(zx::Status::UNAVAILABLE);
        } else if inner.should_wait_to_submit_tasks() {
            return SubmitResult::ShouldWait(inner.event.listen());
        }

        let task = PendingTask {
            request_id,
            partition,
            transfer: scopeguard::ScopeGuard::into_inner(transfer),
            trace_flow_id,
        };
        let tdl = task.transfer.tdl_slot();
        if inner.active_partition == Some(partition) {
            // Fast path, we can immediately submit.
            inner.submit_transfer(tdl, task);
            SubmitResult::Done(zx::Status::OK)
        } else {
            // Slow path, we have to switch partitions before we can submit.
            let receiver = inner.partition_status_receivers.get(&partition).unwrap().clone();
            inner.submit_async_task(SwitchAndSubmitTask { partition, task: Some(task), receiver });
            SubmitResult::Done(zx::Status::OK)
        }
    }

    fn submit_transfer(
        self: &Arc<Self>,
        partition: EmmcPartitionId,
        request_id: RequestId,
        direction: Direction,
        block_offset: u64,
        block_count: u32,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        options: TransferOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        if options.inline_crypto.is_enabled && !self.capabilities.crypto_support() {
            return Err(zx::Status::NOT_SUPPORTED);
        }
        fuchsia_trace::duration!("sdmmc", "cqhci::submit_transfer",
            "op" => direction.as_str(),
            "blocks" => block_count as u64
        );
        let block_offset = block_offset.try_into().map_err(|_| zx::Status::INVALID_ARGS)?;
        let res: Result<(), zx::Status> = loop {
            match self.try_submit_transfer(
                partition,
                request_id,
                direction,
                block_offset,
                block_count,
                vmo.clone(),
                vmo_offset,
                options,
                trace_flow_id,
            ) {
                SubmitResult::Done(status) => {
                    break status.into();
                }
                SubmitResult::ShouldWait(listener) => {
                    listener.wait();
                }
            }
        };
        if let Some(trace_flow_id) = trace_flow_id
            && res.is_ok()
        {
            fuchsia_trace::flow_step!(
                "storage",
                "cqhci::submit_transfer",
                trace_flow_id.get().into()
            );
        }
        res
    }

    pub fn submit_read(
        self: &Arc<Self>,
        partition: EmmcPartitionId,
        request_id: RequestId,
        block_offset: u64,
        block_count: u32,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        options: block_server::ReadOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) {
        debug!("Read {block_count}@{block_offset}");
        if let Err(status) = self.submit_transfer(
            partition,
            request_id,
            Direction::Read,
            block_offset,
            block_count,
            vmo,
            vmo_offset,
            TransferOptions { queue_barrier: false, inline_crypto: options.inline_crypto },
            trace_flow_id,
        ) {
            complete_request(
                self.inner.lock().get_request_completer(partition),
                request_id,
                status,
            );
        }
    }

    pub fn submit_write(
        self: &Arc<Self>,
        partition: EmmcPartitionId,
        request_id: RequestId,
        block_offset: u64,
        block_count: u32,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        options: block_server::WriteOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) {
        debug!("Write {block_count}@{block_offset}");
        if let Err(status) = self.submit_transfer(
            partition,
            request_id,
            Direction::Write,
            block_offset,
            block_count,
            vmo,
            vmo_offset,
            TransferOptions::from(options),
            trace_flow_id,
        ) {
            complete_request(
                self.inner.lock().get_request_completer(partition),
                request_id,
                status,
            );
        }
    }
    pub fn submit_flush(
        self: &Arc<Self>,
        partition: EmmcPartitionId,
        request_id: RequestId,
        trace_flow_id: Option<NonZero<u64>>,
    ) {
        fuchsia_trace::duration!("sdmmc", "cqhci::submit_flush");
        if let Some(trace_flow_id) = trace_flow_id {
            fuchsia_trace::flow_step!("storage", "cqhci::submit_flush", trace_flow_id.get().into());
        }
        debug!("submit_flush");
        if !self.cache_enabled() {
            complete_request(
                self.inner.lock().get_request_completer(partition),
                request_id,
                zx::Status::OK,
            );
            return;
        }
        let mut inner = self.inner.lock();
        let receiver = inner.partition_status_receivers.get(&partition).unwrap().clone();
        inner.submit_async_task(into_async_task(
            async move |mut cq| {
                let result = cq.do_switch(EXT_CSD_FLUSH_CACHE, EXT_CSD_FLUSH_CACHE_FLUSH).await;
                fuchsia_trace::duration!(
                    "sdmmc", "cqhci::complete_flush", "status"
                        => zx::Status::from(result).into_raw()
                );
                if let Some(trace_flow_id) = trace_flow_id {
                    fuchsia_trace::flow_step!(
                        "storage",
                        "cqhci::complete_flush",
                        trace_flow_id.get().into()
                    );
                }
                result
            },
            move |result| receiver.complete(request_id, zx::Status::from(result)),
        ));
    }

    pub fn submit_trim(
        self: &Arc<Self>,
        partition: EmmcPartitionId,
        request_id: RequestId,
        block_offset: u64,
        block_count: u32,
        trace_flow_id: Option<NonZero<u64>>,
    ) {
        fuchsia_trace::duration!("sdmmc", "cqhci::submit_trim");
        if let Some(trace_flow_id) = trace_flow_id {
            fuchsia_trace::flow_step!("storage", "cqhci::submit_trim", trace_flow_id.get().into());
        }
        debug!("submit_trim");
        let mut inner = self.inner.lock();
        let receiver = inner.partition_status_receivers.get(&partition).unwrap().clone();
        inner.submit_async_task(into_async_task(
            async move |mut cq| {
                let result = cq.trim(partition, block_offset, block_count).await;
                fuchsia_trace::duration!(
                    "sdmmc", "cqhci::complete_trim", "status"
                        => zx::Status::from(result).into_raw()
                );
                if let Some(trace_flow_id) = trace_flow_id {
                    fuchsia_trace::flow_step!(
                        "storage",
                        "cqhci::complete_trim",
                        trace_flow_id.get().into()
                    );
                }
                result
            },
            move |result| receiver.complete(request_id, zx::Status::from(result)),
        ));
    }

    pub async fn get_rpmb_info(&self) -> Result<rpmb::natural::DeviceInfo, zx::Status> {
        Ok(self.rpmb.get_device_info().await.map_err(|_| zx::Status::INTERNAL)?.info)
    }

    pub fn rpmb_request<Fut: Future<Output = ()> + Send + 'static>(
        self: &Arc<Self>,
        request: rpmb::Request,
        callback: impl FnOnce(Result<(), zx::Status>) -> Fut + Send + 'static,
    ) {
        self.inner.lock().submit_async_task(into_async_task(
            async |mut cq| cq.rpmb_request(request).await,
            |result| {
                fasync::Task::spawn(async move {
                    callback(result).await;
                })
                .detach()
            },
        ));
    }

    /// Suspends the CQ engine.  The caller should use the returned sender to resume.
    pub async fn suspend(&self) -> Result<oneshot::Sender<()>, zx::Status> {
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock();
            assert!(inner.state == State::Enabled);
            inner.submit_async_task(into_async_task(
                async |mut cq| {
                    cq.disable().await;
                    assert_eq!(
                        std::mem::replace(&mut cq.inner.lock().state, State::Suspended),
                        State::Enabled
                    );
                    let (resume_tx, resume_rx) = oneshot::channel();
                    let _ = tx.send(resume_tx);

                    info!("Suspended");

                    // Wait till resumed.
                    let _ = resume_rx.await;
                    let result = cq.enable().await;
                    cq.inner.lock().state = match result {
                        Ok(()) => {
                            info!("Resumed");
                            State::Enabled
                        }
                        Err(error) => {
                            warn!(error:?; "Failed to resume");
                            State::Disabled
                        }
                    };
                    result
                },
                |_| {},
            ));
        }
        rx.await.map_err(|_| zx::Status::CANCELED)
    }

    /// Pops the next task, returning it and an [`CommandQueueExcl`] representing unique access to
    /// the command queue.
    ///
    /// Returns None when the command queue is shutting down and there are no more tasks.
    async fn get_next_task(self: &Arc<Self>) -> Option<(Box<dyn AsyncTask>, CommandQueueExcl)> {
        let mut excl = None;

        loop {
            let listener = {
                let mut inner = self.inner.lock();

                if inner.shutting_down {
                    return None;
                }

                if inner.needs_recovery {
                    // NB: We don't need to block transfers since we're about to run recovery and
                    // cancel all in-flight transfers.
                    return Some((
                        Box::new(RecoveryTask),
                        excl.unwrap_or_else(|| CommandQueueExcl::new(self.clone(), &mut inner)),
                    ));
                }

                if inner.should_reject_tasks() {
                    return None;
                }

                if inner.async_task_queue.is_empty() {
                    excl = None;
                } else {
                    // Block the queue.
                    excl.get_or_insert_with(|| CommandQueueExcl::new(self.clone(), &mut inner));

                    // Return if there are no pending tasks.
                    if inner.pending_tasks.is_empty() {
                        return Some((
                            inner.async_task_queue.pop_front().unwrap(),
                            excl.take().unwrap(),
                        ));
                    }
                }

                inner.event.listen()
            };

            listener.await;
        }
    }

    async fn async_task_loop(self: &Arc<Self>) {
        while let Some((task, cq)) = self.get_next_task().await {
            task.run(cq).await;
        }

        let _ = CommandQueueExcl { queue: self.clone() }.shutdown().await;

        let mut inner = self.inner.lock();
        inner.async_task_queue.clear();
        inner.state = State::Disabled;

        debug!("async_task_loop completed");
    }

    /// Returns true if the virtual interrupt was triggered, in which case we should wait for that
    /// to ack first before acking the physical interrupt.
    fn on_interrupt(&self, virtual_interrupt: &zx::VirtualInterrupt) -> bool {
        fuchsia_trace::duration!("sdmmc", "cqhci::on_interrupt");
        // NB: Order is important.  We want to finish [`CompletedTasks`] after unlocking `inner`, so
        // that we're not holding the lock while invoking the callbacks, to reduce lock contention.
        let mut completed_tasks = CompletedTasks::default();
        let should_wait_for_virtual_irq = {
            let mut inner = self.inner.lock();

            // Read the SDHCI interrupt status and see if we had a CQHCI interrupt.
            // We'll forward any remaining non-CQHCI interrupts later.
            let mut irq_status =
                SdhciInterruptStatusRegister(inner.sdhci_mmio.load32(SDHCI_IS_OFFSET));
            let just_cqhci_status = irq_status.take_cqhci_interrupt();
            if just_cqhci_status.cqhci_interrupt() {
                inner.sdhci_mmio.store32(SDHCI_IS_OFFSET, just_cqhci_status.raw());
                let cq_irq_status =
                    CqhciCqInterruptStatusRegister(inner.cqhci_mmio.load32(CQHCI_CQ_IS_OFFSET));
                trace!("on_interrupt, sdhci {irq_status:x?} cqhci {cq_irq_status:x?}");
                inner.cqhci_mmio.store32(CQHCI_CQ_IS_OFFSET, cq_irq_status.raw());
                if cq_irq_status.task_complete() {
                    let finished = inner.cqhci_mmio.load32(CQHCI_CQ_TCN_OFFSET);
                    inner.cqhci_mmio.store32(CQHCI_CQ_TCN_OFFSET, finished);
                    inner.take_complete(finished, zx::Status::OK, &mut completed_tasks);
                };
                if cq_irq_status.is_error() {
                    warn!("on_interrupt error, sdhci {irq_status:x?} cqhci {cq_irq_status:x?}");
                    fuchsia_trace::instant!(
                        "sdmmc",
                        "cqhci::on_interrupt::error",
                        fuchsia_trace::Scope::Thread
                    );
                    if cq_irq_status.general_crypto_error() {
                        warn!("General Crypto Error detected!");
                    }
                    if cq_irq_status.invalid_crypto_config_error() {
                        warn!("Invalid Crypto Configuration Error detected!");
                    }
                    let terri =
                        CqhciCqTaskErrorRegister(inner.cqhci_mmio.load32(CQHCI_CQ_TERRI_OFFSET));
                    let mut mask = 0;
                    if terri.response_mode_error_fields_valid() {
                        mask |= 1 << terri.response_mode_error_task_id();
                    }
                    if terri.data_transfer_error_fields_valid() {
                        mask |= 1 << terri.data_transfer_error_task_id();
                    }
                    inner.take_complete(mask, zx::Status::IO, &mut completed_tasks);

                    // Per JESD84-B51A B.2.8, we need to run recovery on error.
                    if inner.needs_recovery {
                        info!("Not running recovery (shutting down or recovery already running)");
                    } else {
                        warn!("CQE needs recovery!");
                        inner.needs_recovery = true;
                        inner.event.notify(usize::MAX);
                    }
                }
            }
            if !irq_status.is_empty() {
                trace!("Forwarding non-CQ interrupt {irq_status:x?}");
                if let Err(err) = virtual_interrupt.trigger(zx::BootInstant::get()) {
                    warn!(err:?; "Failed to trigger virtual interrupt");
                    false
                } else {
                    true
                }
            } else {
                false
            }
        };
        // SAFETY: The tasks were completed.
        fuchsia_trace::instant!(
            "sdmmc",
            "cqhci::on_interrupt",
            fuchsia_trace::Scope::Thread,
            "num_completed" => completed_tasks.count as u64
        );
        unsafe { completed_tasks.complete() }
        should_wait_for_virtual_irq
    }
}

fn irq_thread_main(
    command_queue: Weak<CommandQueue>,
    port: zx::Port,
    physical_interrupt: zx::Interrupt,
    // When dropped, the parent driver will resume handling physical interrupts.
    _lifeline: zx::EventPair,
    virtual_interrupt: zx::VirtualInterrupt,
) {
    loop {
        let packet = port.wait(zx::Instant::INFINITE).unwrap();
        match packet.contents() {
            zx::PacketContents::SignalOne(sig) => {
                match packet.key() {
                    IRQ_PORT_VIRTUAL_IRQ_ACKED_KEY => {
                        debug_assert!(
                            sig.observed().contains(zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED)
                        );
                        trace!("Virtual IRQ acked");
                        if let Err(status) = physical_interrupt.ack() {
                            warn!(status:?; "Failed to ack IRQ");
                            break;
                        }
                    }
                    IRQ_PORT_LIFELINE_KEY => {
                        debug_assert!(sig.observed().contains(zx::Signals::EVENTPAIR_PEER_CLOSED));
                        debug!("Lifeline closed");
                    }
                    _ => {
                        unreachable!()
                    }
                }
                if sig.observed().contains(zx::Signals::EVENTPAIR_PEER_CLOSED) {
                    debug!("Shutdown signal received");
                    break;
                }
            }
            zx::PacketContents::Interrupt(_) => {
                let Some(cq) = command_queue.upgrade() else {
                    break;
                };
                if cq.on_interrupt(&virtual_interrupt) {
                    // We need to wait for the virtual IRQ, then ack the physical IRQ.
                    trace!("Waiting for virtual IRQ ack");
                    if let Err(status) = virtual_interrupt.wait_async(
                        &port,
                        IRQ_PORT_VIRTUAL_IRQ_ACKED_KEY,
                        zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED,
                        zx::WaitAsyncOpts::empty(),
                    ) {
                        warn!(status:?; "Failed to wait on virtual IRQ");
                        break;
                    };
                } else {
                    if let Err(status) = physical_interrupt.ack() {
                        warn!(status:?; "Failed to ack IRQ");
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    if let Err(status) = physical_interrupt.unbind_port(&port) {
        warn!(status:?; "Failed to unbind physical IRQ.  IRQ handling will not resume.");
    }
    if let Err(status) = virtual_interrupt.destroy() {
        warn!(status:?; "Failed to destroy virtual IRQ.");
    }
}
