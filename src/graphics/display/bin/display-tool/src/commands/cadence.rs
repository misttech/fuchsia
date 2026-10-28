// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Measures a display's vsync cadence while presenting frames on a timer-driven schedule.
//!
//! Frames are paced by a timer that does not depend on vsync, so the tool can present below,
//! at, or above the display's refresh rate, and can stop presenting for a while (bursts and idle
//! gaps). Every vsync event is recorded. The report shows how the vsync interval responds to the
//! presentation schedule, and how long each frame took from submission to its first vsync.

use anyhow::{Context, Result, format_err};
use display_utils::{
    Coordinator, DisplayConfig, DisplayInfo, Image, ImageId, ImageParameters, Layer, LayerConfig,
    LayerId, PixelFormat, VsyncEvent,
};
use euclid::default::{Rect, Size2D};
use euclid::{point2, size2};
use futures::{FutureExt, StreamExt, select};
use std::fmt::Write as _;
use std::num::NonZero;

use super::frame_rate_test::{draw_grid, get_position_in_grid_for_frame};
use crate::draw::{Frame, MappedImage};

/// Number of images the frames rotate through.
///
/// A frame's image is reused once a later frame has been displayed. Three images let the
/// presenter run one frame ahead of the display without waiting.
const SWAPCHAIN_IMAGE_COUNT: usize = 3;

/// Number of rows in the frame counter grid. Must be even.
const GRID_ROWS: u32 = 10;

/// Number of columns in the frame counter grid.
const GRID_COLS: u32 = 6;

/// Period of the `--verbose` reports.
const VERBOSE_REPORT_PERIOD_NS: i64 = 1_000_000_000;

/// Upper bound on `--fps`, a 100 µs frame period.
///
/// No display refreshes this fast, and each frame takes milliseconds to render, so higher rates
/// would only measure the tool.
const MAX_FPS: f64 = 10_000.0;

/// How long to wait for the vsync that reports the warm-up frame.
///
/// Covers a few periods of the slowest common refresh rate (1 Hz), plus a refresh mode switch.
const WARM_UP_TIMEOUT_NS: i64 = 5_000_000_000;

const NANOS_PER_MILLI: f64 = 1_000_000.0;
const NANOS_PER_SECOND: f64 = 1_000_000_000.0;

const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Colors of the moving cell, in BGRA order. Consecutive frames get different colors.
///
/// The palette size (7) and the grid size (60) are coprime, so a (position, color) pair repeats
/// only every 420 frames.
const PALETTE: [[u8; 4]; 7] = [
    [0, 0, 255, 255],     // Red.
    [0, 255, 0, 255],     // Green.
    [255, 0, 0, 255],     // Blue.
    [0, 255, 255, 255],   // Yellow.
    [255, 255, 0, 255],   // Cyan.
    [255, 0, 255, 255],   // Magenta.
    [255, 255, 255, 255], // White.
];

/// Upper edges of the vsync interval histogram buckets, in milliseconds.
///
/// The buckets separate the nominal periods of common refresh rates: 120 Hz (8.3 ms), 80 Hz
/// (12.5 ms), 60 Hz (16.7 ms), 48 Hz (20.8 ms), 30 Hz (33.3 ms), 24 Hz (41.7 ms), 10 Hz (100 ms)
/// and 1 Hz (1000 ms). The last bucket has no upper edge.
const HISTOGRAM_EDGES_MS: [f64; 10] =
    [5.0, 10.0, 14.5, 18.75, 27.0, 37.5, 60.0, 150.0, 500.0, 1500.0];

/// Number of vsync interval histogram buckets.
const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_EDGES_MS.len() + 1;

/// Bursts of frames separated by idle gaps, for a [`Schedule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Burst {
    /// Frames per burst.
    frames: NonZero<u64>,

    /// Time between the last frame of a burst and the first frame of the next one.
    idle_ns: i64,
}

/// When the frames of a `cadence` run are presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// Time between consecutive frames in a burst, or [`None`] to present a single frame.
    frame_period_ns: Option<i64>,

    /// Frames per burst and idle time after each burst, or [`None`] for one endless burst.
    burst: Option<Burst>,
}

