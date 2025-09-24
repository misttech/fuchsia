// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component::inspector;
use futures::StreamExt;
use futures::channel::mpsc;
use state_recorder::StateRecorder;
use strum::IntoEnumIterator;
use strum_macros::{Display, EnumIter, FromRepr};

#[derive(Copy, Clone, Display, EnumIter, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
enum FanSpeed {
    OFF = 0,
    LOW = 1,
    HIGH = 2,
}

impl From<FanSpeed> for u64 {
    fn from(value: FanSpeed) -> Self {
        value as Self
    }
}

#[fuchsia::main(logging_tags = ["power_observability", "example"])]
async fn main() -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    log::info!("Starting example");

    // Set up tracing
    fuchsia_trace_provider::trace_provider_create_with_fdio();

    let _inspect_server_task =
        inspect_runtime::publish(inspector(), inspect_runtime::PublishOptions::default());
    inspector().root().record_string("version", "foo");

    let mut recorder = StateRecorder::new("fan_speed".into(), c"power_example", FanSpeed::OFF, 10)
        .expect("StateRecorder construction failed");

    let (mut sender, mut receiver) = mpsc::channel(10);

    // Simulate some state transitions
    fasync::Task::local(async move {
        for _ in 0..30 {
            for value in FanSpeed::iter() {
                sender.try_send(value).unwrap();
                fasync::Timer::new(std::time::Duration::from_secs(1)).await;
            }
        }
    })
    .detach();

    // Record ticks on the process track so the state transitions themselves don't dictate the
    // trace timeline.
    fasync::Task::local(async move {
        loop {
            fuchsia_trace::instant!(c"power_example", c"tick", fuchsia_trace::Scope::Process);
            fasync::Timer::new(std::time::Duration::from_millis(100)).await;
        }
    })
    .detach();

    fasync::Task::local(async move {
        while let Some(state) = receiver.next().await {
            recorder.record_transition(state);
        }
        let () = std::future::pending().await;
    })
    .detach();

    fs.take_and_serve_directory_handle()?;
    fs.collect::<()>().await;

    Ok(())
}
