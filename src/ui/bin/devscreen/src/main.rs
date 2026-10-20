// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! devscreen: a readable boot dashboard for display-only `eng` builds.
//!
//! Runs directly on the display coordinator (above virtcon's priority) and
//! shows device identity, network addresses, battery state and basic metrics,
//! with on-screen buttons for fastboot, reboot and handing the panel back to virtcon.

mod telemetry;
mod ui;

use anyhow::{Context as _, Error};
use carnelian::app::{Config, ViewCreationParameters, ViewMode};
use carnelian::drawing::load_font;
use carnelian::{
    App, AppAssistant, AppSender, MessageTarget, Size, ViewAssistant, ViewAssistantContext,
    ViewAssistantPtr, ViewKey, derive_handle_message, input, make_app_assistant, make_message,
};
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_hardware_display::COMPOSITOR_CLIENT_PRIORITY_VALUE;
use fidl_fuchsia_hardware_power_statecontrol as fpower;
use fidl_fuchsia_power_system as fsystem;
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use log::{error, info, warn};
use std::path::{Path, PathBuf};
use std::time::Duration;
use telemetry::Snapshot;
use ui::{Action, Dashboard, Fonts, Params};

/// How long an armed destructive action waits for its confirmation tap.
const CONFIRM_WINDOW: Duration = Duration::from_secs(4);

/// How long a result message ("Saved …") stays on the status line.
const STATUS_WINDOW: Duration = Duration::from_secs(30);

/// Where snapshots are saved. Retrieve with
/// `ffx component storage copy /core/devscreen::/snapshot-<uptime>.zip .`.
const SNAPSHOT_DIR: &str = "/data";
const SNAPSHOT_PREFIX: &str = "snapshot-";
/// Oldest snapshots are deleted beyond this many.
const SNAPSHOTS_KEPT: usize = 5;
/// Per-data-source budget handed to feedback; a full snapshot takes a
/// multiple of this in the worst case.
const SNAPSHOT_COLLECTION_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(20);

/// Messages delivered to the view.
pub enum DevscreenMessage {
    Snapshot(Box<Snapshot>),
    BeforeSuspend(std::sync::Mutex<Option<futures::channel::oneshot::Sender<()>>>),
    AfterResume,
    /// The confirmation window for an armed action elapsed.
    Disarm,
    /// Result of an action that was executed. `Ok(Some(text))` leaves a
    /// transient message on the status line.
    ActionFinished(Action, Result<Option<String>, String>),
    /// The transient status message expired.
    ClearStatus,
}

#[derive(Default)]
struct DevscreenAppAssistant;

