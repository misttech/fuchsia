// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context as _, Error};
use config::Config;
use fidl_examples_canvas_baseline::{InstanceEvent, InstanceMarker, Point};
use fuchsia_component::client::connect_to_protocol;
use futures::TryStreamExt;
use std::{thread, time};

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    println!("Started");

    // Load the structured config values passed to this component at startup.
    let config = Config::take_from_startup_handle();

    // Use the Component Framework runtime to connect to the newly spun up server component. We wrap
    // our retained client end in a proxy object that lets us asynchronously send Instance requests
    // across the channel.
    let instance = connect_to_protocol::<InstanceMarker>()?;
    println!("Outgoing connection enabled");

    for action in config.script.into_iter() {
        // If the next action in the script is to "WAIT", block until an OnDrawn event is received
        // from the server.
        if action == "WAIT" {
            let mut event_stream = instance.take_event_stream();
            loop {
                match event_stream
                    .try_next()
                    .await
                    .context("Error getting event response from proxy")?
                    .ok_or_else(|| format_err!("Proxy sent no events"))?
                {
                    InstanceEvent::OnDrawn { top_left, bottom_right } => {
                        println!(
                            "OnDrawn event received: top_left: {:?}, bottom_right: {:?}",
                            top_left, bottom_right
                        );
                        break;
                    }
                    InstanceEvent::_UnknownEvent { ordinal, .. } => {
                        println!("Received an unknown event with ordinal {ordinal}");
                    }
                }
            }
            continue;
        }

        // If the action is not a "WAIT", we need to draw a line instead. Parse the string input,
        // making two points out of it.
        let mut points = action
            .split(":")
            .map(|point| {
                let integers = point
                    .split(",")
                    .map(|integer| integer.parse::<i64>().unwrap())
                    .collect::<Vec<i64>>();
                Point { x: integers[0], y: integers[1] }
            })
            .collect::<Vec<Point>>();

        // Assemble a line from the two points.
        let from = points.pop().ok_or_else(|| format_err!("line requires 2 points, but has 0"))?;
        let to = points.pop().ok_or_else(|| format_err!("line requires 2 points, but has 1"))?;
        let line = [from, to];

        // Draw a line to the canvas by calling the server, using the two points we just parsed
        // above as arguments.
        instance.add_line(&line)?;
        println!("AddLine request sent: {:?}", line);
    }

    // TODO(https://fxbug.dev/42156498): We need to sleep here to make sure all logs get drained. Once the
    // referenced bug has been resolved, we can remove the sleep.
    thread::sleep(time::Duration::from_secs(2));
    Ok(())
}
