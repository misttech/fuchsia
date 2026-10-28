// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use display_utils::{Coordinator, DisplayId, PixelFormat};
use std::num::NonZero;
use std::time::Duration;

mod bouncing_squares;
mod cadence;
mod display_color_layer;
mod frame_rate_test;
mod multilayer_squares;
mod static_config_vsync_loop;

use crate::rgb::Rgb888;
use crate::runner::PowerCycle;

pub fn show_display_info(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    fidl: bool,
) -> Result<()> {
    let displays = coordinator.displays();
    println!("{} display(s) available", displays.len());
    for display in displays.iter().filter(|&info| id.map_or(true, |id| id == info.0.id.into())) {
        if fidl {
            println!("{:#?}", display.0);
        } else {
            println!("{}", display);
        }
    }
    Ok(())
}

pub async fn vsync(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    color: Rgb888,
    pixel_format: PixelFormat,
) -> Result<()> {
    let displays = coordinator.displays();
    if displays.is_empty() {
        return Err(format_err!("no displays found"));
    }

    let display = match id {
        // Pick the first available display if no ID was specified.
        None => &displays[0],
        Some(id) => displays
            .iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id))?,
    };

    static_config_vsync_loop::run(
        coordinator,
        static_config_vsync_loop::Args { display, color, pixel_format },
    )
    .await
}

pub async fn color(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    color: Rgb888,
    pixel_format: PixelFormat,
) -> Result<()> {
    let displays = coordinator.displays();
    if displays.is_empty() {
        return Err(format_err!("no displays found"));
    }

    let display = match id {
        // Pick the first available display if no ID was specified.
        None => &displays[0],
        Some(id) => displays
            .iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id))?,
    };

    display_color_layer::run(
        coordinator,
        display_color_layer::Args { display, color, pixel_format },
    )
    .await
}

/// Runs the bouncing squares swapchain animation on `id` (or the first available display).
///
/// # Outcome
///
/// Returns an error if no displays are attached, `id` is not found, or the animation loop fails.
pub async fn squares(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    power_cycle: PowerCycle,
    vsync_timeout: Duration,
) -> Result<()> {
    let displays = coordinator.displays();
    if displays.is_empty() {
        return Err(format_err!("no displays found"));
    }

    let display = match id {
        // Pick the first available display if no ID was specified.
        None => &displays[0],
        Some(id) => displays
            .iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id))?,
    };

    bouncing_squares::run(coordinator, display, power_cycle, vsync_timeout).await
}

pub async fn frame_rate_test(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    grid_width: Option<u32>,
    grid_height: Option<u32>,
    mode_index: usize,
) -> Result<()> {
    let displays = coordinator.displays();
    if displays.is_empty() {
        return Err(format_err!("no displays found"));
    }

    let display = match id {
        // Pick the first available display if no ID was specified.
        None => &displays[0],
        Some(id) => displays
            .iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id))?,
    };

    frame_rate_test::run(coordinator, display, grid_width, grid_height, mode_index).await
}

pub async fn multilayer_squares(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    layer_count: usize,
) -> Result<()> {
    let displays = coordinator.displays();
    if displays.is_empty() {
        return Err(format_err!("no displays found"));
    }

    let display = match id {
        // Pick the first available display if no ID was specified.
        None => &displays[0],
        Some(id) => displays
            .iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id))?,
    };

    multilayer_squares::run(coordinator, display, layer_count).await
}

/// Arguments for [`cadence`].
pub struct CadenceArgs {
    /// Frames per second, or 0 to present one frame.
    pub fps: f64,
    /// Frames per burst. Requires `idle_ms`.
    pub burst_frames: Option<NonZero<u64>>,
    /// Idle time between bursts. Requires `burst_frames`.
    pub idle_ms: Option<u64>,
    /// Length of the run.
    pub duration: Duration,
    /// Print a report every second.
    pub verbose: bool,
    /// Print a line for every vsync event.
    pub log_vsync: bool,
}

/// Presents frames on a timer-driven schedule on `id` (or the first available display) and
/// reports the vsync cadence.
///
/// # Outcome
///
/// Returns an error if the arguments are invalid, no displays are attached, `id` is not found, or
/// the measurement fails.
pub async fn cadence(
    coordinator: &Coordinator,
    id: Option<DisplayId>,
    args: CadenceArgs,
) -> Result<()> {
    let schedule = cadence::Schedule::new(args.fps, args.burst_frames, args.idle_ms)?;
    let duration_ns = i64::try_from(args.duration.as_nanos())
        .map_err(|_| format_err!("--duration-s is too large"))?;
    let display = crate::mode_selection::find_display(coordinator, id)?;
    cadence::run(
        coordinator,
        cadence::Args {
            display: &display,
            schedule,
            duration_ns,
            verbose: args.verbose,
            log_vsync: args.log_vsync,
        },
    )
    .await
}
