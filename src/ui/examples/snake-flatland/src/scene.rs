// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::game::{Direction, GameCommand, GameStatus, INITIAL_SNAKE_LEN, SnakeGame};
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_ui_composition as fland;

const ROOT_TRANSFORM_ID: fland::TransformId = fland::TransformId { value: 1 };

const DEFAULT_VIEWPORT_WIDTH: u32 = 800;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 600;
const MIN_VIEWPORT_DIM: u32 = 200;
const MAX_DISPLAY_SCORE: u32 = 999;
const MIN_DPAD_ZONE_HEIGHT: i32 = 120;
const HUD_LINE_HEIGHT: i32 = 4;

// Color palette using straight RGBA in [0.0, 1.0].
const COLOR_SCREEN_BG: fland::ColorRgba =
    fland::ColorRgba { red: 0.06, green: 0.08, blue: 0.12, alpha: 1.0 };
const COLOR_HUD_BG: fland::ColorRgba =
    fland::ColorRgba { red: 0.09, green: 0.12, blue: 0.18, alpha: 1.0 };
const COLOR_HUD_LINE: fland::ColorRgba =
    fland::ColorRgba { red: 0.18, green: 0.24, blue: 0.36, alpha: 1.0 };
const COLOR_BOARD_BG: fland::ColorRgba =
    fland::ColorRgba { red: 0.07, green: 0.10, blue: 0.15, alpha: 1.0 };

const COLOR_BORDER_PLAYING: fland::ColorRgba =
    fland::ColorRgba { red: 0.18, green: 0.75, blue: 0.65, alpha: 1.0 };
const COLOR_BORDER_PAUSED: fland::ColorRgba =
    fland::ColorRgba { red: 0.95, green: 0.72, blue: 0.18, alpha: 1.0 };
const COLOR_BORDER_WON: fland::ColorRgba = COLOR_BORDER_PAUSED;
const COLOR_BORDER_GAME_OVER: fland::ColorRgba =
    fland::ColorRgba { red: 0.92, green: 0.22, blue: 0.28, alpha: 1.0 };

const COLOR_SNAKE_HEAD: fland::ColorRgba =
    fland::ColorRgba { red: 0.28, green: 1.00, blue: 0.52, alpha: 1.0 };
const COLOR_SNAKE_BODY_EVEN: fland::ColorRgba =
    fland::ColorRgba { red: 0.18, green: 0.88, blue: 0.42, alpha: 1.0 };
const COLOR_SNAKE_BODY_ODD: fland::ColorRgba =
    fland::ColorRgba { red: 0.13, green: 0.72, blue: 0.33, alpha: 1.0 };

const COLOR_SNAKE_DEAD_HEAD: fland::ColorRgba =
    fland::ColorRgba { red: 1.00, green: 0.32, blue: 0.32, alpha: 1.0 };
const COLOR_SNAKE_DEAD_EVEN: fland::ColorRgba =
    fland::ColorRgba { red: 0.85, green: 0.22, blue: 0.22, alpha: 1.0 };
const COLOR_SNAKE_DEAD_ODD: fland::ColorRgba =
    fland::ColorRgba { red: 0.68, green: 0.16, blue: 0.16, alpha: 1.0 };

const COLOR_SNAKE_EYE: fland::ColorRgba =
    fland::ColorRgba { red: 0.05, green: 0.07, blue: 0.10, alpha: 1.0 };

const COLOR_FOOD_MAIN: fland::ColorRgba =
    fland::ColorRgba { red: 0.95, green: 0.22, blue: 0.28, alpha: 1.0 };
const COLOR_FOOD_HIGHLIGHT: fland::ColorRgba =
    fland::ColorRgba { red: 1.00, green: 0.55, blue: 0.60, alpha: 1.0 };
const COLOR_FOOD_STEM: fland::ColorRgba =
    fland::ColorRgba { red: 0.30, green: 0.90, blue: 0.35, alpha: 1.0 };

const COLOR_SCORE_ON: fland::ColorRgba =
    fland::ColorRgba { red: 0.18, green: 0.92, blue: 0.65, alpha: 1.0 };
const COLOR_HIGH_SCORE_ON: fland::ColorRgba =
    fland::ColorRgba { red: 0.96, green: 0.74, blue: 0.20, alpha: 1.0 };
const COLOR_DIGIT_OFF: fland::ColorRgba =
    fland::ColorRgba { red: 0.13, green: 0.17, blue: 0.25, alpha: 1.0 };

const COLOR_DPAD_BTN: fland::ColorRgba =
    fland::ColorRgba { red: 0.12, green: 0.17, blue: 0.26, alpha: 1.0 };
const COLOR_DPAD_ACTIVE: fland::ColorRgba =
    fland::ColorRgba { red: 0.18, green: 0.32, blue: 0.46, alpha: 1.0 };
const COLOR_DPAD_ICON: fland::ColorRgba =
    fland::ColorRgba { red: 0.35, green: 0.88, blue: 0.76, alpha: 1.0 };

const COLOR_OVERLAY_BACKDROP: fland::ColorRgba =
    fland::ColorRgba { red: 0.03, green: 0.04, blue: 0.07, alpha: 0.72 };

/// 7-segment bitmask for digits 0..=9:
/// bit 0: top (a), bit 1: top-right (b), bit 2: bottom-right (c),
/// bit 3: bottom (d), bit 4: bottom-left (e), bit 5: top-left (f), bit 6: middle (g).
const DIGIT_SEGMENTS: [u8; 10] = [
    0b011_1111, // 0
    0b000_0110, // 1
    0b101_1011, // 2
    0b100_1111, // 3
    0b110_0110, // 4
    0b110_1101, // 5
    0b111_1101, // 6
    0b000_0111, // 7
    0b111_1111, // 8
    0b110_1111, // 9
];

const MAX_LAYERS_PER_STACK: usize = fland::MAX_STACK_LAYERS as usize;

#[derive(Debug)]
struct IdGenerator {
    next_id: u64,
}

