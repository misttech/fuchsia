// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod game;
mod internal_message;
mod keyboard;
mod mouse;
mod scene;
mod touch;

use async_utils::hanging_get::client::HangingGetStream;
use fidl::endpoints::{Proxy, create_proxy};
use fidl_fuchsia_element::{
    GraphicalPresenterMarker, GraphicalPresenterProxy, ViewControllerMarker, ViewControllerProxy,
    ViewSpec,
};
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_ui_composition as fland;
use fidl_fuchsia_ui_input3 as fuiinput;
use fidl_fuchsia_ui_pointer as fptr;
use fidl_fuchsia_ui_views as fviews;
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_scenic::ViewRefPair;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::prelude::*;
use game::{GameCommand, SnakeGame};
use internal_message::InternalMessage;
use log::{error, info, warn};
use scene::SnakeScene;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TICK_INTERVAL_MS: u64 = 140;

struct AppModel<'a> {
    flatland: &'a fland::FlatlandProxy,
    graphical_presenter: &'a GraphicalPresenterProxy,
    internal_sender: UnboundedSender<InternalMessage>,
    scope: fasync::ScopeHandle,
    /// Keeps the `fuchsia.ui.input3.Keyboard` channel open; `text_manager`'s `KeyboardService`
    /// removes our `KeyboardListener` when this connection closes.
    keyboard: Option<fuiinput::KeyboardProxy>,
    game: SnakeGame,
    scene: SnakeScene,
}