impl Schedule {
    /// Creates a validated schedule from the `cadence` command-line arguments.
    ///
    /// `fps == 0` presents one frame. Otherwise frames are `1 / fps` seconds apart. When bursts
    /// are configured, the last frame of a burst and the first frame of the next one are
    /// `idle_ms` apart.
    ///
    /// # Outcome
    ///
    /// Returns an error if `fps` is negative, not finite or above [`MAX_FPS`], if only one of
    /// `burst_frames` and `idle_ms` is given, if `idle_ms` is 0, or if bursts are requested
    /// with `fps == 0`.
    pub fn new(fps: f64, burst_frames: Option<NonZero<u64>>, idle_ms: Option<u64>) -> Result<Self> {
        if !fps.is_finite() || !(0.0..=MAX_FPS).contains(&fps) {
            return Err(format_err!("--fps must be between 0 and {}, got {}", MAX_FPS, fps));
        }
        let burst = match (burst_frames, idle_ms) {
            (None, None) => None,
            (Some(_), Some(0)) => {
                return Err(format_err!("--idle-ms must be above 0"));
            }
            (Some(frames), Some(idle_ms)) => {
                let idle_ns = i64::try_from(idle_ms)
                    .ok()
                    .and_then(|ms| ms.checked_mul(1_000_000))
                    .ok_or_else(|| format_err!("--idle-ms {} is too large", idle_ms))?;
                Some(Burst { frames, idle_ns })
            }
            _ => return Err(format_err!("--burst-frames and --idle-ms must be used together")),
        };
        if fps == 0.0 {
            if burst.is_some() {
                return Err(format_err!("--burst-frames and --idle-ms require --fps above 0"));
            }
            return Ok(Self { frame_period_ns: None, burst: None });
        }
        let frame_period_ns = (NANOS_PER_SECOND / fps).round() as i64;
        Ok(Self { frame_period_ns: Some(frame_period_ns), burst })
    }

    /// Time between consecutive frames in a burst, or [`None`] if a single frame is presented.
    pub fn frame_period_ns(&self) -> Option<i64> {
        self.frame_period_ns
    }

    /// Returns when `frame` (0-based) is due, in nanoseconds since the start of the run.
    ///
    /// Returns [`None`] if the schedule has no such frame.
    pub fn frame_offset_ns(&self, frame: u64) -> Option<i64> {
        let Some(period_ns) = self.frame_period_ns else {
            return (frame == 0).then_some(0);
        };
        let frame = i64::try_from(frame).ok()?;
        match self.burst {
            None => period_ns.checked_mul(frame),
            Some(Burst { frames: burst_frames, idle_ns }) => {
                let burst_frames = i64::try_from(burst_frames.get()).ok()?;
                let burst_span_ns =
                    period_ns.checked_mul(burst_frames - 1)?.checked_add(idle_ns)?;
                let burst = frame / burst_frames;
                let frame_in_burst = frame % burst_frames;
                burst_span_ns.checked_mul(burst)?.checked_add(period_ns * frame_in_burst)
            }
        }
    }

    /// Returns the idle time before `frame` (0-based) if it starts a burst other than the first
    /// one, or [`None`] otherwise.
    pub fn idle_before_ns(&self, frame: u64) -> Option<i64> {
        let Burst { frames: burst_frames, idle_ns } = self.burst?;
        (frame > 0 && frame % burst_frames.get() == 0).then_some(idle_ns)
    }

    /// Describes the schedule for the console.
    fn describe(&self) -> String {
        match (self.frame_period_ns, self.burst) {
            (None, _) => "one frame, then idle".to_string(),
            (Some(period_ns), None) => {
                format!("{:.3} fps (every {:.3} ms)", fps(period_ns), ns_to_ms(period_ns))
            }
            (Some(period_ns), Some(Burst { frames, idle_ns })) => format!(
                "bursts of {} frames at {:.3} fps (every {:.3} ms), {:.3} ms apart",
                frames,
                fps(period_ns),
                ns_to_ms(period_ns),
                ns_to_ms(idle_ns)
            ),
        }
    }
}

fn fps(period_ns: i64) -> f64 {
    NANOS_PER_SECOND / period_ns as f64
}

fn ns_to_ms(ns: i64) -> f64 {
    ns as f64 / NANOS_PER_MILLI
}

/// One vsync event received during the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VsyncRecord {
    timestamp_ns: i64,

    /// Stamp of the newest configuration displayed at this vsync.
    config_stamp: u64,

    /// True iff the stamp is 0 (no stamp) or lower than the stamp of an earlier vsync.
    stale: bool,
}

/// One frame presented during the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameRecord {
    /// Stamp of the configuration that presents the frame.
    config_stamp: u64,

    /// When the frame was due, according to the [`Schedule`].
    scheduled_ns: i64,

    /// When the configuration was committed (after `CommitConfig` was sent).
    submitted_ns: i64,

    /// Timestamp of the first vsync that reported `config_stamp`, if any.
    first_vsync_ns: Option<i64>,
}

/// Minimum, median, 90th percentile and maximum of a set of values.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Distribution {
    min: f64,
    median: f64,
    p90: f64,
    max: f64,
}

impl Distribution {
    /// Summarizes `values`. Returns [`None`] if `values` is empty.
    ///
    /// Percentiles use the nearest-rank method, so they are always one of the values.
    fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let rank = |percentile: f64| {
            let rank = (percentile * sorted.len() as f64).ceil() as usize;
            sorted[rank.clamp(1, sorted.len()) - 1]
        };
        Some(Self {
            min: sorted[0],
            median: rank(0.5),
            p90: rank(0.9),
            max: sorted[sorted.len() - 1],
        })
    }
}

