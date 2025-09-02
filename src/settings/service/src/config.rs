// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::AgentCreator;
use settings_common::config::AgentType;

#[macro_export]
macro_rules! create_agent {
    ($component:ident, $create:expr) => {
        AgentCreator {
            debug_id: concat!(stringify!($component), "_agent"),
            create: $crate::agent::CreationFunc::Static(|c| Box::pin($create(c))),
        }
    };
}

impl AgentCreator {
    pub(crate) fn from_type(agent_type: AgentType) -> Option<AgentCreator> {
        use crate::agent::*;
        Some(match agent_type {
            AgentType::CameraWatcher => {
                create_agent!(camera_watcher, camera_watcher::CameraWatcherAgent::create)
            }
            AgentType::Earcons => create_agent!(earcons, earcons::agent::Agent::create),
            AgentType::MediaButtons
            | AgentType::InspectSettingValues
            | AgentType::InspectExternalApis
            | AgentType::InspectSettingProxy
            | AgentType::InspectSettingTypeUsage => {
                // Moved to lib.rs
                return None;
            }
            AgentType::Restore => create_agent!(restore_agent, restore_agent::RestoreAgent::create),
        })
    }
}