impl AppAssistant for DevscreenAppAssistant {
    fn setup(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn create_view_assistant_with_parameters(
        &mut self,
        params: ViewCreationParameters,
    ) -> Result<ViewAssistantPtr, Error> {
        Ok(Box::new(DevscreenViewAssistant::new(params.app_sender, params.view_key)?))
    }

    fn filter_config(&mut self, config: &mut Config) {
        // Never fall back to Flatland; this app only makes sense on the bare
        // display coordinator, and `Direct` keeps watching for the display to
        // appear during boot instead of timing out.
        config.view_mode = ViewMode::Direct;
        // Connect above virtcon (`VIRTCON_CLIENT_PRIORITY_VALUE` = 100) using
        // the platform `COMPOSITOR_CLIENT_PRIORITY_VALUE` (200).
        config.client_priority = Some(COMPOSITOR_CLIENT_PRIORITY_VALUE);
        config.keyboard_autorepeat = false;
    }
}

struct DevscreenViewAssistant {
    app_sender: AppSender,
    view_key: ViewKey,
    owned: bool,
    fonts: Fonts,
    snapshot: Snapshot,
    dashboard: Option<Dashboard>,
    /// Button hit-rects for the current size. Kept separately from
    /// `dashboard` so hit-testing keeps working while the scene is being
    /// rebuilt (`invalidate()` drops `dashboard` until the next render).
    buttons: Vec<ui::Button>,
    size: Size,
    armed: Option<Action>,
    /// Button under the finger when the current touch started.
    target: Option<Action>,
    /// Button currently highlighted.
    pressed: Option<Action>,
    tracking_pointer: Option<input::pointer::PointerId>,
    status: Option<String>,
    /// A snapshot collection is running; the button is disabled meanwhile.
    snapshot_busy: bool,
    // Held, not detached: dropping the view cancels them.
    poller: Option<fasync::Task<()>>,
    disarm_timer: Option<fasync::Task<()>>,
    status_timer: Option<fasync::Task<()>>,
    snapshot_task: Option<fasync::Task<()>>,
    action_task: Option<fasync::Task<()>>,
    suspended: bool,
    _suspend_blocker: fasync::Task<()>,
}

impl DevscreenViewAssistant {
    fn new(app_sender: AppSender, view_key: ViewKey) -> Result<Self, Error> {
        let regular = load_font(PathBuf::from("/pkg/data/fonts/RobotoSlab-Regular.ttf"))
            .context("loading RobotoSlab")?;
        let mono = load_font(PathBuf::from("/pkg/data/fonts/RobotoMono-Regular.ttf"))
            .context("loading RobotoMono")?;
        let snapshot = Snapshot::default();
        let poller =
            fasync::Task::local(telemetry::run(app_sender.clone(), view_key, snapshot.clone()));

        let _suspend_blocker =
            if let Ok(governor) = connect_to_protocol::<fsystem::ActivityGovernorMarker>() {
                let (client, stream) =
                    fidl::endpoints::create_request_stream::<fsystem::SuspendBlockerMarker>();
                let sender = app_sender.clone();
                fasync::Task::local(async move {
                    match governor
                        .register_suspend_blocker(
                            fsystem::ActivityGovernorRegisterSuspendBlockerRequest {
                                name: Some("devscreen".to_string()),
                                suspend_blocker: Some(client),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(Ok(token)) => drop(token),
                        Ok(Err(e)) => {
                            warn!("register_suspend_blocker failed: {:?}", e);
                            return;
                        }
                        Err(e) => {
                            warn!("FIDL error registering suspend blocker: {:?}", e);
                            return;
                        }
                    }
                    drop(governor);
                    let mut stream = stream;
                    while let Ok(Some(req)) = futures::TryStreamExt::try_next(&mut stream).await {
                        match req {
                            fsystem::SuspendBlockerRequest::BeforeSuspend { responder } => {
                                let (tx, rx) = futures::channel::oneshot::channel();
                                sender.queue_message(
                                    MessageTarget::View(view_key),
                                    make_message(DevscreenMessage::BeforeSuspend(
                                        std::sync::Mutex::new(Some(tx)),
                                    )),
                                );
                                let _ = rx.await;
                                let _ = responder.send();
                            }
                            fsystem::SuspendBlockerRequest::AfterResume { responder } => {
                                sender.queue_message(
                                    MessageTarget::View(view_key),
                                    make_message(DevscreenMessage::AfterResume),
                                );
                                let _ = responder.send();
                            }
                            fsystem::SuspendBlockerRequest::_UnknownMethod { .. } => {}
                        }
                    }
                })
            } else {
                fasync::Task::local(async {})
            };
        Ok(Self {
            app_sender,
            view_key,
            owned: true,
            fonts: Fonts { regular, mono },
            snapshot,
            dashboard: None,
            buttons: Vec::new(),
            size: Size::zero(),
            armed: None,
            target: None,
            pressed: None,
            tracking_pointer: None,
            status: None,
            snapshot_busy: false,
            poller: Some(poller),
            disarm_timer: None,
            status_timer: None,
            snapshot_task: None,
            action_task: None,
            suspended: false,
            _suspend_blocker,
        })
    }

    fn invalidate(&mut self) {
        self.dashboard = None;
        if self.owned && !self.suspended {
            self.app_sender.request_render(self.view_key);
        }
    }

    fn ensure_dashboard(&mut self, size: Size) {
        if self.dashboard.is_none() || self.size != size {
            self.size = size;
            let dashboard = ui::build(Params {
                size,
                snapshot: &self.snapshot,
                fonts: &self.fonts,
                armed: self.armed,
                pressed: self.pressed,
                snapshot_busy: self.snapshot_busy,
                status: self.status.as_deref(),
            });
            self.buttons = dashboard.buttons.clone();
            self.dashboard = Some(dashboard);
        }
    }

    /// Button containing `location`, grown by `slop` pixels on every side.
    fn button_at(&self, location: &carnelian::IntPoint, slop: f32) -> Option<Action> {
        let point = location.to_f32();
        self.buttons.iter().find(|b| b.rect.inflate(slop, slop).contains(point)).map(|b| b.action)
    }

    /// Queues `message` to this view after `delay`.
    fn delayed(
        &self,
        delay: Duration,
        message: impl FnOnce() -> DevscreenMessage + 'static,
    ) -> fasync::Task<()> {
        let sender = self.app_sender.clone();
        let view_key = self.view_key;
        fasync::Task::local(async move {
            fasync::Timer::new(delay).await;
            sender.queue_message(MessageTarget::View(view_key), make_message(message()));
        })
    }

    fn handle_devscreen_message(&mut self, message: &DevscreenMessage) {
        match message {
            DevscreenMessage::BeforeSuspend(tx) => {
                self.suspended = true;
                self.poller = None;
                self.invalidate();
                if let Ok(mut tx_guard) = tx.lock() {
                    if let Some(tx) = tx_guard.take() {
                        let _ = tx.send(());
                    }
                }
            }
            DevscreenMessage::AfterResume => {
                self.suspended = false;
                if self.owned {
                    let sender = self.app_sender.clone();
                    let view_key = self.view_key;
                    let snapshot = self.snapshot.clone();
                    self.poller.get_or_insert_with(|| {
                        fasync::Task::local(telemetry::run(sender, view_key, snapshot))
                    });
                }
                self.invalidate();
            }
            DevscreenMessage::Snapshot(snapshot) => {
                self.snapshot = (**snapshot).clone();
                // Don't rebuild under a finger; the next snapshot will catch up.
                if self.tracking_pointer.is_none() {
                    self.invalidate();
                }
            }
            DevscreenMessage::Disarm => {
                if self.pressed.is_some() && self.pressed == self.armed {
                    // Don't disarm underneath a finger that is actively pressing CONFIRM.
                    self.disarm_timer =
                        Some(self.delayed(CONFIRM_WINDOW, || DevscreenMessage::Disarm));
                } else if self.armed.take().is_some() {
                    self.invalidate();
                }
            }
            DevscreenMessage::ActionFinished(action, result) => {
                if *action == Action::Snapshot {
                    self.snapshot_busy = false;
                    self.snapshot_task = None;
                } else {
                    self.action_task = None;
                }
                self.status = match result {
                    Ok(message) => message.clone(),
                    Err(e) => Some(format!("Failed: {e}")),
                };
                self.status_timer = self
                    .status
                    .is_some()
                    .then(|| self.delayed(STATUS_WINDOW, || DevscreenMessage::ClearStatus));
                self.invalidate();
            }
            DevscreenMessage::ClearStatus => {
                self.status = None;
                self.status_timer = None;
                self.invalidate();
            }
        }
    }

    fn tapped(&mut self, action: Action) {
        info!("tapped {action:?} (armed: {:?})", self.armed);
        if action.needs_confirmation() && self.armed != Some(action) {
            self.armed = Some(action);
            self.disarm_timer = Some(self.delayed(CONFIRM_WINDOW, || DevscreenMessage::Disarm));
            return;
        }
        self.armed = None;
        self.disarm_timer = None;
        self.execute(action);
    }

    fn execute(&mut self, action: Action) {
        match action {
            Action::ExitToVirtcon => {
                // Dropping the display coordinator connection hands the panel
                // back to virtcon. Re-launch with
                // `ffx component start /core/devscreen`.
                info!("exiting; handing the panel back to virtcon");
                std::process::exit(0);
            }
            Action::Snapshot if self.snapshot_busy => {}
            Action::Snapshot | Action::Fastboot | Action::Reboot => {
                self.status = Some(match action {
                    Action::Snapshot => "Collecting snapshot… (can take a minute)".to_string(),
                    Action::Fastboot => "Rebooting to bootloader…".to_string(),
                    _ => "Rebooting…".to_string(),
                });
                self.status_timer = None;
                let sender = self.app_sender.clone();
                let view_key = self.view_key;
                let task = fasync::Task::local(async move {
                    let result = perform(action).await.map_err(|e| format!("{e:#}"));
                    if let Err(e) = &result {
                        error!("{action:?} failed: {e}");
                    }
                    sender.queue_message(
                        MessageTarget::View(view_key),
                        make_message(DevscreenMessage::ActionFinished(action, result)),
                    );
                });
                if action == Action::Snapshot {
                    self.snapshot_busy = true;
                    self.snapshot_task = Some(task);
                } else {
                    self.action_task = Some(task);
                }
            }
        }
    }
}

async fn perform(action: Action) -> Result<Option<String>, Error> {
    if action == Action::Snapshot {
        return save_snapshot().await.map(Some);
    }
    let admin = connect_to_protocol::<fpower::AdminMarker>()
        .context("connecting to fuchsia.hardware.power.statecontrol.Admin")?;
    let result = match action {
        Action::Fastboot => admin.reboot_to_bootloader().await.context("RebootToBootloader")?,
        Action::Reboot => admin
            .perform_reboot(&fpower::RebootOptions {
                reasons: Some(vec![fpower::RebootReason2::UserRequest]),
                ..Default::default()
            })
            .await
            .context("PerformReboot")?,
        Action::Snapshot | Action::ExitToVirtcon => Ok(()),
    };
    result
        .map(|()| None)
        .map_err(|status| anyhow::anyhow!("{:?}", zx::Status::err_from_raw(status)))
}

/// Asks feedback for a snapshot and stores the archive under [`SNAPSHOT_DIR`].
/// Returns the status-line message.
async fn save_snapshot() -> Result<String, Error> {
    let provider = connect_to_protocol::<ffeedback::DataProviderMarker>()
        .context("connecting to fuchsia.feedback.DataProvider")?;
    let params = ffeedback::GetSnapshotParameters {
        collection_timeout_per_data: Some(SNAPSHOT_COLLECTION_TIMEOUT.into_nanos()),
        ..Default::default()
    };
    let snapshot = provider.get_snapshot(params).await.context("GetSnapshot")?;
    let archive = snapshot.archive.context("snapshot has no archive (feedback unavailable?)")?;
    let size = archive.value.size as usize;

    let uptime_s = zx::MonotonicInstant::get().into_nanos() / 1_000_000_000;
    let path = Path::new(SNAPSHOT_DIR).join(format!("{SNAPSHOT_PREFIX}{uptime_s}.zip"));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("creating {} (is storage routed?)", path.display()))?;
        let mut buffer = vec![0u8; 1 << 20];
        let mut offset = 0usize;
        while offset < size {
            let chunk = buffer.len().min(size - offset);
            archive.value.vmo.read(&mut buffer[..chunk], offset as u64).context("reading VMO")?;
            file.write_all(&buffer[..chunk]).context("writing archive")?;
            offset += chunk;
        }
    }
    prune_snapshots(&path);
    info!("saved snapshot {} ({} bytes)", path.display(), size);
    Ok(format!(
        "Saved {} ({:.1} MB) · ffx component storage copy /core/devscreen::{} .",
        path.display(),
        size as f64 / 1e6,
        path.strip_prefix(SNAPSHOT_DIR).map(|p| format!("/{}", p.display())).unwrap_or_default()
    ))
}