/// Returns the index of the [`HISTOGRAM_EDGES_MS`] bucket that holds `interval_ms`.
fn histogram_bucket(interval_ms: f64) -> usize {
    HISTOGRAM_EDGES_MS
        .iter()
        .position(|&edge| interval_ms < edge)
        .unwrap_or(HISTOGRAM_EDGES_MS.len())
}

/// Returns the label of histogram bucket `bucket`, such as `5-10`.
fn histogram_bucket_label(bucket: usize) -> String {
    let format_edge = |edge: f64| format!("{}", edge);
    let low =
        if bucket == 0 { "0".to_string() } else { format_edge(HISTOGRAM_EDGES_MS[bucket - 1]) };
    let high =
        HISTOGRAM_EDGES_MS.get(bucket).map_or_else(|| "inf".to_string(), |&edge| format_edge(edge));
    format!("{}-{}", low, high)
}

/// Statistics over a span of a `cadence` run.
#[derive(Clone, Debug, PartialEq)]
struct Report {
    /// Number of vsyncs in the span.
    vsync_count: usize,

    /// Number of those vsyncs whose stamp was 0 or went backwards. See [`VsyncRecord::stale`].
    stale_vsync_count: usize,

    /// Vsyncs per second, from the first to the last vsync in the span.
    mean_rate_hz: Option<f64>,

    /// Time between consecutive vsyncs, in milliseconds.
    intervals_ms: Option<Distribution>,

    /// Number of vsync intervals in each [`HISTOGRAM_EDGES_MS`] bucket.
    histogram: [usize; HISTOGRAM_BUCKET_COUNT],

    /// Number of frames presented in the span.
    frames_presented: usize,

    /// Number of those frames that a vsync reported.
    ///
    /// A frame is never reported if a newer configuration replaces it before the display shows
    /// it, or if the run ends first.
    frames_shown: usize,

    /// Number of frames committed more than half a frame period after their scheduled time.
    ///
    /// A frame is rendered as soon as its image is free, and committed when it is due. It is
    /// late if its image was not free in time, because the display had not shown a later frame
    /// yet, or if rendering it did not finish in time.
    frames_late: usize,

    /// Time from submitting a frame to the first vsync that reported it, in milliseconds.
    latencies_ms: Option<Distribution>,
}

impl Report {
    /// Computes the statistics for `vsyncs` and `frames`.
    ///
    /// `previous_vsync_ns` is the vsync right before `vsyncs`, if any, so the interval that
    /// crosses into the span is counted. `frame_period_ns` sets the lateness threshold.
    fn new(
        previous_vsync_ns: Option<i64>,
        vsyncs: &[VsyncRecord],
        frames: &[FrameRecord],
        frame_period_ns: Option<i64>,
    ) -> Self {
        let timestamps: Vec<i64> =
            previous_vsync_ns.into_iter().chain(vsyncs.iter().map(|v| v.timestamp_ns)).collect();
        let intervals: Vec<f64> =
            timestamps.windows(2).map(|pair| ns_to_ms(pair[1] - pair[0])).collect();
        let mut histogram = [0; HISTOGRAM_BUCKET_COUNT];
        for &interval in &intervals {
            histogram[histogram_bucket(interval)] += 1;
        }
        let mean_rate_hz = match (timestamps.first(), timestamps.last()) {
            (Some(first), Some(last)) if last > first => {
                Some(intervals.len() as f64 * NANOS_PER_SECOND / (last - first) as f64)
            }
            _ => None,
        };

        let latencies: Vec<f64> = frames
            .iter()
            .filter_map(|frame| {
                frame.first_vsync_ns.map(|vsync| ns_to_ms(vsync - frame.submitted_ns))
            })
            .collect();
        let frames_late = match frame_period_ns {
            None => 0,
            Some(period_ns) => frames
                .iter()
                .filter(|frame| frame.submitted_ns - frame.scheduled_ns > period_ns / 2)
                .count(),
        };

        Self {
            vsync_count: vsyncs.len(),
            stale_vsync_count: vsyncs.iter().filter(|v| v.stale).count(),
            mean_rate_hz,
            intervals_ms: Distribution::of(&intervals),
            histogram,
            frames_presented: frames.len(),
            frames_shown: latencies.len(),
            frames_late,
            latencies_ms: Distribution::of(&latencies),
        }
    }

