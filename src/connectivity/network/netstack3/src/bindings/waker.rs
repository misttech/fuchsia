// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_matches::assert_matches;
use futures::future::{FusedFuture, OptionFuture};
use futures::{FutureExt as _, TryStreamExt as _};
use log::{debug, info, warn};
use netstack3_core::sync::Mutex;
use zx::{AsHandleRef, HandleBased};
use {
    fidl_fuchsia_net_power as fnet_power, fidl_fuchsia_net_resources as fnet_resources,
    fuchsia_async as fasync,
};

use crate::bindings::util::{DataNotifier, DataWatcher, ResultExt as _, ScopeExt as _};

#[derive(Default, Clone)]
pub(crate) struct WakeGroups(Arc<Mutex<WakeGroupsInner>>);

#[derive(Default)]
struct WakeGroupsInner {
    wake_groups: HashMap<zx::Koid, DataNotifier>,
}

impl WakeGroups {
    pub(crate) async fn serve_provider(
        self,
        mut stream: fnet_power::WakeGroupProviderRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.try_next().await? {
            let fnet_power::WakeGroupProviderRequest::CreateWakeGroup {
                options,
                request,
                responder,
            } = request;
            let fnet_power::WakeGroupOptions { debug_name, __source_breaking } = options;

            let debug_name = debug_name.unwrap_or_else(|| {
                static COUNTER: AtomicUsize = AtomicUsize::new(0);
                format!("wake-group-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
            });
            let id = self.create_wake_group(debug_name, request.into_stream());

            responder
                .send(fnet_power::CreateWakeGroupResponse {
                    token: Some(fnet_resources::WakeGroupToken { token: id.token }),
                    __source_breaking: fidl::marker::SourceBreaking,
                })
                .unwrap_or_log("failed to respond to CreateWakeGroup");
        }
        Ok(())
    }

    fn create_wake_group(
        &self,
        debug_name: String,
        stream: fnet_power::WakeGroupRequestStream,
    ) -> WakeGroupId {
        let WakeGroups(inner) = self;

        let (data_watcher, data_notifier) = DataWatcher::new();
        let wake_group = WakeGroup::new(debug_name, data_watcher);
        let id = wake_group.id.duplicate();

        assert_matches!(
            inner.lock().wake_groups.insert(id.koid, data_notifier),
            None,
            "koid of new wake group should be unique",
        );

        info!("creating wake group '{}' {id:?}", wake_group.name);

        let wake_groups = self.clone();
        fasync::Scope::current().spawn_request_stream_handler(stream, |stream| async move {
            wake_group.serve(stream, wake_groups).await
        });

        id
    }

    fn remove_wake_group(&self, id: WakeGroupId) {
        let WakeGroups(inner) = self;
        assert_matches!(inner.lock().wake_groups.remove(&id.koid), Some(_));
    }

    pub(crate) fn get_data_notifier(&self, wake_group: &WakeGroupId) -> Option<DataNotifier> {
        let Self(inner) = self;
        let wake_groups = &inner.lock().wake_groups;
        let data_notifier = wake_groups.get(&wake_group.koid)?;
        Some(data_notifier.clone())
    }
}

#[derive(Debug)]
pub(crate) struct WakeGroupId {
    token: zx::Event,
    koid: zx::Koid,
}

impl WakeGroupId {
    fn new() -> Self {
        let token = zx::Event::create();
        let koid = token.get_koid().expect("get koid of wake group token");
        Self { token, koid }
    }

    fn duplicate(&self) -> Self {
        let Self { token, koid } = self;
        let token = token
            .duplicate_handle(zx::Rights::TRANSFER | zx::Rights::DUPLICATE)
            .expect("must be able to duplicate wake group token");

        Self { token, koid: *koid }
    }
}

impl From<fnet_resources::WakeGroupToken> for WakeGroupId {
    fn from(token: fnet_resources::WakeGroupToken) -> Self {
        let fnet_resources::WakeGroupToken { token } = token;
        let koid = token.get_koid().expect("get koid of wake group token");
        Self { token, koid }
    }
}

#[derive(Debug)]
struct WakeGroup {
    name: String,
    id: WakeGroupId,
    watcher: DataWatcher,
}

impl WakeGroup {
    fn new(name: String, watcher: DataWatcher) -> Self {
        Self { name, id: WakeGroupId::new(), watcher }
    }