impl IdGenerator {
    fn new() -> Self {
        // 1 is reserved for ROOT_TRANSFORM_ID.
        Self { next_id: 2 }
    }

    fn next_transform_id(&mut self) -> fland::TransformId {
        let id = self.next_id;
        self.next_id += 1;
        fland::TransformId { value: id }
    }

    fn next_layer_id(&mut self) -> fland::LayerId {
        let id = self.next_id;
        self.next_id += 1;
        fland::LayerId { value: id }
    }

    fn next_layer_stack_id(&mut self) -> fland::LayerStackId {
        let id = self.next_id;
        self.next_id += 1;
        fland::LayerStackId { value: id }
    }
}

#[derive(Debug)]
struct LayerStackSlot {
    stack_id: fland::LayerStackId,
    layers: Vec<fland::LayerId>,
    dirty: bool,
}

/// Packs Flatland2 [`fland::LayerId`]s into [`fland::LayerStackId`]s of up to
/// [`MAX_LAYERS_PER_STACK`] layers under a group parent transform.
#[derive(Debug)]
struct LayerStackGroup {
    parent_transform: fland::TransformId,
    stacks: Vec<LayerStackSlot>,
}

impl LayerStackGroup {
    fn new(flatland: &fland::FlatlandProxy, ids: &mut IdGenerator) -> Self {
        let parent_transform = ids.next_transform_id();
        flatland.create_transform(&parent_transform).expect("fidl error");
        flatland.add_child(&ROOT_TRANSFORM_ID, &parent_transform).expect("fidl error");
        Self { parent_transform, stacks: Vec::new() }
    }

    fn alloc_rect(&mut self, flatland: &fland::FlatlandProxy, ids: &mut IdGenerator) -> RectNode {
        let layer_id = ids.next_layer_id();
        flatland.create_layer(&layer_id).expect("fidl error");

        let need_new_stack =
            self.stacks.last().is_none_or(|slot| slot.layers.len() >= MAX_LAYERS_PER_STACK);
        if need_new_stack {
            let transform_id = ids.next_transform_id();
            let stack_id = ids.next_layer_stack_id();
            flatland.create_transform(&transform_id).expect("fidl error");
            flatland.add_child(&self.parent_transform, &transform_id).expect("fidl error");
            flatland.create_layer_stack(&stack_id).expect("fidl error");
            flatland
                .set_transform_content(
                    &transform_id,
                    Some(&fland::TransformContent::LayerStack(stack_id)),
                )
                .expect("fidl error");
            self.stacks.push(LayerStackSlot {
                stack_id,
                layers: Vec::with_capacity(MAX_LAYERS_PER_STACK),
                dirty: false,
            });
        }

        let slot = self.stacks.last_mut().expect("stack slot created");
        slot.layers.push(layer_id);
        slot.dirty = true;

        RectNode { layer_id, visible: false, last_rect: None, last_color: None }
    }

    fn flush(&mut self, flatland: &fland::FlatlandProxy) {
        for slot in &mut self.stacks {
            if slot.dirty {
                flatland.set_stack_layers(&slot.stack_id, &slot.layers).expect("fidl error");
                slot.dirty = false;
            }
        }
    }
}

/// A single Flatland2 solid-color `Layer` whose geometry, color, and visibility
/// are updated via `SetLayerProperties` without mutating the transform graph.
#[derive(Debug)]
struct RectNode {
    layer_id: fland::LayerId,
    visible: bool,
    last_rect: Option<fmath::RectU>,
    last_color: Option<fland::ColorRgba>,
}

impl RectNode {
    fn show(
        &mut self,
        flatland: &fland::FlatlandProxy,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        color: fland::ColorRgba,
    ) {
        // Clip against the top-left origin if a negative coordinate is ever passed.
        let clip_x = (-x).max(0) as u32;
        let clip_y = (-y).max(0) as u32;
        let width = width.saturating_sub(clip_x);
        let height = height.saturating_sub(clip_y);
        if width == 0 || height == 0 {
            self.hide(flatland);
            return;
        }
        let rect = fmath::RectU { x: x.max(0) as u32, y: y.max(0) as u32, width, height };
        let mode_changed = !self.visible;
        let rect_changed = self.last_rect != Some(rect);
        let color_changed = self.last_color != Some(color);

        if mode_changed || rect_changed || color_changed {
            let properties = fland::LayerProperties {
                composition_mode: mode_changed.then_some(fland::CompositionMode::SolidColor),
                display_rect: rect_changed.then_some(rect),
                color: color_changed.then_some(color),
                blend_mode: color_changed.then_some(if color.alpha < 1.0 {
                    fland::BlendMode2::StraightAlpha
                } else {
                    fland::BlendMode2::Replace
                }),
                ..Default::default()
            };
            flatland.set_layer_properties(&self.layer_id, &properties).expect("fidl error");
            self.visible = true;
            self.last_rect = Some(rect);
            self.last_color = Some(color);
        }
    }

    fn hide(&mut self, flatland: &fland::FlatlandProxy) {
        if self.visible {
            let properties = fland::LayerProperties {
                composition_mode: Some(fland::CompositionMode::Invisible),
                ..Default::default()
            };
            flatland.set_layer_properties(&self.layer_id, &properties).expect("fidl error");
            self.visible = false;
        }
    }
}

#[derive(Debug)]
struct SevenSegmentDigit {
    segments: [RectNode; 7],
}

impl SevenSegmentDigit {
    fn new(
        flatland: &fland::FlatlandProxy,
        group: &mut LayerStackGroup,
        ids: &mut IdGenerator,
    ) -> Self {
        Self { segments: std::array::from_fn(|_| group.alloc_rect(flatland, ids)) }
    }