    /// Formats the report for people, as indented lines.
    fn human_text(&self) -> String {
        let mut text = String::new();
        let rate =
            self.mean_rate_hz.map_or_else(|| "n/a".to_string(), |hz| format!("{:.3} Hz", hz));
        writeln!(
            text,
            "  vsyncs: {} ({} with a stale stamp), mean rate {}",
            self.vsync_count, self.stale_vsync_count, rate
        )
        .unwrap();
        if let Some(d) = self.intervals_ms {
            writeln!(
                text,
                "  vsync interval: min {:.3} ms, median {:.3} ms, p90 {:.3} ms, max {:.3} ms",
                d.min, d.median, d.p90, d.max
            )
            .unwrap();
            let buckets: Vec<String> = (0..HISTOGRAM_BUCKET_COUNT)
                .filter(|&bucket| self.histogram[bucket] > 0)
                .map(|bucket| {
                    format!("{} ms: {}", histogram_bucket_label(bucket), self.histogram[bucket])
                })
                .collect();
            writeln!(text, "  vsync interval histogram: {}", buckets.join(", ")).unwrap();
        }
        writeln!(
            text,
            "  frames: {} presented, {} shown, {} never reported by a vsync, {} late",
            self.frames_presented,
            self.frames_shown,
            self.frames_presented - self.frames_shown,
            self.frames_late
        )
        .unwrap();
        if let Some(d) = self.latencies_ms {
            writeln!(
                text,
                "  submit to first vsync: min {:.3} ms, median {:.3} ms, p90 {:.3} ms, max {:.3} ms",
                d.min, d.median, d.p90, d.max
            )
            .unwrap();
        }
        text
    }

    /// Formats the report as one line of space-separated `key=value` pairs.
    ///
    /// Missing values (for example, latencies when no frame was shown) are `nan`.
    fn key_values(&self) -> String {
        let float =
            |value: Option<f64>| value.map_or_else(|| "nan".to_string(), |v| format!("{:.3}", v));
        let intervals = self.intervals_ms;
        let latencies = self.latencies_ms;
        let histogram: Vec<String> = (0..HISTOGRAM_BUCKET_COUNT)
            .map(|bucket| format!("{}:{}", histogram_bucket_label(bucket), self.histogram[bucket]))
            .collect();
        format!(
            "vsyncs={} vsyncs_stale={} mean_hz={} interval_min_ms={} interval_median_ms={} \
             interval_p90_ms={} interval_max_ms={} interval_hist_ms={} frames_presented={} \
             frames_shown={} frames_late={} latency_min_ms={} latency_median_ms={} \
             latency_p90_ms={} latency_max_ms={}",
            self.vsync_count,
            self.stale_vsync_count,
            float(self.mean_rate_hz),
            float(intervals.map(|d| d.min)),
            float(intervals.map(|d| d.median)),
            float(intervals.map(|d| d.p90)),
            float(intervals.map(|d| d.max)),
            histogram.join(","),
            self.frames_presented,
            self.frames_shown,
            self.frames_late,
            float(latencies.map(|d| d.min)),
            float(latencies.map(|d| d.median)),
            float(latencies.map(|d| d.p90)),
            float(latencies.map(|d| d.max)),
        )
    }
}

/// Draws a visible frame counter: a cell that moves one grid position per frame, in a color
/// that changes every frame, with a color bar below the grid in the same color.
struct CadenceScene {
    grid: Rect<u32>,
    screen_size: Size2D<u32>,
}

impl CadenceScene {
    fn new(screen_size: Size2D<u32>) -> Self {
        let min_length = u32::min(screen_size.width, screen_size.height);
        let grid_length = (min_length as f32 * std::f32::consts::FRAC_1_SQRT_2) as u32;
        let grid_size = size2(grid_length, grid_length);
        let grid_origin = point2(0, 0) + (screen_size - grid_size) / 2;
        CadenceScene { grid: Rect::new(grid_origin, grid_size), screen_size }
    }

    /// Renders frame number `frame` into `image`, whose pixels outside the grid and the color
    /// bar must be black.
    fn render(&self, frame: u64, image: &mut MappedImage) -> Result<()> {
        let grid = self.grid;
        image
            .fill_region(
                &BLACK,
                &Frame {
                    pos_x: grid.min_x(),
                    pos_y: grid.min_y(),
                    width: grid.width(),
                    height: grid.height(),
                },
            )
            .context("failed to clear the grid pixels")?;
        draw_grid(grid, GRID_ROWS, GRID_COLS, &WHITE, image).context("failed to draw grid")?;

        let color = &PALETTE[(frame % PALETTE.len() as u64) as usize];
        let cell_size: Size2D<u32> = size2(grid.width() / GRID_COLS, grid.height() / GRID_ROWS);
        let position = get_position_in_grid_for_frame(frame, GRID_ROWS, GRID_COLS);
        image
            .fill_region(
                color,
                &Frame {
                    pos_x: grid.min_x() + position.x * cell_size.width,
                    pos_y: grid.min_y() + position.y * cell_size.height,
                    width: cell_size.width,
                    height: cell_size.height,
                },
            )
            .context("failed to draw the moving cell")?;

        // The color bar shows the frame's color across the grid's width, so a camera can tell
        // consecutive frames apart wherever the panel's scan is when the picture is taken.
        let bar = Frame {
            pos_x: grid.min_x(),
            pos_y: grid.max_y() + cell_size.height / 2,
            width: grid.width(),
            height: cell_size.height,
        };
        // The grid lines reach one pixel past the grid on every side.
        let mut drawn = vec![Frame {
            pos_x: grid.min_x().saturating_sub(1),
            pos_y: grid.min_y().saturating_sub(1),
            width: grid.width() + 2,
            height: grid.height() + 2,
        }];
        if bar.pos_y + bar.height <= self.screen_size.height {
            image.fill_region(color, &bar).context("failed to draw the color bar")?;
            drawn.push(bar);
        }

        // Only the pixels drawn above change between frames, so only they need cleaning.
        image.surface().cache_clean_regions(&drawn)?;
        Ok(())
    }
}