    async fn serve(
        self,
        mut stream: fnet_power::WakeGroupRequestStream,
        wake_groups: WakeGroups,
    ) -> Result<(), fidl::Error> {
        let Self { name, id: _, mut watcher } = self;

        let mut hanging_get = None;
        let mut data_available = OptionFuture::default();

        loop {
            enum Work {
                Request(fnet_power::WakeGroupRequest),
                DataAvailable(fnet_power::WakeGroupWaitForDataResponder),
            }

            let work = futures::select! {
                request = stream.try_next() => {
                    let Some(request) = request? else {
                        break;
                    };
                    Work::Request(request)
                }
                responder = data_available => {
                    let responder = responder.expect("OptionFuture must yield Some in select");
                    Work::DataAvailable(responder)
                }
            };

            match work {
                Work::Request(request) => match request {
                    fnet_power::WakeGroupRequest::WaitForData { responder } => {
                        if hanging_get.is_some() {
                            warn!(
                                "wake group '{name}': WaitForData called when call was already \
                                hanging; closing channel",
                            );
                            break;
                        }
                        if !data_available.is_terminated() {
                            warn!(
                                "wake group '{name}': WaitForData called when call was already \
                                hanging and armed; closing channel",
                            );
                            break;
                        }
                        hanging_get = Some(responder);
                    }
                    fnet_power::WakeGroupRequest::Arm { responder } => {
                        if !data_available.is_terminated() {
                            warn!(
                                "wake group '{name}': Arm called when WaitForData was already \
                                hanging and armed; closing channel",
                            );
                            break;
                        }
                        let Some(hanging_get_responder) = hanging_get.take() else {
                            // NB: Attempting to arm the hanging get when there is no hanging get is
                            // a no-op.
                            //
                            // This handles a possible race condition where the netstack's response
                            // to the hanging get (in order to delegate a lease) races with the
                            // client arming the hanging get. In this scenario, `Arm` may be called
                            // with no hanging get pending. In that scenario, rather than close the
                            // channel, we just ignore the `Arm` and expect the client to post
                            // another hanging get (and eventually arm it when they wish to be woken
                            // up).
                            responder.send().unwrap_or_log("failed to respond to WakeGroup::Arm");
                            continue;
                        };

                        // Drop `data_available` since it's holding a mutable borrow of `watcher`.
                        drop(data_available);

                        // NB: only send the response once we've registered the waiter, so that
                        // any packets that arrive after the method returns to the caller will
                        // wake the caller.
                        data_available =
                            Some(watcher.reset_and_wait().map(|()| hanging_get_responder)).into();

                        debug!("wake group '{name}' is armed");

                        responder.send().unwrap_or_log("failed to respond to WakeGroup::Arm");
                    }
                },
                Work::DataAvailable(responder) => {
                    let response = fnet_power::WakeGroupWaitForDataResponse {
                        source: Some(fnet_power::WakeSource::Data(fnet_power::Empty {})),
                        __source_breaking: fidl::marker::SourceBreaking,
                    };
                    responder
                        .send(response)
                        .unwrap_or_log("failed to respond to WakeGroup::WaitForData");

                    debug!("notified wake group '{name}' of incoming data");
                }
            }
        }

        // Tear down this wake group. Note that sockets that were attached to this wake
        // group still hold onto their `DataNotifier`s, but without a watcher, those
        // notifications are no-ops.
        wake_groups.remove_wake_group(self.id);

        debug!("removed wake group '{name}'");

        Ok(())
    }
}
