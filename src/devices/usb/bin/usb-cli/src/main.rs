// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use argh::FromArgs;
use fidl_fuchsia_diagnostics as _;
use fidl_fuchsia_usb_policy as usb_policy;

mod health;
mod inspect;

#[derive(FromArgs)]
/// USB diagnostics tool.
struct UsbCliArgs {
    /// prints the usb policy health report
    #[argh(switch, short = 'H')]
    health: bool,

    /// prints the device-side usb inspect diagnostics
    #[argh(switch, short = 'i')]
    inspect: bool,

    /// prints both health report and inspect diagnostics
    #[argh(switch, short = 'a')]
    all: bool,
}

#[fuchsia::main(logging_tags = ["usb-cli"])]
async fn main() {
    if let Err(e) = run_cli().await {
        eprintln!("usb-cli error: {:?}", e);
        std::process::exit(1);
    }
}

async fn get_health_report() -> Result<usb_policy::HealthReport, Error> {
    let health =
        fuchsia_component::client::connect_to_protocol_at_path::<usb_policy::HealthMarker>(
            "/exposed/fuchsia.usb.policy.Health",
        )
        .map_err(|e| {
            anyhow::format_err!("Failed to connect to Health protocol at /exposed: {:?}", e)
        })?;

    health
        .get_report()
        .await
        .map_err(|e| anyhow::format_err!("Failed to communicate (get_report): {:?}", e))?
        .map_err(|e| {
            anyhow::format_err!("Failed to get report (zx status): {:?}", zx::Status::from_raw(e))
        })
}

async fn run_cli() -> Result<(), Error> {
    let args: UsbCliArgs = argh::from_env();

    // Default mode (no flags) runs health.
    let default_mode = !args.inspect && !args.health && !args.all;
    let run_health = args.health || args.all || default_mode;
    let run_inspect = args.inspect || args.all;

    let mut report_res = Ok(None);

    if run_health {
        report_res = get_health_report().await.map(Some);
    }

    match report_res {
        Ok(Some(report)) => println!("{}", health::format_report(&report)),
        Ok(None) => {}
        Err(e) => {
            if args.health {
                println!(
                    "USB Policy Health service not available: fuchsia.usb.policy.Health not found."
                );
                return Err(e);
            } else {
                println!("USB Policy Health service not available (skipping).");
            }
        }
    }

    if run_inspect {
        if let Err(e) = inspect::print_usb_inspect_diagnostics().await {
            println!("Failed to print USB inspect diagnostics: {:?}", e);
        }
    }

    Ok(())
}