/// Arguments for [`run`].
pub struct Args<'a> {
    pub display: &'a DisplayInfo,
    pub schedule: Schedule,
    /// Length of the run, in nanoseconds.
    pub duration_ns: i64,
    /// Print a report every [`VERBOSE_REPORT_PERIOD_NS`].
    pub verbose: bool,
    /// Print a line for every vsync event.
    pub log_vsync: bool,
}

/// Presents frames on `args.schedule` and reports the vsync cadence.
///
/// # Outcome
///
/// Presents a black warm-up frame and waits for the display to show it, then runs for
/// `args.duration_ns`, then prints the final report. Returns an error if images cannot be
/// allocated, a configuration is rejected, or vsync events stop arriving.
//
// Subtleties:
// Each frame is rendered as soon as its image is free, and committed when it is due, so the
// rendering time does not count towards lateness or latency unless the image frees up late.
//
// When frames fall behind the schedule, the overdue ones are committed back to back. To keep
// the idle gap between bursts, a burst that would start less than the idle time after the last
// frame of the previous burst was committed starts that long after it instead, and the rest of
// the schedule moves with it.
pub async fn run(coordinator: &Coordinator, args: Args<'_>) -> Result<()> {
    let Args { display, schedule, duration_ns, verbose, log_vsync } = args;

    // The tool's images use the preferred mode's resolution. `--refresh-rate` only picks modes
    // with this resolution.
    let (width, height) = {
        let mode = &display.0.modes[0];
        (mode.active_area.width, mode.active_area.height)
    };
    let params = ImageParameters {
        width,
        height,
        pixel_format: PixelFormat::Bgra32,
        color_space: fidl_fuchsia_images2::ColorSpace::Srgb,
        name: Some("display-tool cadence".to_string()),
    };
    // The swapchain, followed by the black image of the warm-up frame.
    let mut images = Vec::with_capacity(SWAPCHAIN_IMAGE_COUNT + 1);
    for index in 0..SWAPCHAIN_IMAGE_COUNT + 1 {
        let image = MappedImage::create(
            Image::create(coordinator.clone(), ImageId(index as u64 + 1), &params).await?,
        )?;
        image.zero().context("failed to clear background")?;
        image.cache_clean()?;
        images.push(image);
    }
    let layer_id = coordinator.create_layer().await?;
    let scene = CadenceScene::new(size2(width, height));

    // Listen before the first commit, so the first frame's vsync is not missed.
    let mut vsync_listener = coordinator.add_vsync_listener(Some(display.id()))?;

    // Why: The first configuration carries the mode that `--refresh-rate` selected, and the
    // display may switch modes and restart its pipeline before showing it. The warm-up frame
    // keeps that out of the measurement.
    warm_up(
        coordinator,
        &build_display_configs(display, layer_id, &params, &images[SWAPCHAIN_IMAGE_COUNT]),
        &mut vsync_listener,
    )
    .await?;

    println!(
        "Display {}: presenting {} for {:.3} s",
        display.id().0,
        schedule.describe(),
        duration_ns as f64 / NANOS_PER_SECOND
    );

    // Why: Frame 0 is due as soon as the clock starts, so it is rendered before.
    scene.render(0, &mut images[0])?;

    let start_ns = zx::MonotonicInstant::get().into_nanos();
    let end_ns = start_ns + duration_ns;
    let mut frames: Vec<FrameRecord> = Vec::new();
    let mut vsyncs: Vec<VsyncRecord> = Vec::new();
    let mut displayed_stamp: Option<u64> = None;

    // The frame rendered into its image and waiting to be committed, if any.
    let mut rendered_frame: Option<usize> = Some(0);

    // How far bursts that started late moved the schedule.
    let mut schedule_delay_ns: i64 = 0;

    // Start of the current `--verbose` report span.
    let mut window_start_ns = start_ns;
    let mut window_first_frame: usize = 0;
    let mut window_first_vsync: usize = 0;

    loop {
        let now_ns = zx::MonotonicInstant::get().into_nanos();
        if now_ns >= end_ns {
            break;
        }

        if verbose && now_ns >= window_start_ns + VERBOSE_REPORT_PERIOD_NS {
            let previous_vsync_ns =
                window_first_vsync.checked_sub(1).map(|i| vsyncs[i].timestamp_ns);
            let report = Report::new(
                previous_vsync_ns,
                &vsyncs[window_first_vsync..],
                &frames[window_first_frame..],
                schedule.frame_period_ns(),
            );
            let t_s = (now_ns - start_ns) as f64 / NANOS_PER_SECOND;
            println!(
                "[t={:.3}s] last {:.3} s:",
                t_s,
                (now_ns - window_start_ns) as f64 / NANOS_PER_SECOND
            );
            print!("{}", report.human_text());
            println!("cadence_window t_s={:.3} {}", t_s, report.key_values());
            window_start_ns = now_ns;
            window_first_frame = frames.len();
            window_first_vsync = vsyncs.len();
        }

        // Frame `next_frame` reuses the image of frame `next_frame - SWAPCHAIN_IMAGE_COUNT`. That
        // image is free once any later frame has been displayed.
        let next_frame = frames.len();
        let image_free = next_frame < SWAPCHAIN_IMAGE_COUNT
            || displayed_stamp
                .is_some_and(|s| s >= frames[next_frame + 1 - SWAPCHAIN_IMAGE_COUNT].config_stamp);
        let due_ns = schedule
            .frame_offset_ns(next_frame as u64)
            .map(|offset| {
                let due_ns = start_ns + offset + schedule_delay_ns;
                let anchor_ns = schedule
                    .idle_before_ns(next_frame as u64)
                    .zip(frames.last())
                    .map(|(idle_ns, previous)| previous.submitted_ns + idle_ns);
                match anchor_ns {
                    Some(anchor_ns) if anchor_ns > due_ns => {
                        schedule_delay_ns += anchor_ns - due_ns;
                        anchor_ns
                    }
                    _ => due_ns,
                }
            })
            .filter(|&due| due < end_ns);

        if due_ns.is_some() && image_free && rendered_frame != Some(next_frame) {
            scene.render(next_frame as u64, &mut images[next_frame % SWAPCHAIN_IMAGE_COUNT])?;
            rendered_frame = Some(next_frame);
            continue;
        }

        let present_at_ns = due_ns.filter(|_| rendered_frame == Some(next_frame));
        if let Some(present_at_ns) = present_at_ns.filter(|&at| now_ns >= at) {
            let config_stamp = coordinator
                .commit_config(&build_display_configs(
                    display,
                    layer_id,
                    &params,
                    &images[next_frame % SWAPCHAIN_IMAGE_COUNT],
                ))
                .await?;
            frames.push(FrameRecord {
                config_stamp,
                scheduled_ns: present_at_ns,
                submitted_ns: zx::MonotonicInstant::get().into_nanos(),
                first_vsync_ns: None,
            });
            continue;
        }

        let mut wake_ns = end_ns;
        if let Some(present_at_ns) = present_at_ns {
            wake_ns = wake_ns.min(present_at_ns);
        }
        if verbose {
            wake_ns = wake_ns.min(window_start_ns + VERBOSE_REPORT_PERIOD_NS);
        }
        select! {
            event = vsync_listener.next() => {
                let event = event.ok_or_else(|| format_err!("stopped receiving vsync events"))?;
                let recorder = VsyncRecorder { start_ns, end_ns, log_vsync };
                recorder.record(event, &mut frames, &mut vsyncs, &mut displayed_stamp);
            }
            () = fuchsia_async::Timer::new(zx::MonotonicInstant::from_nanos(wake_ns)).fuse() => {}
        }
    }

    // Why: A vsync event is delivered after its timestamp, so the events of the last vsyncs of
    // the run may still be queued. `VsyncRecorder` drops the ones stamped after the run ended.
    let recorder = VsyncRecorder { start_ns, end_ns, log_vsync };
    while let Some(Some(event)) = vsync_listener.next().now_or_never() {
        recorder.record(event, &mut frames, &mut vsyncs, &mut displayed_stamp);
    }

    let report = Report::new(None, &vsyncs, &frames, schedule.frame_period_ns());
    println!("Cadence over {:.3} s:", duration_ns as f64 / NANOS_PER_SECOND);
    print!("{}", report.human_text());
    println!("cadence_summary {}", report.key_values());
    Ok(())
}