    fn render(
        &mut self,
        flatland: &fland::FlatlandProxy,
        origin_x: i32,
        origin_y: i32,
        digit_w: i32,
        digit_h: i32,
        thickness: i32,
        digit: u8,
        on_color: fland::ColorRgba,
    ) {
        let mask = DIGIT_SEGMENTS[(digit as usize) % 10];
        let t = thickness.max(2);
        let w = digit_w.max(t * 3);
        let h = digit_h.max(t * 5);
        let half_h = h / 2;

        // Geometries of the 7 segments (a..=g) relative to (origin_x, origin_y):
        let rects: [(i32, i32, u32, u32); 7] = [
            // a: top horizontal
            (origin_x, origin_y, w as u32, t as u32),
            // b: top-right vertical
            (origin_x + w - t, origin_y, t as u32, (half_h + t / 2) as u32),
            // c: bottom-right vertical
            (origin_x + w - t, origin_y + half_h - t / 2, t as u32, (h - half_h + t / 2) as u32),
            // d: bottom horizontal
            (origin_x, origin_y + h - t, w as u32, t as u32),
            // e: bottom-left vertical
            (origin_x, origin_y + half_h - t / 2, t as u32, (h - half_h + t / 2) as u32),
            // f: top-left vertical
            (origin_x, origin_y, t as u32, (half_h + t / 2) as u32),
            // g: middle horizontal
            (origin_x, origin_y + half_h - t / 2, w as u32, t as u32),
        ];

        for (i, &(x, y, rw, rh)) in rects.iter().enumerate() {
            let active = (mask & (1 << i)) != 0;
            let color = if active { on_color } else { COLOR_DIGIT_OFF };
            self.segments[i].show(flatland, x, y, rw, rh, color);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HitRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl HitRect {
    fn contains(&self, px: i32, py: i32) -> bool {
        self.w > 0
            && self.h > 0
            && px >= self.x
            && px < self.x + self.w
            && py >= self.y
            && py < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BoardLayout {
    viewport_w: i32,
    viewport_h: i32,
    top_inset: i32,
    hud_h: i32,
    margin: i32,
    board_x: i32,
    board_y: i32,
    board_px_w: i32,
    board_px_h: i32,
    cell_size: i32,
    border_thickness: i32,
    has_dpad: bool,
    dpad_up: HitRect,
    dpad_down: HitRect,
    dpad_left: HitRect,
    dpad_right: HitRect,
    dpad_center: HitRect,
}

/// Manages the Flatland2 scene using only `CompositionMode::SolidColor` layers.
#[derive(Debug)]
pub(crate) struct SnakeScene {
    viewport_size: fmath::SizeU,
    ids: IdGenerator,
    snake_group: LayerStackGroup,
    screen_bg: RectNode,
    hud_bg: RectNode,
    hud_line: RectNode,
    status_indicator: RectNode,
    score_digits: [SevenSegmentDigit; 3],
    high_score_digits: [SevenSegmentDigit; 3],
    board_border: RectNode,
    board_bg: RectNode,
    food_main: RectNode,
    food_highlight: RectNode,
    food_stem: RectNode,
    snake_segments: Vec<RectNode>,
    eye_left: RectNode,
    eye_right: RectNode,
    dpad_up_btn: RectNode,
    dpad_down_btn: RectNode,
    dpad_left_btn: RectNode,
    dpad_right_btn: RectNode,
    dpad_center_btn: RectNode,
    dpad_up_pip: RectNode,
    dpad_down_pip: RectNode,
    dpad_left_pip: RectNode,
    dpad_right_pip: RectNode,
    overlay_backdrop: RectNode,
    /// Used as the left vertical bar when Paused, or outer square badge when GameOver/Won.
    overlay_primary: RectNode,
    /// Used as the right vertical bar when Paused, or inner square cutout when GameOver/Won.
    overlay_secondary: RectNode,
    layout: BoardLayout,
}

impl SnakeScene {
    pub(crate) fn new(flatland: &fland::FlatlandProxy) -> Self {
        flatland.create_transform(&ROOT_TRANSFORM_ID).expect("fidl error");
        flatland.set_root_transform(&ROOT_TRANSFORM_ID).expect("fidl error");

        let mut ids = IdGenerator::new();

        // Create 3 ordered layer-stack groups under ROOT_TRANSFORM_ID so dynamically grown
        // snake_segments never render on top of eyes, D-Pad, or status overlays.
        let mut base_group = LayerStackGroup::new(flatland, &mut ids);
        let mut snake_group = LayerStackGroup::new(flatland, &mut ids);
        let mut top_group = LayerStackGroup::new(flatland, &mut ids);

        let screen_bg = base_group.alloc_rect(flatland, &mut ids);
        let hud_bg = base_group.alloc_rect(flatland, &mut ids);
        let hud_line = base_group.alloc_rect(flatland, &mut ids);
        let status_indicator = base_group.alloc_rect(flatland, &mut ids);
        let score_digits =
            std::array::from_fn(|_| SevenSegmentDigit::new(flatland, &mut base_group, &mut ids));
        let high_score_digits =
            std::array::from_fn(|_| SevenSegmentDigit::new(flatland, &mut base_group, &mut ids));
        let board_border = base_group.alloc_rect(flatland, &mut ids);
        let board_bg = base_group.alloc_rect(flatland, &mut ids);
        let food_main = base_group.alloc_rect(flatland, &mut ids);
        let food_highlight = base_group.alloc_rect(flatland, &mut ids);
        let food_stem = base_group.alloc_rect(flatland, &mut ids);
        base_group.flush(flatland);

        let mut snake_segments = Vec::with_capacity(INITIAL_SNAKE_LEN as usize);
        for _ in 0..INITIAL_SNAKE_LEN {
            snake_segments.push(snake_group.alloc_rect(flatland, &mut ids));
        }
        snake_group.flush(flatland);

        let eye_left = top_group.alloc_rect(flatland, &mut ids);
        let eye_right = top_group.alloc_rect(flatland, &mut ids);
        let dpad_up_btn = top_group.alloc_rect(flatland, &mut ids);
        let dpad_down_btn = top_group.alloc_rect(flatland, &mut ids);
        let dpad_left_btn = top_group.alloc_rect(flatland, &mut ids);
        let dpad_right_btn = top_group.alloc_rect(flatland, &mut ids);
        let dpad_center_btn = top_group.alloc_rect(flatland, &mut ids);
        let dpad_up_pip = top_group.alloc_rect(flatland, &mut ids);
        let dpad_down_pip = top_group.alloc_rect(flatland, &mut ids);
        let dpad_left_pip = top_group.alloc_rect(flatland, &mut ids);
        let dpad_right_pip = top_group.alloc_rect(flatland, &mut ids);
        let overlay_backdrop = top_group.alloc_rect(flatland, &mut ids);
        let overlay_primary = top_group.alloc_rect(flatland, &mut ids);
        let overlay_secondary = top_group.alloc_rect(flatland, &mut ids);
        top_group.flush(flatland);

        let mut scene = Self {
            viewport_size: fmath::SizeU {
                width: DEFAULT_VIEWPORT_WIDTH,
                height: DEFAULT_VIEWPORT_HEIGHT,
            },
            ids,
            snake_group,
            screen_bg,
            hud_bg,
            hud_line,
            status_indicator,
            score_digits,
            high_score_digits,
            board_border,
            board_bg,
            food_main,
            food_highlight,
            food_stem,
            snake_segments,
            eye_left,
            eye_right,
            dpad_up_btn,
            dpad_down_btn,
            dpad_left_btn,
            dpad_right_btn,
            dpad_center_btn,
            dpad_up_pip,
            dpad_down_pip,
            dpad_left_pip,
            dpad_right_pip,
            overlay_backdrop,
            overlay_primary,
            overlay_secondary,
            layout: BoardLayout::default(),
        };
        scene.compute_board_layout();
        scene
    }

    pub(crate) fn set_viewport_size(&mut self, size: fmath::SizeU) {
        if size.width > 0 && size.height > 0 {
            self.viewport_size = size;
            self.compute_board_layout();
        }
    }

    pub(crate) fn hit_test_dpad(&self, x: f32, y: f32) -> Option<GameCommand> {
        let px = x.round() as i32;
        let py = y.round() as i32;
        if self.layout.dpad_up.contains(px, py) {
            Some(GameCommand::Move(Direction::Up))
        } else if self.layout.dpad_down.contains(px, py) {
            Some(GameCommand::Move(Direction::Down))
        } else if self.layout.dpad_left.contains(px, py) {
            Some(GameCommand::Move(Direction::Left))
        } else if self.layout.dpad_right.contains(px, py) {
            Some(GameCommand::Move(Direction::Right))
        } else if self.layout.dpad_center.contains(px, py) {
            Some(GameCommand::TogglePause)
        } else {
            None
        }
    }

    /// Converts a point `(x, y)` in view coordinates to fractional board grid coordinates.
    pub(crate) fn view_to_grid(&self, x: f32, y: f32) -> (f32, f32) {
        let cell = (self.layout.cell_size.max(1)) as f32;
        ((x - self.layout.board_x as f32) / cell, (y - self.layout.board_y as f32) / cell)
    }

    pub(crate) fn render(&mut self, flatland: &fland::FlatlandProxy, game: &SnakeGame) {
        self.compute_board_layout();
        let status_color = Self::status_color(game.status());

        self.screen_bg.show(
            flatland,
            0,
            0,
            self.layout.viewport_w as u32,
            self.layout.viewport_h as u32,
            COLOR_SCREEN_BG,
        );

        self.render_hud(flatland, game, status_color);
        self.render_board_and_food(flatland, game, status_color);
        self.render_snake(flatland, game);
        self.render_dpad(flatland, game);
        self.render_overlay(flatland, game);
    }

    fn status_color(status: GameStatus) -> fland::ColorRgba {
        match status {
            GameStatus::Playing => COLOR_BORDER_PLAYING,
            GameStatus::Paused => COLOR_BORDER_PAUSED,
            GameStatus::Won => COLOR_BORDER_WON,
            GameStatus::GameOver => COLOR_BORDER_GAME_OVER,
        }
    }

    fn compute_board_layout(&mut self) {
        let (grid_w, grid_h) = SnakeGame::grid_dimensions();
        let viewport_w = self.viewport_size.width.max(MIN_VIEWPORT_DIM) as i32;
        let viewport_h = self.viewport_size.height.max(MIN_VIEWPORT_DIM) as i32;
        let is_taller_than_wide = viewport_h > viewport_w;
        let top_inset = if is_taller_than_wide { (viewport_h / 28).clamp(24, 96) } else { 0 };
        let hud_h = top_inset + (viewport_h / 12).clamp(48, 120);
        let margin = (viewport_w.min(viewport_h) / 28).clamp(8, 36);

        let has_room_for_dpad = viewport_h > viewport_w * 5 / 4;
        let raw_dpad_h = if has_room_for_dpad {
            (viewport_h - hud_h - viewport_w).max(viewport_h / 4)
        } else {
            0
        };
        let has_dpad = raw_dpad_h >= MIN_DPAD_ZONE_HEIGHT;
        let dpad_zone_h = if has_dpad { raw_dpad_h } else { 0 };

        let avail_w = (viewport_w - margin * 2).max(grid_w * 4);
        let avail_h =
            (viewport_h - hud_h - HUD_LINE_HEIGHT - dpad_zone_h - margin * 2).max(grid_h * 4);
        let cell_size = (avail_w / grid_w).min(avail_h / grid_h).max(4);
        let border_thickness = (cell_size / 4).clamp(3, 12);

        let board_px_w = cell_size * grid_w;
        let board_px_h = cell_size * grid_h;
        let board_x = (viewport_w - board_px_w) / 2;
        let board_y = hud_h
            + HUD_LINE_HEIGHT
            + (viewport_h - hud_h - HUD_LINE_HEIGHT - dpad_zone_h - board_px_h) / 2;

        let (dpad_up, dpad_down, dpad_left, dpad_right, dpad_center) = if has_dpad {
            let dpad_top = board_y + board_px_h + border_thickness + margin;
            let dpad_avail_h = (viewport_h - dpad_top - margin).max(0);
            let dpad_avail_w = (viewport_w - margin * 2).max(0);
            let dpad_span = dpad_avail_h.min(dpad_avail_w);
            let gap = (dpad_span / 24).clamp(4, 16);
            let btn_size = ((dpad_span - 2 * gap) / 3).clamp(24, 180);
            let cx = viewport_w / 2;
            let cy = dpad_top + dpad_avail_h / 2;
            let center_x = cx - btn_size / 2;
            let center_y = cy - btn_size / 2;
            (
                HitRect { x: center_x, y: center_y - gap - btn_size, w: btn_size, h: btn_size },
                HitRect { x: center_x, y: center_y + btn_size + gap, w: btn_size, h: btn_size },
                HitRect { x: center_x - gap - btn_size, y: center_y, w: btn_size, h: btn_size },
                HitRect { x: center_x + btn_size + gap, y: center_y, w: btn_size, h: btn_size },
                HitRect { x: center_x, y: center_y, w: btn_size, h: btn_size },
            )
        } else {
            (
                HitRect::default(),
                HitRect::default(),
                HitRect::default(),
                HitRect::default(),
                HitRect::default(),
            )
        };

        self.layout = BoardLayout {
            viewport_w,
            viewport_h,
            top_inset,
            hud_h,
            margin,
            board_x,
            board_y,
            board_px_w,
            board_px_h,
            cell_size,
            border_thickness,
            has_dpad,
            dpad_up,
            dpad_down,
            dpad_left,
            dpad_right,
            dpad_center,
        };
    }

    fn render_three_digit_number(
        flatland: &fland::FlatlandProxy,
        digits: &mut [SevenSegmentDigit; 3],
        value: u32,
        start_x: i32,
        digit_y: i32,
        digit_w: i32,
        digit_h: i32,
        digit_gap: i32,
        seg_thickness: i32,
        on_color: fland::ColorRgba,
    ) {
        let clamped = value.min(MAX_DISPLAY_SCORE);
        let vals =
            [((clamped / 100) % 10) as u8, ((clamped / 10) % 10) as u8, (clamped % 10) as u8];
        for (i, &d) in vals.iter().enumerate() {
            let dx = start_x + (i as i32) * (digit_w + digit_gap);
            digits[i].render(flatland, dx, digit_y, digit_w, digit_h, seg_thickness, d, on_color);
        }
    }

    fn render_hud(
        &mut self,
        flatland: &fland::FlatlandProxy,
        game: &SnakeGame,
        status_color: fland::ColorRgba,
    ) {
        let BoardLayout { viewport_w, top_inset, hud_h, margin, board_x, board_px_w, .. } =
            self.layout;

        self.hud_bg.show(flatland, 0, 0, viewport_w as u32, hud_h as u32, COLOR_HUD_BG);
        self.hud_line.show(
            flatland,
            0,
            hud_h,
            viewport_w as u32,
            HUD_LINE_HEIGHT as u32,
            COLOR_HUD_LINE,
        );

        let content_hud_h = hud_h - top_inset;
        let digit_h = (content_hud_h - 16).clamp(20, 64);
        let digit_w = (digit_h * 3 / 5).max(12);
        let digit_gap = (digit_w / 3).max(4);
        let seg_thickness = (digit_w / 5).clamp(2, 8);
        let digit_y = top_inset + (content_hud_h - digit_h) / 2;

        let left_start_x = board_x.max(margin);
        Self::render_three_digit_number(
            flatland,
            &mut self.score_digits,
            game.score(),
            left_start_x,
            digit_y,
            digit_w,
            digit_h,
            digit_gap,
            seg_thickness,
            COLOR_SCORE_ON,
        );

        let total_digits_w = 3 * digit_w + 2 * digit_gap;
        let right_start_x =
            (board_x + board_px_w - total_digits_w).min(viewport_w - margin - total_digits_w);
        Self::render_three_digit_number(
            flatland,
            &mut self.high_score_digits,
            game.high_score(),
            right_start_x,
            digit_y,
            digit_w,
            digit_h,
            digit_gap,
            seg_thickness,
            COLOR_HIGH_SCORE_ON,
        );

        let pill_w = (digit_w * 2).max(24);
        let pill_h = (digit_h / 2).max(10);
        self.status_indicator.show(
            flatland,
            (viewport_w - pill_w) / 2,
            top_inset + (content_hud_h - pill_h) / 2,
            pill_w as u32,
            pill_h as u32,
            status_color,
        );
    }

    fn render_board_and_food(
        &mut self,
        flatland: &fland::FlatlandProxy,
        game: &SnakeGame,
        status_color: fland::ColorRgba,
    ) {
        let BoardLayout {
            board_x,
            board_y,
            board_px_w,
            board_px_h,
            cell_size,
            border_thickness,
            ..
        } = self.layout;

        self.board_border.show(
            flatland,
            board_x - border_thickness,
            board_y - border_thickness,
            (board_px_w + border_thickness * 2) as u32,
            (board_px_h + border_thickness * 2) as u32,
            status_color,
        );
        self.board_bg.show(
            flatland,
            board_x,
            board_y,
            board_px_w as u32,
            board_px_h as u32,
            COLOR_BOARD_BG,
        );

        if game.status() == GameStatus::Won {
            self.food_main.hide(flatland);
            self.food_highlight.hide(flatland);
            self.food_stem.hide(flatland);
            return;
        }

        let food = game.food();
        let food_inset = (cell_size / 8).clamp(1, 3);
        let food_x = board_x + food.x * cell_size + food_inset;
        let food_y = board_y + food.y * cell_size + food_inset;
        let food_w = (cell_size - food_inset * 2).max(2);
        self.food_main.show(
            flatland,
            food_x,
            food_y,
            food_w as u32,
            food_w as u32,
            COLOR_FOOD_MAIN,
        );
        let highlight_size = (food_w / 3).max(1);
        self.food_highlight.show(
            flatland,
            food_x + (food_w / 6).max(1),
            food_y + (food_w / 6).max(1),
            highlight_size as u32,
            highlight_size as u32,
            COLOR_FOOD_HIGHLIGHT,
        );
        let stem_w = (food_w / 4).max(2);
        let stem_h = (food_w / 3).max(2);
        self.food_stem.show(
            flatland,
            food_x + (food_w - stem_w) / 2,
            (food_y - stem_h / 2).max(board_y),
            stem_w as u32,
            stem_h as u32,
            COLOR_FOOD_STEM,
        );
    }

    fn render_snake(&mut self, flatland: &fland::FlatlandProxy, game: &SnakeGame) {
        let BoardLayout { board_x, board_y, cell_size, .. } = self.layout;
        let snake = game.snake();

        while self.snake_segments.len() < snake.len() {
            self.snake_segments.push(self.snake_group.alloc_rect(flatland, &mut self.ids));
        }
        self.snake_group.flush(flatland);

        let seg_inset = (cell_size / 12).clamp(1, 2);
        let seg_size = (cell_size - seg_inset * 2).max(2) as u32;
        let is_dead = game.status() == GameStatus::GameOver;

        for (i, pos) in snake.iter().enumerate() {
            let sx = board_x + pos.x * cell_size + seg_inset;
            let sy = board_y + pos.y * cell_size + seg_inset;
            let color = if i == 0 {
                if is_dead { COLOR_SNAKE_DEAD_HEAD } else { COLOR_SNAKE_HEAD }
            } else if i % 2 == 0 {
                if is_dead { COLOR_SNAKE_DEAD_EVEN } else { COLOR_SNAKE_BODY_EVEN }
            } else {
                if is_dead { COLOR_SNAKE_DEAD_ODD } else { COLOR_SNAKE_BODY_ODD }
            };
            self.snake_segments[i].show(flatland, sx, sy, seg_size, seg_size, color);
        }

        for i in snake.len()..self.snake_segments.len() {
            if !self.snake_segments[i].visible {
                break;
            }
            self.snake_segments[i].hide(flatland);
        }

        if let Some(head) = snake.front() {
            let head_x = board_x + head.x * cell_size + seg_inset;
            let head_y = board_y + head.y * cell_size + seg_inset;
            let head_size = seg_size as i32;
            let eye_size = (head_size / 5).max(2);
            let pad = (head_size / 6).max(1);
            let far = (head_size - pad - eye_size).max(pad);

            let ((left_eye_x, left_eye_y), (right_eye_x, right_eye_y)) = match game.direction() {
                Direction::Right => ((head_x + far, head_y + pad), (head_x + far, head_y + far)),
                Direction::Left => ((head_x + pad, head_y + pad), (head_x + pad, head_y + far)),
                Direction::Up => ((head_x + pad, head_y + pad), (head_x + far, head_y + pad)),
                Direction::Down => ((head_x + pad, head_y + far), (head_x + far, head_y + far)),
            };
            self.eye_left.show(
                flatland,
                left_eye_x,
                left_eye_y,
                eye_size as u32,
                eye_size as u32,
                COLOR_SNAKE_EYE,
            );
            self.eye_right.show(
                flatland,
                right_eye_x,
                right_eye_y,
                eye_size as u32,
                eye_size as u32,
                COLOR_SNAKE_EYE,
            );
        } else {
            self.eye_left.hide(flatland);
            self.eye_right.hide(flatland);
        }
    }

    fn render_dpad(&mut self, flatland: &fland::FlatlandProxy, game: &SnakeGame) {
        let BoardLayout {
            has_dpad, dpad_up, dpad_down, dpad_left, dpad_right, dpad_center, ..
        } = self.layout;

        if !has_dpad {
            self.dpad_up_btn.hide(flatland);
            self.dpad_down_btn.hide(flatland);
            self.dpad_left_btn.hide(flatland);
            self.dpad_right_btn.hide(flatland);
            self.dpad_center_btn.hide(flatland);
            self.dpad_up_pip.hide(flatland);
            self.dpad_down_pip.hide(flatland);
            self.dpad_left_pip.hide(flatland);
            self.dpad_right_pip.hide(flatland);
            return;
        }

        let btn_size = dpad_center.w;
        let dir = game.direction();
        let up_color = if dir == Direction::Up { COLOR_DPAD_ACTIVE } else { COLOR_DPAD_BTN };
        let down_color = if dir == Direction::Down { COLOR_DPAD_ACTIVE } else { COLOR_DPAD_BTN };
        let left_color = if dir == Direction::Left { COLOR_DPAD_ACTIVE } else { COLOR_DPAD_BTN };
        let right_color = if dir == Direction::Right { COLOR_DPAD_ACTIVE } else { COLOR_DPAD_BTN };

        self.dpad_up_btn.show(
            flatland,
            dpad_up.x,
            dpad_up.y,
            dpad_up.w as u32,
            dpad_up.h as u32,
            up_color,
        );
        self.dpad_down_btn.show(
            flatland,
            dpad_down.x,
            dpad_down.y,
            dpad_down.w as u32,
            dpad_down.h as u32,
            down_color,
        );
        self.dpad_left_btn.show(
            flatland,
            dpad_left.x,
            dpad_left.y,
            dpad_left.w as u32,
            dpad_left.h as u32,
            left_color,
        );
        self.dpad_right_btn.show(
            flatland,
            dpad_right.x,
            dpad_right.y,
            dpad_right.w as u32,
            dpad_right.h as u32,
            right_color,
        );
        self.dpad_center_btn.show(
            flatland,
            dpad_center.x,
            dpad_center.y,
            dpad_center.w as u32,
            dpad_center.h as u32,
            COLOR_HUD_BG,
        );

        let pip_long = (btn_size / 3).max(8);
        let pip_short = (btn_size / 6).max(4);
        self.dpad_up_pip.show(
            flatland,
            dpad_up.x + (btn_size - pip_short) / 2,
            dpad_up.y + (btn_size - pip_long) / 2,
            pip_short as u32,
            pip_long as u32,
            COLOR_DPAD_ICON,
        );
        self.dpad_down_pip.show(
            flatland,
            dpad_down.x + (btn_size - pip_short) / 2,
            dpad_down.y + (btn_size - pip_long) / 2,
            pip_short as u32,
            pip_long as u32,
            COLOR_DPAD_ICON,
        );
        self.dpad_left_pip.show(
            flatland,
            dpad_left.x + (btn_size - pip_long) / 2,
            dpad_left.y + (btn_size - pip_short) / 2,
            pip_long as u32,
            pip_short as u32,
            COLOR_DPAD_ICON,
        );
        self.dpad_right_pip.show(
            flatland,
            dpad_right.x + (btn_size - pip_long) / 2,
            dpad_right.y + (btn_size - pip_short) / 2,
            pip_long as u32,
            pip_short as u32,
            COLOR_DPAD_ICON,
        );
    }

    fn render_overlay(&mut self, flatland: &fland::FlatlandProxy, game: &SnakeGame) {
        let BoardLayout { board_x, board_y, board_px_w, board_px_h, cell_size, .. } = self.layout;
        match game.status() {
            GameStatus::Playing => {
                self.overlay_backdrop.hide(flatland);
                self.overlay_primary.hide(flatland);
                self.overlay_secondary.hide(flatland);
            }
            GameStatus::Paused => {
                self.overlay_backdrop.show(
                    flatland,
                    board_x,
                    board_y,
                    board_px_w as u32,
                    board_px_h as u32,
                    COLOR_OVERLAY_BACKDROP,
                );
                let bar_w = (cell_size * 4 / 5).max(8);
                let bar_h = (cell_size * 3).max(24);
                let gap = bar_w;
                let cx = board_x + board_px_w / 2;
                let cy = board_y + board_px_h / 2;
                self.overlay_primary.show(
                    flatland,
                    cx - gap / 2 - bar_w,
                    cy - bar_h / 2,
                    bar_w as u32,
                    bar_h as u32,
                    COLOR_BORDER_PAUSED,
                );
                self.overlay_secondary.show(
                    flatland,
                    cx + gap / 2,
                    cy - bar_h / 2,
                    bar_w as u32,
                    bar_h as u32,
                    COLOR_BORDER_PAUSED,
                );
            }
            GameStatus::GameOver | GameStatus::Won => {
                self.overlay_backdrop.show(
                    flatland,
                    board_x,
                    board_y,
                    board_px_w as u32,
                    board_px_h as u32,
                    COLOR_OVERLAY_BACKDROP,
                );
                let outer_size = (cell_size * 4).max(32);
                let inner_size = (cell_size * 2).max(16);
                let cx = board_x + board_px_w / 2;
                let cy = board_y + board_px_h / 2;
                let badge_color = if game.status() == GameStatus::Won {
                    COLOR_BORDER_WON
                } else {
                    COLOR_BORDER_GAME_OVER
                };
                self.overlay_primary.show(
                    flatland,
                    cx - outer_size / 2,
                    cy - outer_size / 2,
                    outer_size as u32,
                    outer_size as u32,
                    badge_color,
                );
                self.overlay_secondary.show(
                    flatland,
                    cx - inner_size / 2,
                    cy - inner_size / 2,
                    inner_size as u32,
                    inner_size as u32,
                    COLOR_BOARD_BG,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GridPos;
    use fidl::endpoints::create_proxy_and_stream;
    use futures::StreamExt;

    async fn drain_flatland_requests(
        stream: &mut fland::FlatlandRequestStream,
    ) -> Vec<fland::FlatlandRequest> {
        let mut reqs = Vec::new();
        while let Some(Some(Ok(req))) = futures::FutureExt::now_or_never(stream.next()) {
            reqs.push(req);
        }
        reqs
    }

    #[fuchsia::test]
    async fn test_scene_uses_only_flatland2_solid_color_layers() {
        let (flatland_proxy, mut flatland_stream) =
            create_proxy_and_stream::<fland::FlatlandMarker>();

        let mut scene = SnakeScene::new(&flatland_proxy);
        scene.set_viewport_size(fmath::SizeU { width: 1080, height: 2400 });

        let mut game = SnakeGame::new(7);
        let head = *game.snake().front().unwrap();
        game.set_food_for_test(GridPos { x: 0, y: 0 });
        scene.render(&flatland_proxy, &game);

        let initial_reqs = drain_flatland_requests(&mut flatland_stream).await;
        let mut created_layers = 0;
        let mut created_stacks = 0;
        let mut stack_layer_updates = 0;
        let mut layer_property_updates = 0;
        let mut created_transforms = 0;

        for req in initial_reqs {
            match req {
                fland::FlatlandRequest::CreateLayer { .. } => created_layers += 1,
                fland::FlatlandRequest::CreateLayerStack { .. } => created_stacks += 1,
                fland::FlatlandRequest::SetStackLayers { .. } => stack_layer_updates += 1,
                fland::FlatlandRequest::SetLayerProperties { properties, .. } => {
                    if let Some(mode) = properties.composition_mode {
                        assert!(matches!(
                            mode,
                            fland::CompositionMode::SolidColor | fland::CompositionMode::Invisible
                        ));
                    }
                    layer_property_updates += 1;
                }
                fland::FlatlandRequest::CreateTransform { .. } => created_transforms += 1,
                fland::FlatlandRequest::SetRootTransform { .. }
                | fland::FlatlandRequest::AddChild { .. }
                | fland::FlatlandRequest::SetTransformContent { .. } => {}
                other => panic!("Unexpected Flatland request: {:?}", other),
            }
        }

        assert!(created_layers > 0);
        assert!(created_stacks > 0);
        assert_eq!(stack_layer_updates, created_stacks);
        assert!(layer_property_updates > 0);
        assert!(created_transforms < created_layers);

        // Case 1: A second render() with no game change emits zero SetLayerProperties calls.
        scene.render(&flatland_proxy, &game);
        let unchanged_reqs = drain_flatland_requests(&mut flatland_stream).await;
        assert!(
            unchanged_reqs.is_empty(),
            "Expected no FIDL calls on unchanged render, got {:?}",
            unchanged_reqs
        );

        // Case 2: Place food directly ahead of the head, step(), and render():
        // grows snake from 3 to 4 segments -> 1 CreateLayer and 1 SetStackLayers with 4 layers.
        game.set_food_for_test(GridPos { x: head.x + 1, y: head.y });
        assert!(game.step());
        scene.render(&flatland_proxy, &game);
        let grow_reqs = drain_flatland_requests(&mut flatland_stream).await;
        let grow_created_layers = grow_reqs
            .iter()
            .filter(|r| matches!(r, fland::FlatlandRequest::CreateLayer { .. }))
            .count();
        let grow_stack_layers: Vec<&Vec<fland::LayerId>> = grow_reqs
            .iter()
            .filter_map(|r| match r {
                fland::FlatlandRequest::SetStackLayers { layers, .. } => Some(layers),
                _ => None,
            })
            .collect();
        assert_eq!(grow_created_layers, 1);
        assert_eq!(grow_stack_layers.len(), 1);
        assert_eq!(grow_stack_layers[0].len(), 4);

        // Case 3: Reset the longer snake back to INITIAL_SNAKE_LEN (3) and render():
        // the 4th segment layer must be hidden via CompositionMode::Invisible.
        game.reset();
        game.set_food_for_test(GridPos { x: 0, y: 0 });
        scene.render(&flatland_proxy, &game);
        let reset_reqs = drain_flatland_requests(&mut flatland_stream).await;
        let invisible_updates = reset_reqs
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    fland::FlatlandRequest::SetLayerProperties {
                        properties: fland::LayerProperties {
                            composition_mode: Some(fland::CompositionMode::Invisible),
                            ..
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(invisible_updates, 1);
    }

    #[fuchsia::test]
    async fn test_dpad_hit_testing_portrait_and_landscape() {
        let (flatland_proxy, _flatland_stream) = create_proxy_and_stream::<fland::FlatlandMarker>();

        let mut scene = SnakeScene::new(&flatland_proxy);

        // Portrait viewport enables D-Pad hit rects immediately in set_viewport_size().
        scene.set_viewport_size(fmath::SizeU { width: 1080, height: 2400 });

        let up = scene.layout.dpad_up;
        let down = scene.layout.dpad_down;
        let left = scene.layout.dpad_left;
        let right = scene.layout.dpad_right;
        let center = scene.layout.dpad_center;

        assert_eq!(
            scene.hit_test_dpad((up.x + up.w / 2) as f32, (up.y + up.h / 2) as f32),
            Some(GameCommand::Move(Direction::Up))
        );
        assert_eq!(
            scene.hit_test_dpad((down.x + down.w / 2) as f32, (down.y + down.h / 2) as f32),
            Some(GameCommand::Move(Direction::Down))
        );
        assert_eq!(
            scene.hit_test_dpad((left.x + left.w / 2) as f32, (left.y + left.h / 2) as f32),
            Some(GameCommand::Move(Direction::Left))
        );
        assert_eq!(
            scene.hit_test_dpad((right.x + right.w / 2) as f32, (right.y + right.h / 2) as f32),
            Some(GameCommand::Move(Direction::Right))
        );
        assert_eq!(
            scene.hit_test_dpad((center.x + center.w / 2) as f32, (center.y + center.h / 2) as f32),
            Some(GameCommand::TogglePause)
        );
        assert_eq!(scene.hit_test_dpad(10.0, 10.0), None);

        // Landscape viewport disables D-Pad hit rects immediately in set_viewport_size().
        scene.set_viewport_size(fmath::SizeU { width: 1920, height: 1080 });
        assert_eq!(scene.hit_test_dpad((up.x + up.w / 2) as f32, (up.y + up.h / 2) as f32), None);
    }

    #[fuchsia::test]
    async fn test_dpad_layout_fits_within_viewport_and_below_board_border() {
        let (flatland_proxy, _flatland_stream) = create_proxy_and_stream::<fland::FlatlandMarker>();
        let mut scene = SnakeScene::new(&flatland_proxy);

        for (width, height) in [(360, 800), (412, 915), (408, 921), (500, 1200)] {
            scene.set_viewport_size(fmath::SizeU { width, height });
            let layout = scene.layout;
            assert!(layout.has_dpad, "D-Pad should be enabled on {width}x{height}");

            let board_border_bottom = layout.board_y + layout.board_px_h + layout.border_thickness;
            for (name, rect) in [
                ("up", layout.dpad_up),
                ("down", layout.dpad_down),
                ("left", layout.dpad_left),
                ("right", layout.dpad_right),
                ("center", layout.dpad_center),
            ] {
                assert!(rect.w > 0 && rect.h > 0, "{name} rect empty on {width}x{height}");
                assert!(
                    rect.x >= 0 && rect.x + rect.w <= layout.viewport_w,
                    "{name} rect {:?} overflows width {} on {width}x{height}",
                    rect,
                    layout.viewport_w
                );
                assert!(
                    rect.y > board_border_bottom && rect.y + rect.h <= layout.viewport_h,
                    "{name} rect {:?} overlaps board border ({board_border_bottom}) or overflows height {} on {width}x{height}",
                    rect,
                    layout.viewport_h
                );
            }
        }
    }

    #[fuchsia::test]
    async fn test_view_to_grid_accounts_for_nonzero_board_origin() {
        let (flatland_proxy, _flatland_stream) = create_proxy_and_stream::<fland::FlatlandMarker>();
        let mut scene = SnakeScene::new(&flatland_proxy);
        scene.layout.board_x = 50;
        scene.layout.board_y = 200;
        scene.layout.cell_size = 10;

        // With head at (10, 10) (center at grid (10.5, 10.5), view pixel (155, 305)):
        // A tap at view pixel (155, 250) is above the head in board space (grid y = 5.0 < 10.5),
        // whereas ignoring board_y=200 would treat y=250 as grid y = 25.0 > 10.5 (Down).
        let (gx, gy) = scene.view_to_grid(155.0, 250.0);
        assert_eq!((gx, gy), (10.5, 5.0));

        let mut game = SnakeGame::new(42);
        game.set_food_for_test(GridPos { x: 0, y: 0 });
        assert!(!game.handle_pointer_tap(gx, gy));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);
    }
}
