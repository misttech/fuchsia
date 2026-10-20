// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

///! Demonstrates building an animation using a double-buffer swapchain with multiple layers.
use {
    anyhow::{Context, Result, format_err},
    display_utils::{Alpha, Coordinator, DisplayInfo, ImageId, PixelFormat},
    euclid::default::{Point2D, Vector2D},
    euclid::{point2, vec2},
    fidl_fuchsia_hardware_display_types as fdisplay_types,
    std::cmp::min,
    std::collections::HashMap,
};

use crate::draw::{Frame, MappedImage, Surface};
use crate::runner_multilayer::{MultiLayerFenceLoop, MultiLayerScene};

const MIN_LAYER_COUNT: usize = 2;

struct BouncingSquare {
    color: [u8; 4],
    frame: Frame,
    /// Pixels moved per frame.
    velocity: Vector2D<i64>,
}

impl BouncingSquare {
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

// TODO(https://fxbug.dev/568873242): Create a type that represents BGRA colors,
// instead of relying on [u8; 4]. premultiply becomes a method that returns a new
// instance.

/// Converts a BGRA color from straight alpha to premultiplied alpha.
///
/// Rounds each scaled color channel to the nearest integer.
fn premultiply([b, g, r, alpha]: [u8; 4]) -> [u8; 4] {
    // The quotient is at most 255, so the cast does not truncate.
    let scale = |channel: u8| ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8;
    [scale(b), scale(g), scale(r), alpha]
}

struct MultiLayerSquaresScene {
    width: u32,
    height: u32,
    /// One square per layer. `squares[i]` is drawn on layer `i`, and layer 0
    /// is the bottom layer.
    squares: Vec<BouncingSquare>,
    /// The square frame that each image holds, by image ID, from the last
    /// time it was rendered. Images that were never rendered have no entry.
    drawn: HashMap<ImageId, Frame>,
}

impl MultiLayerSquaresScene {
    /// `layer_count` must be between [`MIN_LAYER_COUNT`] and 8.
    pub fn new(width: u32, height: u32, layer_count: usize) -> Self {
        assert!(layer_count >= MIN_LAYER_COUNT);
        let small = min(width, height) / 8;
        let large = small * 4;
        let square = |size: u32, color, top_left: Point2D<u32>, velocity| BouncingSquare {
            color: premultiply(color),
            frame: Frame { pos_x: top_left.x, pos_y: top_left.y, width: size, height: size },
            velocity,
        };
        let center = |size: u32| point2((width - size) / 2, (height - size) / 2);

        // One square for each of the 8 layers that a display engine may
        // support (MAX_ALLOWED_MAX_LAYER_COUNT in fuchsia.hardware.display.engine).
        // Squares alternate between large opaque ones and small
        // semi-transparent (alpha 150) ones, starting with the bottom layer,
        // so that every scene covers alpha blending. Colors are in BGRA byte
        // order, with straight alpha.
        let mut squares = vec![
            // Fuchsia (#ff00ff).
            square(large, [255, 0, 255, 255], point2(width - large - 1, 0), vec2(-8, 8)),
            // Semi-transparent green (#00ff64).
            square(small, [100, 255, 0, 150], point2(0, height - small - 1), vec2(4, -8)),
            // Blue (#0064ff).
            square(large, [255, 100, 0, 255], point2(0, 0), vec2(16, 16)),
            // Semi-transparent orange (#ff6400).
            square(
                small,
                [0, 100, 255, 150],
                point2(width - small - 1, height - small - 1),
                vec2(-16, -8),
            ),
            // The remaining squares start at the screen center, and their
            // velocities differ so that they drift apart.
            // Cyan (#00ffff).
            square(large, [255, 255, 0, 255], center(large), vec2(12, 6)),
            // Semi-transparent yellow (#ffff00).
            square(small, [0, 255, 255, 150], center(small), vec2(-12, -6)),
            // White (#ffffff).
            square(large, [255, 255, 255, 255], center(large), vec2(6, -12)),
            // Semi-transparent gray (#808080).
            square(small, [128, 128, 128, 150], center(small), vec2(-6, 12)),
        ];
        assert!(layer_count <= squares.len());
        squares.truncate(layer_count);

        MultiLayerSquaresScene { width, height, squares, drawn: HashMap::new() }
    }