/// Commits `configs`, a black warm-up frame, and waits until the display shows it.
///
/// # Outcome
///
/// Returns once a vsync reports the warm-up frame, and the vsync after it has arrived, so that
/// the first interval the run measures is in the selected mode. Returns an error if the
/// configuration is rejected, no vsync reports it within [`WARM_UP_TIMEOUT_NS`], or vsync events
/// stop arriving.
async fn warm_up(
    coordinator: &Coordinator,
    configs: &[DisplayConfig],
    vsync_listener: &mut futures::channel::mpsc::UnboundedReceiver<VsyncEvent>,
) -> Result<()> {
    let stamp = coordinator.commit_config(configs).await?;
    let deadline = fuchsia_async::Timer::new(zx::MonotonicInstant::from_nanos(
        zx::MonotonicInstant::get().into_nanos() + WARM_UP_TIMEOUT_NS,
    ))
    .fuse();
    futures::pin_mut!(deadline);
    let mut shown = false;
    loop {
        select! {
            event = vsync_listener.next() => {
                let VsyncEvent { config, .. } =
                    event.ok_or_else(|| format_err!("stopped receiving vsync events"))?;
                if shown {
                    return Ok(());
                }
                shown = config.value >= stamp;
            }
            () = deadline => {
                return Err(format_err!(
                    "no vsync reported the warm-up frame within {:.3} s",
                    WARM_UP_TIMEOUT_NS as f64 / NANOS_PER_SECOND
                ));
            }
        }
    }
}

