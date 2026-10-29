// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::app::strategies::base::AppStrategy;
use crate::app::{InternalSender, MessageInternal};
use crate::view::ViewKey;
use crate::view::strategies::base::{FlatlandParams, ViewStrategyParams, ViewStrategyPtr};
use crate::view::strategies::flatland::{FLATLAND_DEBUG_NAME, FlatlandViewStrategy};
use anyhow::{Error, bail};
use async_trait::async_trait;
use fuchsia_component::server::{ServiceFs, ServiceObjLocal};
use futures::channel::mpsc::UnboundedSender;

pub(crate) struct FlatlandAppStrategy;

#[async_trait(?Send)]
impl AppStrategy for FlatlandAppStrategy {
    async fn create_view_strategy(
        &mut self,
        key: ViewKey,
        app_sender: UnboundedSender<MessageInternal>,
        strategy_params: ViewStrategyParams,
    ) -> Result<ViewStrategyPtr, Error> {
        match strategy_params {
            ViewStrategyParams::Flatland(flatland_params) => {
                Ok(FlatlandViewStrategy::new(key, flatland_params, app_sender.clone()).await?)
            }
            _ => bail!(
                "Incorrect ViewStrategyParams passed to create_view_strategy for Scenic (Flatland)"
            ),
        }
    }

    fn create_view_strategy_params_for_additional_view(
        &mut self,
        view_key: ViewKey,
    ) -> ViewStrategyParams {
        ViewStrategyParams::Flatland(FlatlandParams {
            view_key: Some(view_key),
            debug_name: Some(FLATLAND_DEBUG_NAME.to_string()),
        })
    }

    fn supports_scenic(&self) -> bool {
        return true;
    }

    // Hosted mode creates its primary view here because no external ViewProvider
    // caller exists any more; `App::test()` also relies on this to initialize the view.
    fn start_services<'a, 'b>(
        &self,
        app_sender: UnboundedSender<MessageInternal>,
        _fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
    ) -> Result<(), Error> {
        app_sender
            .unbounded_send(MessageInternal::CreateView(ViewStrategyParams::Flatland(
                FlatlandParams {
                    view_key: None,
                    debug_name: Some(FLATLAND_DEBUG_NAME.to_string()),
                },
            )))
            .expect("unbounded_send");
        Ok(())
    }

    async fn post_setup(&mut self, _internal_sender: &InternalSender) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_view_strategy_params_for_additional_view() {
        let mut strategy = FlatlandAppStrategy;
        let params = strategy.create_view_strategy_params_for_additional_view(ViewKey(7));
        assert_eq!(params.view_key(), Some(ViewKey(7)));
    }
}