    /// Renders the square of layer `layer_index` into `surface`, the pixels
    /// of the image with ID `image_id`, and cleans the caches for every byte
    /// written.
    fn render_layer(
        &mut self,
        layer_index: usize,
        image_id: ImageId,
        surface: &Surface,
    ) -> Result<()> {
        let square = &self.squares[layer_index];
        // Forget the image's content until it is fully rendered, so a failed
        // render is followed by a full redraw.
        match self.drawn.remove(&image_id) {
            Some(old_frame) => {
                // The image holds a transparent layer with the square at
                // `old_frame`. Only the old and new square change.
                surface.fill_region(&TRANSPARENT, &old_frame)?;
                surface.fill_region(&square.color, &square.frame)?;
                surface.cache_clean_regions(&[old_frame, square.frame])?;
            }
            None => {
                surface.fill_region(
                    &TRANSPARENT,
                    &Frame { pos_x: 0, pos_y: 0, width: self.width, height: self.height },
                )?;
                surface.fill_region(&square.color, &square.frame)?;
                surface.cache_clean()?;
            }
        }
        self.drawn.insert(image_id, square.frame);
        Ok(())
    }
}

/// The layer background color.
const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

impl MultiLayerScene for MultiLayerSquaresScene {
    fn update(&mut self) -> Result<()> {
        for s in &mut self.squares {
            s.update(self.width, self.height);
        }
        Ok(())
    }

    fn init_images(&self, images: &mut [MappedImage]) -> Result<()> {
        for image in images {
            image.zero().context("failed to zero image")?;
        }
        Ok(())
    }

    fn render(&mut self, images: &mut [MappedImage]) -> Result<()> {
        if images.len() < self.squares.len() {
            anyhow::bail!(
                "expected at least {} images for rendering, got {}",
                self.squares.len(),
                images.len()
            );
        }
        for layer_index in 0..self.squares.len() {
            let image = &images[layer_index];
            self.render_layer(layer_index, image.id(), image.surface())?;
        }
        Ok(())
    }