/// Records the vsync events of a `cadence` run.
struct VsyncRecorder {
    /// Start of the run.
    start_ns: i64,
    /// End of the run. Vsyncs at or after it are dropped.
    end_ns: i64,
    /// Print a line for every recorded vsync.
    log_vsync: bool,
}

impl VsyncRecorder {
    /// Records `event` in `vsyncs`, and as the first vsync of the frame in `frames` that it
    /// reports, if it is that frame's first.
    ///
    /// # Outcome
    ///
    /// Drops `event` if it is stamped at or after the end of the run. Otherwise, raises
    /// `displayed_stamp` to the event's config stamp. A stamp of 0, or one lower than
    /// `displayed_stamp`, marks the vsync stale and leaves `displayed_stamp` alone.
    fn record(
        &self,
        event: VsyncEvent,
        frames: &mut [FrameRecord],
        vsyncs: &mut Vec<VsyncRecord>,
        displayed_stamp: &mut Option<u64>,
    ) {
        let VsyncEvent { timestamp, config, .. } = event;
        let timestamp_ns = timestamp.into_nanos();
        if timestamp_ns >= self.end_ns {
            return;
        }
        let config_stamp = config.value;
        let stale = config_stamp == 0 || displayed_stamp.is_some_and(|s| config_stamp < s);
        *displayed_stamp = (*displayed_stamp).max(Some(config_stamp));
        let record = VsyncRecord { timestamp_ns, config_stamp, stale };

        let frame_index = frames.binary_search_by_key(&config_stamp, |f| f.config_stamp).ok();
        let mut first_for_frame = false;
        if let Some(frame) =
            frame_index.map(|i| &mut frames[i]).filter(|f| f.first_vsync_ns.is_none())
        {
            frame.first_vsync_ns = Some(timestamp_ns);
            first_for_frame = true;
        }
        if self.log_vsync {
            let interval = vsyncs.last().map_or_else(
                || "nan".to_string(),
                |previous: &VsyncRecord| {
                    format!("{:.3}", ns_to_ms(timestamp_ns - previous.timestamp_ns))
                },
            );
            let frame = frame_index.map_or_else(|| "none".to_string(), |i| i.to_string());
            println!(
                "vsync t_ms={:.3} interval_ms={} stamp={} stale={} frame={} first={}",
                ns_to_ms(timestamp_ns - self.start_ns),
                interval,
                config_stamp,
                stale,
                frame,
                first_for_frame
            );
        }
        vsyncs.push(record);
    }
}

