// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

///! Demonstrates building a two-frame animation using a double-buffer
///! swapchain. The animated contents are pre-rendered before display
///! presentations.
use {
    anyhow::Result,
    display_utils::{Coordinator, DisplayInfo, ImageId, PixelFormat},
    rand::{thread_rng, Rng},
    std::collections::HashSet,
};

use crate::{draw::MappedImage, runner::DoubleBufferedFenceLoop};

use crate::runner::Scene;

const BGRA_COLORS: [[u8; 4]; 6] = [
    [255, 0, 0, 255],
    [0, 255, 0, 255],
    [0, 0, 255, 255],
    [255, 255, 0, 255],
    [0, 255, 255, 255],
    [255, 0, 255, 255],
];

#[derive(Default)]
struct FlippingColorsScene {
    rendered_image_ids: HashSet<ImageId>,
    used_color_indices: HashSet<usize>,
}

impl FlippingColorsScene {
    pub fn new() -> Self {
        FlippingColorsScene {
            rendered_image_ids: HashSet::new(),
            used_color_indices: HashSet::new(),
        }
    }
}

impl Scene for FlippingColorsScene {
    fn update(&mut self) -> Result<()> {
        Ok(())
    }

    fn init_image(&self, image: &mut MappedImage) -> Result<()> {
        image.zero()?;
        Ok(())
    }

    fn render(&mut self, image: &mut MappedImage) -> Result<()> {
        // Each frame buffer image is rendered exactly once using an unique
        // color.
        if !self.rendered_image_ids.contains(&image.id()) {
            assert!(self.used_color_indices.len() < BGRA_COLORS.len());
            let mut rng = thread_rng();
            let color = loop {
                let color_index = rng.gen_range(0..BGRA_COLORS.len());
                if !self.used_color_indices.insert(color_index) {
                    continue;
                }
                break BGRA_COLORS[color_index];
            };
            image.fill(&color)?;
            image.cache_clean()?;
            self.rendered_image_ids.insert(image.id());
        }
        Ok(())
    }
}

pub async fn run(coordinator: &Coordinator, display: &DisplayInfo) -> Result<()> {
    // Obtain the display resolution based on the display's preferred mode.
    let (width, height) = {
        let mode = display.0.modes[0];
        (mode.horizontal_resolution, mode.vertical_resolution)
    };

    let scene = FlippingColorsScene::new();
    let mut double_buffered_fence_loop = DoubleBufferedFenceLoop::new(
        coordinator,
        display.id(),
        width,
        height,
        PixelFormat::Bgra32,
        scene,
    )
    .await?;

    double_buffered_fence_loop.run().await?;
    Ok(())
}
