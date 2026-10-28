// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Selects a display mode by refresh rate, for the top-level `--refresh-rate` option.

use anyhow::{Context, Result, format_err};
use display_utils::{Coordinator, DisplayId, DisplayInfo};
use fidl_fuchsia_hardware_display_types as fidl_display_types;

/// Returns the index in `modes` of the mode whose refresh rate is closest to `refresh_rate_hz`.
///
/// Only modes with the same active area as `modes[0]` are considered. The tool's commands size
/// their images from `modes[0]`, so a mode with another resolution would not fit them.
///
/// Ties go to the mode listed first (in preference order).
///
/// Returns [`None`] if `modes` is empty.
pub fn closest_refresh_rate_mode(
    modes: &[fidl_display_types::Mode],
    refresh_rate_hz: f64,
) -> Option<usize> {
    let active_area = modes.first()?.active_area;
    let target_millihertz = refresh_rate_hz * 1000.0;
    let mut best: Option<(usize, f64)> = None;
    for (index, mode) in modes.iter().enumerate() {
        if mode.active_area != active_area {
            continue;
        }
        let distance = (f64::from(mode.refresh_rate_millihertz) - target_millihertz).abs();
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((index, distance));
        }
    }
    best.map(|(index, _)| index)
}

/// Relative difference between a requested and a selected refresh rate above which
/// [`set_refresh_rate`] warns that the display has no mode near the requested rate.
const REFRESH_RATE_WARNING_TOLERANCE: f64 = 0.02;

/// Returns true iff `selected_millihertz` differs from `requested_hz` by more than
/// [`REFRESH_RATE_WARNING_TOLERANCE`] of `requested_hz`.
fn is_far_from_requested_rate(requested_hz: f64, selected_millihertz: u32) -> bool {
    let selected_hz = f64::from(selected_millihertz) / 1000.0;
    (selected_hz - requested_hz).abs() > requested_hz * REFRESH_RATE_WARNING_TOLERANCE
}

/// Requests the mode of `display` whose refresh rate is closest to `refresh_rate_hz`.
///
/// `SetDisplayMode` only changes the client's draft configuration. The mode takes effect when the
/// command that runs next commits a configuration. `main` does not call this for `info`, which
/// commits no configuration.
///
/// # Outcome
///
/// Prints the chosen mode, and a warning if its refresh rate is more than 2% away from
/// `refresh_rate_hz`, and returns its index in the display's mode list. Returns an error if
/// `refresh_rate_hz` is not a positive finite number, the display has no modes, or the FIDL
/// message cannot be sent.
pub fn set_refresh_rate(
    coordinator: &Coordinator,
    display: &DisplayInfo,
    refresh_rate_hz: f64,
) -> Result<usize> {
    if !refresh_rate_hz.is_finite() || refresh_rate_hz <= 0.0 {
        return Err(format_err!(
            "--refresh-rate must be a positive number of Hz, got {}",
            refresh_rate_hz
        ));
    }
    let modes = &display.0.modes;
    let index = closest_refresh_rate_mode(modes, refresh_rate_hz)
        .ok_or_else(|| format_err!("display {} reports no modes", display.id().0))?;
    let mode = &modes[index];
    println!(
        "Display {}: requested {:.3} Hz, selected mode {}: {:.3} Hz @ {}x{}",
        display.id().0,
        refresh_rate_hz,
        index,
        f64::from(mode.refresh_rate_millihertz) / 1000.0,
        mode.active_area.width,
        mode.active_area.height
    );
    if is_far_from_requested_rate(refresh_rate_hz, mode.refresh_rate_millihertz) {
        println!(
            "Warning: display {} has no mode within {}% of {:.3} Hz",
            display.id().0,
            REFRESH_RATE_WARNING_TOLERANCE * 100.0,
            refresh_rate_hz
        );
    }
    coordinator
        .proxy()
        .set_display_mode(&display.id().into(), mode)
        .context("SetDisplayMode FIDL call failed")?;
    Ok(index)
}

/// Returns the display with ID `id`, or the first display if `id` is [`None`].
///
/// # Outcome
///
/// Returns an error if no displays are attached or `id` is not found.
pub fn find_display(coordinator: &Coordinator, id: Option<DisplayId>) -> Result<DisplayInfo> {
    let displays = coordinator.displays();
    match id {
        None => displays.into_iter().next().ok_or_else(|| format_err!("no displays found")),
        Some(id) => displays
            .into_iter()
            .find(|d| d.id() == id)
            .ok_or_else(|| format_err!("display with id '{:?}' not found", id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_math as fidl_math;
    use googletest::{expect_eq, gtest};

    fn mode(width: u32, height: u32, refresh_rate_millihertz: u32) -> fidl_display_types::Mode {
        fidl_display_types::Mode {
            active_area: fidl_math::SizeU { width, height },
            refresh_rate_millihertz,
            flags: fidl_display_types::ModeFlags::empty(),
        }
    }

    fn adaptive_panel_modes() -> Vec<fidl_display_types::Mode> {
        [117_794, 78_529, 58_897, 47_118, 29_449, 23_559, 9_816, 982]
            .into_iter()
            .map(|rate| mode(1080, 2400, rate))
            .collect()
    }

    #[gtest]
    #[fuchsia::test]
    fn no_modes() {
        expect_eq!(closest_refresh_rate_mode(&[], 60.0), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn exact_and_nearest_rates() {
        let modes = adaptive_panel_modes();
        expect_eq!(closest_refresh_rate_mode(&modes, 120.0), Some(0));
        expect_eq!(closest_refresh_rate_mode(&modes, 80.0), Some(1));
        expect_eq!(closest_refresh_rate_mode(&modes, 60.0), Some(2));
        expect_eq!(closest_refresh_rate_mode(&modes, 58.897), Some(2));
        expect_eq!(closest_refresh_rate_mode(&modes, 24.0), Some(5));
        expect_eq!(closest_refresh_rate_mode(&modes, 1.0), Some(7));
        expect_eq!(closest_refresh_rate_mode(&modes, 0.001), Some(7));
        expect_eq!(closest_refresh_rate_mode(&modes, 1000.0), Some(0));
    }

    #[gtest]
    #[fuchsia::test]
    fn far_rates_are_flagged() {
        expect_eq!(is_far_from_requested_rate(60.0, 58_897), false);
        expect_eq!(is_far_from_requested_rate(90.0, 78_529), true);
        expect_eq!(is_far_from_requested_rate(120.0, 117_794), false);
        expect_eq!(is_far_from_requested_rate(15.0, 9_816), true);
    }

    #[gtest]
    #[fuchsia::test]
    fn ties_pick_the_preferred_mode() {
        let modes = vec![mode(100, 100, 60_000), mode(100, 100, 40_000)];
        expect_eq!(closest_refresh_rate_mode(&modes, 50.0), Some(0));
    }

    #[gtest]
    #[fuchsia::test]
    fn modes_with_other_resolutions_are_skipped() {
        let modes = vec![mode(100, 100, 60_000), mode(200, 200, 30_000), mode(100, 100, 24_000)];
        expect_eq!(closest_refresh_rate_mode(&modes, 30.0), Some(2));
    }
}
