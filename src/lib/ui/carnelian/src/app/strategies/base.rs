// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::app::strategies::flatland::FlatlandAppStrategy;
use crate::app::strategies::framebuffer::{
    first_display_device_path, DisplayCoordinator, DisplayDirectAppStrategy, DisplayId,
};
use crate::app::{Config, InternalSender, MessageInternal, ViewMode};
use crate::input::{self};
use crate::view::strategies::base::{ViewStrategyParams, ViewStrategyPtr};
use crate::view::ViewKey;
use anyhow::Error;
use async_trait::async_trait;
use fidl_fuchsia_hardware_display::VirtconMode;
use fidl_fuchsia_input_report as hid_input_report;
use fuchsia_component::server::{ServiceFs, ServiceObjLocal};
use futures::channel::mpsc::UnboundedSender;
use keymaps::select_keymap;
use std::path::PathBuf;

// This trait exists to keep the hosted implementation and the
// direct implementations as separate as possible.
// At the moment this abstraction is quite leaky, but it is good
// enough and can be refined with experience.
#[async_trait(?Send)]
pub(crate) trait AppStrategy {
    async fn create_view_strategy(
        &mut self,
        key: ViewKey,
        app_sender: UnboundedSender<MessageInternal>,
        strategy_params: ViewStrategyParams,
    ) -> Result<ViewStrategyPtr, Error>;
    #[allow(dead_code)]
    fn supports_scenic(&self) -> bool;
    fn create_view_for_testing(&self, _: &UnboundedSender<MessageInternal>) -> Result<(), Error> {
        Ok(())
    }
    fn create_view_strategy_params_for_additional_view(
        &mut self,
        view_key: ViewKey,
    ) -> ViewStrategyParams;
    fn start_services<'a, 'b>(
        &self,
        _app_sender: UnboundedSender<MessageInternal>,
        _fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn post_setup(&mut self, _internal_sender: &InternalSender) -> Result<(), Error>;
    fn handle_input_report(
        &mut self,
        _device_id: &input::DeviceId,
        _input_report: &hid_input_report::InputReport,
    ) -> Vec<input::Event> {
        Vec::new()
    }
    fn handle_keyboard_autorepeat(&mut self, _device_id: &input::DeviceId) -> Vec<input::Event> {
        Vec::new()
    }
    fn handle_register_input_device(
        &mut self,
        _device_id: &input::DeviceId,
        _device_descriptor: &hid_input_report::DeviceDescriptor,
    ) {
    }
    async fn handle_new_display_coordinator(&mut self, _display_path: PathBuf) {}
    async fn handle_display_coordinator_event(
        &mut self,
        _event: fidl_fuchsia_hardware_display::CoordinatorListenerRequest,
    ) {
    }
    fn set_virtcon_mode(&mut self, _virtcon_mode: VirtconMode) {}
    fn handle_view_closed(&mut self, _view_key: ViewKey) {}
    fn get_focused_view_key(&self) -> Option<ViewKey> {
        panic!("get_focused_view_key not implemented");
    }
    fn get_visible_view_key_for_display(&self, _display_id: DisplayId) -> Option<ViewKey> {
        panic!("get_visible_view_key_for_display not implemented");
    }
}

pub(crate) type AppStrategyPtr = Box<dyn AppStrategy>;

fn make_flatland_app_strategy() -> Result<AppStrategyPtr, Error> {
    Ok::<AppStrategyPtr, Error>(Box::new(FlatlandAppStrategy {}))
}

fn make_direct_app_strategy(
    display_coordinator: Option<DisplayCoordinator>,
    app_config: &Config,
    internal_sender: InternalSender,
) -> Result<AppStrategyPtr, Error> {
    let strat = DisplayDirectAppStrategy::new(
        display_coordinator,
        select_keymap(&app_config.keymap_name),
        internal_sender,
        &app_config,
    );

    Ok(Box::new(strat))
}

pub(crate) async fn create_app_strategy(
    internal_sender: &InternalSender,
) -> Result<AppStrategyPtr, Error> {
    let app_config = Config::get();
    match app_config.view_mode {
        ViewMode::Auto => {
            // Tries to open the display coordinator. If that fails, assume we want to run as hosted.
            let display_coordinator = if let Some(path) = first_display_device_path() {
                DisplayCoordinator::open(
                    path.to_str().unwrap(),
                    &app_config.virtcon_mode,
                    &internal_sender,
                )
                .await
                .ok()
            } else {
                None
            };
            if display_coordinator.is_none() {
                make_flatland_app_strategy()
            } else {
                make_direct_app_strategy(display_coordinator, app_config, internal_sender.clone())
            }
        }
        ViewMode::Direct => {
            DisplayCoordinator::watch_displays(internal_sender.clone()).await;
            make_direct_app_strategy(None, app_config, internal_sender.clone())
        }
        ViewMode::Hosted => make_flatland_app_strategy(),
    }
}
