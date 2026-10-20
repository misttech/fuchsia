// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Demonstrates building an animation using a double-buffer swapchain.

use anyhow::{Context, Result};
use display_utils::{Coordinator, DisplayInfo, ImageId, PixelFormat};
use euclid::default::Vector2D;
use euclid::vec2;
use std::cmp::min;
use std::collections::HashMap;
use std::time::Duration;

use crate::draw::{Frame, MappedImage, Surface};
use crate::runner::{DoubleBufferedFenceLoop, PowerCycle, Scene};

struct BouncingSquare {
    color: [u8; 4],
    frame: Frame,
    /// Pixels moved per frame.
    velocity: Vector2D<i64>,
}

impl BouncingSquare {
    // Update the position along the velocity vector. The velocities are updated such that the
    // square bounces off the boundaries of an enclosing screen of the given dimensions.
    //
    // The velocity is treated as a fixed increment in pixels and the calculation intentionally
    // does not factor in elapsed time or interpolate between steps to make the speed of the boxes
    // vary with the framerate.
    fn update(&mut self, screen_width: u32, screen_height: u32) {
        let x = self.frame.pos_x as i64 + self.velocity.x;
        let y = self.frame.pos_y as i64 + self.velocity.y;
        if x < 0 || x as u32 + self.frame.width > screen_width {
            self.velocity.x *= -1;
        }
        if y < 0 || y as u32 + self.frame.height > screen_height {
            self.velocity.y *= -1;
        }
        self.frame.pos_x = min(x.abs() as u32, screen_width - self.frame.width - 1);
        self.frame.pos_y = min(y.abs() as u32, screen_height - self.frame.height - 1);
    }
}

struct BouncingSquaresScene {
    width: u32,
    height: u32,

    squares: Vec<BouncingSquare>,

    /// The square frames that each image holds, by image ID, from the last time it was rendered.
    /// Images that were never rendered have no entry.
    drawn: HashMap<ImageId, Vec<Frame>>,
}

/// The color of the pixels outside all squares. Equals the result of [`Surface::zero`].
const BACKGROUND: [u8; 4] = [0, 0, 0, 0];

impl BouncingSquaresScene {
    pub fn new(width: u32, height: u32) -> Self {
        // Construct squares that start out at the 4 corners of the screen.
        let dim = height / 8;
        let squares = vec![
            BouncingSquare {
                color: [255, 100, 0, 255],
                frame: Frame { pos_x: 0, pos_y: 0, width: dim, height: dim },
                velocity: vec2(16, 16),
            },
            BouncingSquare {
                color: [255, 0, 255, 255],
                frame: Frame { pos_x: width - dim - 1, pos_y: 0, width: dim, height: dim },
                velocity: vec2(-8, 8),
            },
            BouncingSquare {
                color: [100, 255, 0, 255],
                frame: Frame { pos_x: 0, pos_y: height - dim - 1, width: dim, height: dim },
                velocity: vec2(4, -8),
            },
            BouncingSquare {
                color: [0, 100, 255, 255],
                frame: Frame {
                    pos_x: width - dim - 1,
                    pos_y: height - dim - 1,
                    width: dim,
                    height: dim,
                },
                velocity: vec2(-16, -8),
            },
        ];
        BouncingSquaresScene { width, height, squares, drawn: HashMap::new() }
    }

    /// Renders the scene into `surface`, the pixels of the image with ID `image_id`, and cleans
    /// the caches for every byte written.
    fn render_surface(&mut self, image_id: ImageId, surface: &Surface) -> Result<()> {
        let frames: Vec<Frame> = self.squares.iter().map(|s| s.frame).collect();
        // Forget the image's content until it is fully rendered, so a failed render is followed
        // by a full redraw.
        match self.drawn.remove(&image_id) {
            Some(old_frames) if old_frames.len() == frames.len() => {
                // The image holds the background with the squares at `old_frames`. Only pixels in
                // an old or a new square change. A square moves little between two renders of
                // the same image, so one rectangle around its old and new frame covers both.
                let dirty_frames: Vec<Frame> = old_frames
                    .iter()
                    .zip(&frames)
                    .map(|(old, new)| surface.clip(old).bounding_box(&surface.clip(new)))
                    .collect();
                // Redraw each dirty rectangle in full: background, then every square in order,
                // clipped to it. Later squares paint over earlier ones, as in a full redraw.
                for dirty_frame in &dirty_frames {
                    surface
                        .fill_region(&BACKGROUND, dirty_frame)
                        .context("failed to clear background")?;
                    for s in &self.squares {
                        if let Some(part) = surface.clip(&s.frame).intersection(dirty_frame) {
                            surface
                                .fill_region(&s.color, &part)
                                .context("failed to draw bouncing square")?;
                        }
                    }
                }
                surface.cache_clean_regions(&dirty_frames)?;
            }
            _ => {
                surface.zero().context("failed to clear background")?;
                for s in &self.squares {
                    surface
                        .fill_region(&s.color, &s.frame)
                        .context("failed to draw bouncing square")?;
                }
                surface.cache_clean()?;
            }
        }
        self.drawn.insert(image_id, frames);
        Ok(())
    }
}