fn build_display_configs(
    display: &DisplayInfo,
    layer_id: LayerId,
    params: &ImageParameters,
    image: &MappedImage,
) -> Vec<DisplayConfig> {
    vec![DisplayConfig {
        id: display.id(),
        layers: vec![Layer {
            id: layer_id,
            config: LayerConfig::Primary {
                image_id: image.id(),
                image_metadata: params.into(),
                unblock_event: None,
                alpha: None,
            },
        }],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_that, gtest, matchers};

    const MS: i64 = 1_000_000;

    fn vsync(timestamp_ms: i64, config_stamp: u64) -> VsyncRecord {
        VsyncRecord { timestamp_ns: timestamp_ms * MS, config_stamp, stale: false }
    }

    fn frame(scheduled_ms: i64, submitted_ms: i64, first_vsync_ms: Option<i64>) -> FrameRecord {
        FrameRecord {
            config_stamp: 0,
            scheduled_ns: scheduled_ms * MS,
            submitted_ns: submitted_ms * MS,
            first_vsync_ns: first_vsync_ms.map(|ms| ms * MS),
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn schedule_rejects_bad_arguments() {
        expect_that!(Schedule::new(-1.0, None, None), matchers::err(matchers::anything()));
        expect_that!(Schedule::new(f64::NAN, None, None), matchers::err(matchers::anything()));
        expect_that!(Schedule::new(f64::INFINITY, None, None), matchers::err(matchers::anything()));
        expect_that!(
            Schedule::new(60.0, NonZero::new(10), None),
            matchers::err(matchers::anything())
        );
        expect_that!(Schedule::new(60.0, None, Some(100)), matchers::err(matchers::anything()));
        expect_that!(
            Schedule::new(0.0, NonZero::new(10), Some(100)),
            matchers::err(matchers::anything())
        );
        expect_that!(
            Schedule::new(60.0, NonZero::new(10), Some(0)),
            matchers::err(matchers::anything())
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn schedule_with_zero_fps_presents_one_frame() {
        let schedule = Schedule::new(0.0, None, None).unwrap();
        expect_eq!(schedule.frame_period_ns(), None);
        expect_eq!(schedule.frame_offset_ns(0), Some(0));
        expect_eq!(schedule.frame_offset_ns(1), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn schedule_paces_frames_evenly() {
        let schedule = Schedule::new(50.0, None, None).unwrap();
        expect_eq!(schedule.frame_period_ns(), Some(20 * MS));
        expect_eq!(schedule.frame_offset_ns(0), Some(0));
        expect_eq!(schedule.frame_offset_ns(1), Some(20 * MS));
        expect_eq!(schedule.frame_offset_ns(100), Some(2000 * MS));
    }

    #[gtest]
    #[fuchsia::test]
    fn schedule_separates_bursts_by_idle_time() {
        let schedule = Schedule::new(100.0, NonZero::new(3), Some(500)).unwrap();
        let offsets_ms: Vec<i64> =
            (0..7).map(|frame| schedule.frame_offset_ns(frame).unwrap() / MS).collect();
        expect_eq!(offsets_ms, vec![0, 10, 20, 520, 530, 540, 1040]);
        let idle_ms: Vec<Option<i64>> =
            (0..7).map(|frame| schedule.idle_before_ns(frame).map(|ns| ns / MS)).collect();
        expect_eq!(idle_ms, vec![None, None, None, Some(500), None, None, Some(500)]);
        expect_eq!(Schedule::new(100.0, None, None).unwrap().idle_before_ns(3), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn distribution_uses_nearest_rank() {
        expect_eq!(Distribution::of(&[]), None);
        expect_eq!(
            Distribution::of(&[7.0]),
            Some(Distribution { min: 7.0, median: 7.0, p90: 7.0, max: 7.0 })
        );
        let values: Vec<f64> = (1..=10).rev().map(f64::from).collect();
        expect_eq!(
            Distribution::of(&values),
            Some(Distribution { min: 1.0, median: 5.0, p90: 9.0, max: 10.0 })
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn histogram_buckets_separate_common_refresh_rates() {
        let buckets: Vec<usize> = [
            1000.0 / 120.0,
            12.5,
            1000.0 / 60.0,
            1000.0 / 48.0,
            1000.0 / 30.0,
            1000.0 / 24.0,
            100.0,
            1000.0,
        ]
        .into_iter()
        .map(histogram_bucket)
        .collect();
        expect_eq!(buckets, vec![1, 2, 3, 4, 5, 6, 7, 9]);
        expect_eq!(histogram_bucket(0.0), 0);
        expect_eq!(histogram_bucket(5.0), 1);
        expect_eq!(histogram_bucket(2000.0), 10);
        expect_eq!(histogram_bucket_label(0), "0-5");
        expect_eq!(histogram_bucket_label(3), "14.5-18.75");
        expect_eq!(histogram_bucket_label(10), "1500-inf");
    }

    #[gtest]
    #[fuchsia::test]
    fn report_without_events() {
        let report = Report::new(None, &[], &[], Some(16 * MS));
        expect_eq!(report.vsync_count, 0);
        expect_eq!(report.mean_rate_hz, None);
        expect_eq!(report.intervals_ms, None);
        expect_eq!(report.latencies_ms, None);
        expect_eq!(
            report.key_values(),
            "vsyncs=0 vsyncs_stale=0 mean_hz=nan interval_min_ms=nan interval_median_ms=nan \
             interval_p90_ms=nan interval_max_ms=nan interval_hist_ms=0-5:0,5-10:0,10-14.5:0,\
             14.5-18.75:0,18.75-27:0,27-37.5:0,37.5-60:0,60-150:0,150-500:0,500-1500:0,\
             1500-inf:0 frames_presented=0 frames_shown=0 frames_late=0 latency_min_ms=nan \
             latency_median_ms=nan latency_p90_ms=nan latency_max_ms=nan"
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn report_measures_intervals_and_latency() {
        let vsyncs = [
            vsync(10, 1),
            vsync(20, 1),
            VsyncRecord { stale: true, ..vsync(30, 2) },
            vsync(1030, 2),
        ];
        let frames = [frame(0, 5, Some(10)), frame(20, 22, Some(30)), frame(40, 60, None)];
        let report = Report::new(Some(0), &vsyncs, &frames, Some(20 * MS));

        expect_eq!(report.vsync_count, 4);
        expect_eq!(report.stale_vsync_count, 1);
        expect_that!(report.mean_rate_hz.unwrap(), matchers::approx_eq(4.0 / 1.03));
        expect_eq!(
            report.intervals_ms,
            Some(Distribution { min: 10.0, median: 10.0, p90: 1000.0, max: 1000.0 })
        );
        expect_eq!(report.histogram[2], 3);
        expect_eq!(report.histogram[9], 1);
        expect_eq!(report.frames_presented, 3);
        expect_eq!(report.frames_shown, 2);
        expect_eq!(report.frames_late, 1);
        expect_eq!(
            report.latencies_ms,
            Some(Distribution { min: 5.0, median: 5.0, p90: 8.0, max: 8.0 })
        );
    }
}