impl<'a> AppModel<'a> {
    fn new(
        flatland: &'a fland::FlatlandProxy,
        graphical_presenter: &'a GraphicalPresenterProxy,
        internal_sender: UnboundedSender<InternalMessage>,
        scope: fasync::ScopeHandle,
    ) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xCAFE_BABE_DEAD_BEEF);
        let game = SnakeGame::new(seed);
        let mut scene = SnakeScene::new(flatland);
        scene.render(flatland, &game);

        Self { flatland, graphical_presenter, internal_sender, scope, keyboard: None, game, scene }
    }

    async fn setup_optional_keyboard(&mut self, view_ref: fviews::ViewRef) {
        if let Ok(keyboard) = connect_to_protocol::<fuiinput::KeyboardMarker>() {
            let (keyboard_listener_client, keyboard_listener_stream) =
                fidl::endpoints::create_request_stream::<fuiinput::KeyboardListenerMarker>();
            if keyboard.add_listener(view_ref, keyboard_listener_client).await.is_ok() {
                keyboard::spawn_keyboard_listener(
                    &self.scope,
                    keyboard_listener_stream,
                    self.internal_sender.clone(),
                );
                self.keyboard = Some(keyboard);
            } else {
                info!("Keyboard protocol not routed in this realm; using touch/mouse input");
            }
        }
    }

    async fn create_view(&mut self) -> bool {
        let fuchsia_scenic::flatland::ViewCreationTokenPair {
            view_creation_token,
            viewport_creation_token,
        } = fuchsia_scenic::flatland::ViewCreationTokenPair::new()
            .expect("failed to create view tokens");
        let view_identity = fviews::ViewIdentityOnCreation::from(
            ViewRefPair::new().expect("failed viewref creation"),
        );

        let (parent_viewport_watcher, parent_viewport_watcher_request) =
            create_proxy::<fland::ParentViewportWatcherMarker>();

        let (touch, touch_request) = create_proxy::<fptr::TouchSourceMarker>();
        let (mouse, mouse_request) = create_proxy::<fptr::MouseSourceMarker>();
        let (focuser, focuser_request) = create_proxy::<fviews::FocuserMarker>();
        let view_bound_protocols = fland::ViewBoundProtocols {
            touch_source: Some(touch_request),
            mouse_source: Some(mouse_request),
            view_focuser: Some(focuser_request),
            ..Default::default()
        };

        let view_ref_for_keyboard = fuchsia_scenic::duplicate_view_ref(&view_identity.view_ref)
            .expect("failed to dup view ref");
        let view_ref_for_focuser = fuchsia_scenic::duplicate_view_ref(&view_identity.view_ref)
            .expect("failed to dup view ref");

        self.flatland
            .create_view2(
                view_creation_token,
                view_identity,
                view_bound_protocols,
                parent_viewport_watcher_request,
            )
            .expect("fidl error");

        // Present the initial frame before awaiting `present_view()`, because `GraphicalPresenter`
        // may wait for the child view to present its first frame before `present_view()` resolves.
        self.flatland.present(fland::PresentArgs::default()).expect("flatland present");

        let (view_controller_proxy, view_controller_request) =
            create_proxy::<ViewControllerMarker>();
        let view_spec = ViewSpec {
            viewport_creation_token: Some(viewport_creation_token),
            ..Default::default()
        };

        match self
            .graphical_presenter
            .present_view(view_spec, None, Some(view_controller_request))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("GraphicalPresenter.PresentView() error: {:?}", e);
                return false;
            }
            Err(e) => {
                error!("GraphicalPresenter.PresentView() FIDL error: {:?}", e);
                return false;
            }
        }

        Self::spawn_view_controller_watcher(
            &self.scope,
            view_controller_proxy,
            self.internal_sender.clone(),
        );
        Self::spawn_layout_info_watcher(
            &self.scope,
            parent_viewport_watcher,
            self.internal_sender.clone(),
        );
        touch::spawn_touch_source_watcher(&self.scope, touch, self.internal_sender.clone());
        mouse::spawn_mouse_source_watcher(&self.scope, mouse, self.internal_sender.clone());

        match focuser.request_focus(view_ref_for_focuser).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Focuser.RequestFocus() denied: {:?}", e),
            Err(e) => warn!("Focuser.RequestFocus() FIDL error: {:?}", e),
        }
        self.setup_optional_keyboard(view_ref_for_keyboard).await;
        true
    }

    fn spawn_view_controller_watcher(
        scope: &fasync::ScopeHandle,
        view_controller: ViewControllerProxy,
        sender: UnboundedSender<InternalMessage>,
    ) {
        scope.spawn(async move {
            // Hold `view_controller` open until the presenter closes the channel to dismiss us.
            let _ = view_controller.on_closed().await;
            let _ = sender.unbounded_send(InternalMessage::ViewDismissed);
        });
    }

    fn spawn_layout_info_watcher(
        scope: &fasync::ScopeHandle,
        parent_viewport_watcher: fland::ParentViewportWatcherProxy,
        sender: UnboundedSender<InternalMessage>,
    ) {
        scope.spawn(async move {
            let mut layout_info_stream = HangingGetStream::new(
                parent_viewport_watcher,
                fland::ParentViewportWatcherProxy::get_layout,
            );

            while let Some(result) = layout_info_stream.next().await {
                match result {
                    Ok(layout_info) => {
                        let _ = sender.unbounded_send(InternalMessage::Relayout {
                            size: layout_info
                                .logical_size
                                .unwrap_or(fmath::SizeU { width: 0, height: 0 }),
                        });
                    }
                    Err(fidl_error) => {
                        warn!("ParentViewportWatcher GetLayout() error: {:?}", fidl_error);
                        let _ = sender.unbounded_send(InternalMessage::ViewDismissed);
                        return;
                    }
                }
            }
            let _ = sender.unbounded_send(InternalMessage::ViewDismissed);
        });
    }

    fn on_relayout(&mut self, size: fmath::SizeU) {
        self.scene.set_viewport_size(size);
        self.scene.render(self.flatland, &self.game);
    }

    fn on_tick(&mut self) -> bool {
        if self.game.step() {
            self.scene.render(self.flatland, &self.game);
            true
        } else {
            false
        }
    }

    fn on_command(&mut self, cmd: GameCommand) -> bool {
        let changed = match cmd {
            GameCommand::Move(dir) => self.game.queue_direction(dir),
            GameCommand::TogglePause => self.game.toggle_pause(),
            GameCommand::Restart => {
                self.game.reset();
                true
            }
        };
        if changed {
            self.scene.render(self.flatland, &self.game);
        }
        changed
    }

    fn on_pointer_tap(&mut self, x: f32, y: f32) -> bool {
        if let Some(cmd) = self.scene.hit_test_dpad(x, y) {
            return self.on_command(cmd);
        }
        let (grid_x, grid_y) = self.scene.view_to_grid(x, y);
        let changed = self.game.handle_pointer_tap(grid_x, grid_y);
        if changed {
            self.scene.render(self.flatland, &self.game);
        }
        changed
    }
}