/// Keeps only the newest [`SNAPSHOTS_KEPT`] archives; `latest` is never
/// removed (the clock may run backwards across boots on devices without an
/// RTC, so modification time alone is not trustworthy).
fn prune_snapshots(latest: &Path) {
    let Ok(entries) = std::fs::read_dir(SNAPSHOT_DIR) else { return };
    let mut archives: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SNAPSHOT_PREFIX) && n.ends_with(".zip"))
        })
        .filter(|p| p != latest)
        .collect();
    // Uptime in the name restarts every boot, so order by modification time.
    archives.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    while archives.len() + 1 > SNAPSHOTS_KEPT {
        let old = archives.remove(0);
        if let Err(e) = std::fs::remove_file(&old) {
            warn!("could not remove {}: {e}", old.display());
        }
    }
}

impl ViewAssistant for DevscreenViewAssistant {
    fn resize(&mut self, new_size: &Size) -> Result<(), Error> {
        self.ensure_dashboard(*new_size);
        Ok(())
    }

    fn get_scene(&mut self, target_size: Size) -> Option<&mut carnelian::scene::scene::Scene> {
        self.ensure_dashboard(target_size);
        self.dashboard.as_mut().map(|d| &mut d.scene)
    }

    derive_handle_message!(DevscreenMessage => handle_devscreen_message);

