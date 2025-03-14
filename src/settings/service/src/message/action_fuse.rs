// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_async as fasync;
use futures::lock::Mutex;
use std::rc::Rc;

/// Closure definition for an action that can be triggered by ActionFuse.
pub type TriggeredAction = Box<dyn FnOnce()>;
/// The reference-counted handle to an ActionFuse. When all references go out of
/// scope, the action will be triggered (if not defused).
pub type ActionFuseHandle = Rc<Mutex<ActionFuse>>;

/// ActionFuse is a wrapper around a triggered action (a closure with no
/// arguments and no return value). This action is invoked once the fuse goes
/// out of scope (via the Drop trait). An ActionFuse can be defused, preventing
/// the action from automatically invoking when going out of scope.
pub struct ActionFuse {
    /// An optional action that will be invoked when the ActionFuse goes out of
    /// scope.
    actions: Option<TriggeredAction>,
}

impl ActionFuse {
    /// Returns an ActionFuse reference with the given TriggerAction.
    pub(crate) fn create(action: TriggeredAction) -> ActionFuseHandle {
        Rc::new(Mutex::new(ActionFuse { actions: Some(action) }))
    }

    /// Suppresses the action from automatically executing.
    pub(crate) fn defuse(handle: ActionFuseHandle) {
        fasync::Task::local(async move {
            let mut fuse = handle.lock().await;
            fuse.actions = None;
        })
        .detach();
    }
}

impl Drop for ActionFuse {
    fn drop(&mut self) {
        if let Some(action) = self.actions.take() {
            (action)();
        }
    }
}
