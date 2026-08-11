// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::callback_interface::{Interface, Request, Session, SessionManager};
use crate::verifier::Verifier;
use crate::{BlockInfo, DeviceInfo, OffsetMap};
use fuchsia_sync::Mutex;
use std::borrow::Cow;
use std::sync::Arc;

pub struct MockInterface {
    pub request_sender: std::sync::mpsc::Sender<Request>,
    pub blobs: Arc<mapping::Blobs>,
    pub verifier: Mutex<Option<Arc<Verifier>>>,
}

impl MockInterface {
    pub fn new(request_sender: std::sync::mpsc::Sender<Request>) -> Self {
        Self { request_sender, blobs: Arc::new(mapping::Blobs::new()), verifier: Mutex::new(None) }
    }
}

impl Interface for MockInterface {
    type Orchestrator = SessionManager<Self>;

    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Owned(DeviceInfo::Block(BlockInfo { block_count: 1024, ..Default::default() }))
    }

    fn spawn_session(&self, session: Arc<Session<Self>>) {
        std::thread::spawn(move || {
            session.run();
        });
    }

    fn on_requests(&self, requests: &[Request]) {
        for request in requests {
            self.request_sender.send(request.clone()).unwrap();
        }
    }

    fn on_open_mapper_session(
        &self,
        _mapping_vmo: &zx::Vmo,
        _offset_map: &OffsetMap,
        delivery_queue: zx::Vmo,
    ) -> Result<(Arc<mapping::Blobs>, Arc<Verifier>), zx::Status> {
        let verifier = Arc::new(Verifier::new(delivery_queue));
        *self.verifier.lock() = Some(verifier.clone());
        Ok((self.blobs.clone(), verifier))
    }
}