    fn handle_pointer_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _event: &input::Event,
        pointer_event: &input::pointer::Event,
    ) -> Result<(), Error> {
        if !self.owned {
            return Ok(());
        }
        let is_tracked = self.tracking_pointer.as_ref() == Some(&pointer_event.pointer_id);
        // Once a tap has started, be forgiving about the finger wandering a
        // little past the button edge.
        let move_slop = (self.size.width.min(self.size.height) * 0.04).max(24.0);
        match &pointer_event.phase {
            input::pointer::Phase::Down(location) => {
                if self.tracking_pointer.is_none() {
                    if let Some(action) = self.button_at(location, 0.0) {
                        self.tracking_pointer = Some(pointer_event.pointer_id.clone());
                        self.target = Some(action);
                        self.pressed = Some(action);
                        self.invalidate();
                    } else {
                        // Useful when diagnosing touch/display coordinate
                        // mismatches (rotation, scaling).
                        info!(
                            "touch down at ({}, {}) hit no button (view {}x{})",
                            location.x, location.y, self.size.width, self.size.height
                        );
                    }
                }
            }
            input::pointer::Phase::Moved(location) if is_tracked => {
                // Highlight only while the finger is still over the button it
                // started on; sliding clearly off cancels the tap.
                let inside = self.button_at(location, move_slop) == self.target;
                let pressed = if inside { self.target } else { None };
                if pressed != self.pressed {
                    self.pressed = pressed;
                    self.invalidate();
                }
            }
            input::pointer::Phase::Up if is_tracked => {
                self.tracking_pointer = None;
                self.target = None;
                if let Some(action) = self.pressed.take() {
                    self.tapped(action);
                }
                self.invalidate();
            }
            input::pointer::Phase::Remove | input::pointer::Phase::Cancel if is_tracked => {
                self.tracking_pointer = None;
                self.target = None;
                self.pressed = None;
                self.invalidate();
            }
            _ => {}
        }
        context.request_render();
        Ok(())
    }

    fn ownership_changed(&mut self, owned: bool) -> Result<(), Error> {
        info!("display ownership: {owned}");
        self.owned = owned;
        if !owned {
            warn!("lost display ownership (another primary client connected)");
            self.poller = None;
            self.tracking_pointer = None;
            self.target = None;
            self.pressed = None;
            self.armed = None;
            self.disarm_timer = None;
            self.dashboard = None;
        } else if !self.suspended {
            let sender = self.app_sender.clone();
            let view_key = self.view_key;
            let snapshot = self.snapshot.clone();
            self.poller.get_or_insert_with(|| {
                fasync::Task::local(telemetry::run(sender, view_key, snapshot))
            });
            self.invalidate();
        }
        Ok(())
    }
}

#[fuchsia::main(logging_tags = ["devscreen"])]
fn main() -> Result<(), Error> {
    info!("devscreen starting");
    App::run(make_app_assistant::<DevscreenAppAssistant>())
}