impl Scene for BouncingSquaresScene {
    fn update(&mut self) -> Result<()> {
        for s in &mut self.squares {
            s.update(self.width, self.height);
        }
        Ok(())
    }

    fn init_image(&self, _image: &mut MappedImage) -> Result<()> {
        // No need to initialize the image since its first render redraws it in full.
        Ok(())
    }

    fn render(&mut self, image: &mut MappedImage) -> Result<()> {
        self.render_surface(image.id(), image.surface())
    }
}

/// Runs the bouncing squares animation loop on `display`.
///
/// # Outcome
///
/// Allocates swapchain buffers and runs the animation until interrupted or an error occurs.
pub async fn run(
    coordinator: &Coordinator,
    display: &DisplayInfo,
    power_cycle: PowerCycle,
    vsync_timeout: Duration,
) -> Result<()> {
    // Obtain the display resolution based on the display's preferred mode.
    let (width, height) = {
        let mode = &display.0.modes[0];
        (mode.active_area.width, mode.active_area.height)
    };

    let scene = BouncingSquaresScene::new(width, height);
    let mut double_buffered_fence_loop = DoubleBufferedFenceLoop::new(
        coordinator,
        display.id(),
        width,
        height,
        PixelFormat::Bgra32,
        scene,
    )
    .await?;
    double_buffered_fence_loop.set_power_cycle(power_cycle);
    double_buffered_fence_loop.set_vsync_timeout(vsync_timeout);

    double_buffered_fence_loop.run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{ByteCounts, testing};
    use googletest::{expect_eq, expect_true, gtest};
    use std::collections::HashSet;

    /// Renders `frame_count` frames of a `width` x `height` scene into 2 alternating test
    /// surfaces, as the runner does, and checks each rendered image against a full redraw.
    fn check_incremental_rendering(width: u32, height: u32, frame_count: usize) {
        let mut scene = BouncingSquaresScene::new(width, height);
        let full_frame = Frame { pos_x: 0, pos_y: 0, width, height };
        let surfaces = [testing::new_surface(width, height), testing::new_surface(width, height)];
        // Start from non-zero pixels, so the first render of each image must not rely on the
        // image's initial contents.
        for surface in &surfaces {
            surface.fill_region(&[9, 9, 9, 9], &full_frame).expect("fill_region");
            let _ = surface.take_log();
            let _ = surface.take_byte_counts();
        }
        let reference = testing::new_surface(width, height);
        let mut bounces = HashSet::new();
        let mut overlaps = 0;

        for frame_index in 0..frame_count {
            let velocities: Vec<Vector2D<i64>> = scene.squares.iter().map(|s| s.velocity).collect();
            scene.update().expect("update");
            for (old, square) in velocities.iter().zip(&scene.squares) {
                let new = square.velocity;
                if old.x > 0 && new.x < 0 {
                    bounces.insert("right");
                }
                if old.x < 0 && new.x > 0 {
                    bounces.insert("left");
                }
                if old.y > 0 && new.y < 0 {
                    bounces.insert("bottom");
                }
                if old.y < 0 && new.y > 0 {
                    bounces.insert("top");
                }
            }
            for (index, square) in scene.squares.iter().enumerate() {
                for other in &scene.squares[index + 1..] {
                    if square.frame.intersection(&other.frame).is_some() {
                        overlaps += 1;
                    }
                }
            }

            let image_index = frame_index % 2;
            let surface = &surfaces[image_index];
            scene.render_surface(ImageId(1 + image_index as u64), surface).expect("render");

            // Every byte written must be cleaned before the image is presented.
            expect_eq!(
                testing::first_uncleaned_byte(&surface.take_log()),
                None,
                "frame {}",
                frame_index
            );

            reference.zero().expect("zero");
            for s in &scene.squares {
                reference.fill_region(&s.color, &s.frame).expect("fill_region");
            }
            let first_difference = surface
                .bytes()
                .iter()
                .zip(reference.bytes().iter())
                .position(|(actual, expected)| actual != expected);
            expect_eq!(first_difference, None, "frame {}", frame_index);

            // After the first render of each image, only the parts that changed are drawn, and
            // only written row segments are cleaned (each byte at most once).
            let ByteCounts { written, cleaned } = surface.take_byte_counts();
            if frame_index >= 2 {
                expect_true!(
                    written < u64::from(width) * u64::from(height) * 4,
                    "frame {} written {}",
                    frame_index,
                    written
                );
                expect_true!(
                    cleaned <= written,
                    "frame {} written {} cleaned {}",
                    frame_index,
                    written,
                    cleaned
                );
            }
        }
        expect_eq!(bounces.len(), 4, "bounces: {:?}", bounces);
        expect_true!(overlaps > 0, "the squares never overlapped");
    }

    #[gtest]
    #[fuchsia::test]
    fn incremental_rendering_matches_full_redraw_portrait() {
        check_incremental_rendering(200, 320, 300);
    }

    #[gtest]
    #[fuchsia::test]
    fn incremental_rendering_matches_full_redraw_landscape() {
        check_incremental_rendering(320, 200, 300);
    }
}