    fn get_alpha_for_layer(&self, layer_index: usize) -> Option<Alpha> {
        match layer_index {
            // For the first layer (index 0), make it fully opaque.
            0 => Some(Alpha {
                mode: fdisplay_types::AlphaMode::Disable,
                val: 0.0, // currently ignored
            }),
            // For the rest of the layers, use an alpha mode.
            _ => Some(Alpha {
                mode: fdisplay_types::AlphaMode::Premultiplied,
                val: 0.0, // currently ignored
            }),
        }
    }
}

pub async fn run(
    coordinator: &Coordinator,
    display: &DisplayInfo,
    layer_count: usize,
) -> Result<()> {
    if layer_count < MIN_LAYER_COUNT {
        return Err(format_err!(
            "--layer-count must be at least {}, got {}",
            MIN_LAYER_COUNT,
            layer_count
        ));
    }
    let max_layer_count = display.0.max_layer_count as usize;
    if layer_count > max_layer_count {
        return Err(format_err!(
            "--layer-count {} exceeds the {} layers supported by display {:?}",
            layer_count,
            max_layer_count,
            display.id()
        ));
    }

    let (width, height) = {
        let mode = &display.0.modes[0];
        (mode.active_area.width, mode.active_area.height)
    };

    let scene = MultiLayerSquaresScene::new(width, height, layer_count);
    let mut fence_loop = MultiLayerFenceLoop::new(
        coordinator,
        display.id(),
        width,
        height,
        PixelFormat::Bgra32,
        layer_count,
        scene,
    )
    .await?;

    fence_loop.run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{ByteCounts, testing};
    use googletest::{expect_eq, expect_true, gtest};
    use std::collections::HashSet;

    #[gtest]
    #[fuchsia::test]
    fn one_square_per_layer() {
        for layer_count in MIN_LAYER_COUNT..=8 {
            let scene = MultiLayerSquaresScene::new(1920, 1080, layer_count);
            expect_eq!(scene.squares.len(), layer_count);
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn squares_alternate_between_large_opaque_and_small_semi_transparent() {
        let scene = MultiLayerSquaresScene::new(1920, 1080, 8);
        for (layer_index, square) in scene.squares.iter().enumerate() {
            let (expected_size, expected_alpha) =
                if layer_index % 2 == 0 { (540, 255) } else { (135, 150) };
            expect_eq!(square.frame.width, expected_size, "layer {}", layer_index);
            expect_eq!(square.frame.height, expected_size, "layer {}", layer_index);
            expect_eq!(square.color[3], expected_alpha, "layer {}", layer_index);
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn square_colors_are_premultiplied() {
        let scene = MultiLayerSquaresScene::new(1920, 1080, 8);
        for (layer_index, square) in scene.squares.iter().enumerate() {
            let [b, g, r, alpha] = square.color;
            expect_true!(
                b <= alpha && g <= alpha && r <= alpha,
                "layer {}: {:?}",
                layer_index,
                square.color
            );
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn premultiply_scales_color_channels_by_alpha() {
        expect_eq!(premultiply([100, 255, 0, 150]), [59, 150, 0, 150]);
        expect_eq!(premultiply([128, 128, 128, 150]), [75, 75, 75, 150]);
        expect_eq!(premultiply([255, 100, 0, 255]), [255, 100, 0, 255]);
        expect_eq!(premultiply([255, 255, 255, 0]), [0, 0, 0, 0]);
    }

    #[gtest]
    #[fuchsia::test]
    fn squares_have_distinct_colors_and_velocities() {
        let scene = MultiLayerSquaresScene::new(1920, 1080, 8);
        let colors: HashSet<[u8; 4]> = scene.squares.iter().map(|square| square.color).collect();
        expect_eq!(colors.len(), scene.squares.len());
        let velocities: HashSet<Vector2D<i64>> =
            scene.squares.iter().map(|square| square.velocity).collect();
        expect_eq!(velocities.len(), scene.squares.len());
    }

    /// Renders `frame_count` frames of a `width` x `height` scene with
    /// `layer_count` layers into 2 alternating sets of test surfaces, as the
    /// runner does, and checks each rendered layer against a full redraw.
    fn check_incremental_rendering(
        width: u32,
        height: u32,
        layer_count: usize,
        frame_count: usize,
    ) {
        let mut scene = MultiLayerSquaresScene::new(width, height, layer_count);
        let full_frame = Frame { pos_x: 0, pos_y: 0, width, height };
        let presentations: Vec<Vec<Surface>> = (0..2)
            .map(|_| (0..layer_count).map(|_| testing::new_surface(width, height)).collect())
            .collect();
        // Start from non-zero pixels, so the first render of each image must
        // not rely on the image's initial contents.
        for surface in presentations.iter().flatten() {
            surface.fill_region(&[9, 9, 9, 9], &full_frame).expect("fill_region");
            let _ = surface.take_log();
            let _ = surface.take_byte_counts();
        }
        let reference = testing::new_surface(width, height);
        let mut bounces = HashSet::new();

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

            let presentation_index = frame_index % 2;
            for (layer_index, surface) in presentations[presentation_index].iter().enumerate() {
                let image_id = ImageId((1 + presentation_index * layer_count + layer_index) as u64);
                let old_frame = scene.drawn.get(&image_id).copied();
                scene.render_layer(layer_index, image_id, surface).expect("render_layer");

                // Every byte written must be cleaned before the image is
                // presented.
                expect_eq!(
                    testing::first_uncleaned_byte(&surface.take_log()),
                    None,
                    "frame {} layer {}",
                    frame_index,
                    layer_index
                );

                let square = &scene.squares[layer_index];
                reference.fill_region(&TRANSPARENT, &full_frame).expect("fill_region");
                reference.fill_region(&square.color, &square.frame).expect("fill_region");
                let first_difference = surface
                    .bytes()
                    .iter()
                    .zip(reference.bytes().iter())
                    .position(|(actual, expected)| actual != expected);
                expect_eq!(first_difference, None, "frame {} layer {}", frame_index, layer_index);

                // After the first render of each image, only the old and the
                // new square are drawn, and exactly their pixels are cleaned,
                // once each.
                let ByteCounts { written, cleaned } = surface.take_byte_counts();
                if frame_index >= 2 {
                    let area = |frame: &Frame| u64::from(frame.width) * u64::from(frame.height);
                    let square_bytes = area(&square.frame) * 4;
                    expect_eq!(written, 2 * square_bytes, "frame {}", frame_index);
                    let old_frame = old_frame.expect("image was rendered before");
                    let overlap = old_frame.intersection(&square.frame).map_or(0, |f| area(&f));
                    expect_eq!(
                        cleaned,
                        (area(&old_frame) - overlap + area(&square.frame)) * 4,
                        "frame {} layer {}",
                        frame_index,
                        layer_index
                    );
                }
            }
        }
        expect_eq!(bounces.len(), 4, "bounces: {:?}", bounces);
    }

    #[gtest]
    #[fuchsia::test]
    fn incremental_rendering_matches_full_redraw_portrait() {
        check_incremental_rendering(120, 200, 8, 200);
    }

    #[gtest]
    #[fuchsia::test]
    fn incremental_rendering_matches_full_redraw_landscape() {
        check_incremental_rendering(200, 120, 2, 200);
    }
}