fn setup_handle_flatland_events(
    scope: &fasync::ScopeHandle,
    event_stream: fland::FlatlandEventStream,
    sender: UnboundedSender<InternalMessage>,
) {
    scope.spawn(
        event_stream
            .try_for_each(move |event| {
                match event {
                    fland::FlatlandEvent::OnNextFrameBegin { .. } => {
                        let _ = sender.unbounded_send(InternalMessage::OnNextFrameBegin);
                    }
                    fland::FlatlandEvent::OnFramePresented { .. } => {}
                    fland::FlatlandEvent::OnError { error } => {
                        let _ = sender.unbounded_send(InternalMessage::OnPresentError { error });
                    }
                }
                future::ok(())
            })
            .unwrap_or_else(|e| error!("error listening for Flatland Events: {:?}", e)),
    );
}

fn spawn_game_ticker(scope: &fasync::ScopeHandle, sender: UnboundedSender<InternalMessage>) {
    scope.spawn(async move {
        let mut interval = fasync::Interval::new(Duration::from_millis(TICK_INTERVAL_MS).into());
        while interval.next().await.is_some() {
            if sender.unbounded_send(InternalMessage::Tick).is_err() {
                break;
            }
        }
    });
}

async fn connect_flatland2() -> fland::FlatlandProxy {
    let factory = connect_to_protocol::<fland::FlatlandFactoryMarker>()
        .expect("error connecting to FlatlandFactory");
    let (flatland, server_end) = create_proxy::<fland::FlatlandMarker>();
    let config = fland::FlatlandConfig { use_flatland2: Some(true), ..Default::default() };
    factory
        .create_flatland(server_end, &config)
        .await
        .expect("fidl error calling FlatlandFactory::CreateFlatland")
        .expect("FlatlandFactory::CreateFlatland failed");
    flatland
}

#[fuchsia::main]
async fn main() {
    info!("Starting snake-flatland (Flatland2)");

    let scope = fasync::Scope::new();
    let scope_handle = scope.to_handle();
    let (internal_sender, mut internal_receiver) = unbounded::<InternalMessage>();

    let flatland = connect_flatland2().await;
    flatland.set_debug_name("snake-flatland").expect("fidl error");

    let graphical_presenter = connect_to_protocol::<GraphicalPresenterMarker>()
        .expect("error connecting to GraphicalPresenter");

    setup_handle_flatland_events(
        &scope_handle,
        flatland.take_event_stream(),
        internal_sender.clone(),
    );

    let mut app = AppModel::new(
        &flatland,
        &graphical_presenter,
        internal_sender.clone(),
        scope_handle.clone(),
    );
    if !app.create_view().await {
        return;
    }

    spawn_game_ticker(&scope_handle, internal_sender.clone());

    let mut can_present = false;
    let mut scene_dirty = false;

    while let Some(message) = internal_receiver.next().await {
        match message {
            InternalMessage::Relayout { size } => {
                app.on_relayout(size);
                scene_dirty = true;
            }
            InternalMessage::OnPresentError { error } => {
                error!("OnPresentError({:?})", error);
                break;
            }
            InternalMessage::ViewDismissed => {
                info!("View dismissed or layout watcher closed; exiting");
                break;
            }
            InternalMessage::OnNextFrameBegin => {
                can_present = true;
            }
            InternalMessage::Tick => {
                if app.on_tick() {
                    scene_dirty = true;
                }
            }
            InternalMessage::Command(cmd) => {
                if app.on_command(cmd) {
                    scene_dirty = true;
                }
            }
            InternalMessage::PointerTap { x, y } => {
                if app.on_pointer_tap(x, y) {
                    scene_dirty = true;
                }
            }
        }

        if can_present && scene_dirty {
            flatland.present(fland::PresentArgs::default()).expect("Present call failed");
            can_present = false;
            scene_dirty = false;
        }
    }
}
