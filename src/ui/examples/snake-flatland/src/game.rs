// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::VecDeque;

const GRID_WIDTH: i32 = 20;
const GRID_HEIGHT: i32 = 20;
pub(crate) const INITIAL_SNAKE_LEN: i32 = 3;
const GRID_CELL_COUNT: usize = (GRID_WIDTH * GRID_HEIGHT) as usize;
const MAX_QUEUED_DIRS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridPos {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GameCommand {
    Move(Direction),
    TogglePause,
    Restart,
}

impl Direction {
    fn delta(self) -> (i32, i32) {
        match self {
            Direction::Up => (0, -1),
            Direction::Down => (0, 1),
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
        }
    }

    fn is_opposite(self, other: Direction) -> bool {
        matches!(
            (self, other),
            (Direction::Up, Direction::Down)
                | (Direction::Down, Direction::Up)
                | (Direction::Left, Direction::Right)
                | (Direction::Right, Direction::Left)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GameStatus {
    Playing,
    Paused,
    GameOver,
    Won,
}

/// Simple deterministic Xorshift64 PRNG so no external RNG crate is needed.
#[derive(Clone, Debug)]
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        // Xorshift stays at 0 forever from a zero state, so replace seed 0 with a non-zero constant.
        Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        if bound <= 1 { 0 } else { (self.next_u64() as usize) % bound }
    }
}

#[derive(Debug)]
pub(crate) struct SnakeGame {
    snake: VecDeque<GridPos>,
    direction: Direction,
    queued_directions: VecDeque<Direction>,
    food: GridPos,
    score: u32,
    high_score: u32,
    status: GameStatus,
    rng: Rng,
}

impl SnakeGame {
    pub(crate) fn new(seed: u64) -> Self {
        let mut game = Self {
            snake: VecDeque::with_capacity(GRID_CELL_COUNT),
            direction: Direction::Right,
            queued_directions: VecDeque::with_capacity(MAX_QUEUED_DIRS),
            food: GridPos { x: 0, y: 0 },
            score: 0,
            high_score: 0,
            status: GameStatus::Playing,
            rng: Rng::new(seed),
        };
        game.reset();
        game
    }

    pub(crate) const fn grid_dimensions() -> (i32, i32) {
        (GRID_WIDTH, GRID_HEIGHT)
    }

    pub(crate) fn snake(&self) -> &VecDeque<GridPos> {
        &self.snake
    }

    pub(crate) fn direction(&self) -> Direction {
        self.direction
    }

    pub(crate) fn food(&self) -> GridPos {
        self.food
    }

    #[cfg(test)]
    pub(crate) fn set_food_for_test(&mut self, food: GridPos) {
        self.food = food;
    }

    pub(crate) fn score(&self) -> u32 {
        self.score
    }

    pub(crate) fn high_score(&self) -> u32 {
        self.high_score
    }

    pub(crate) fn status(&self) -> GameStatus {
        self.status
    }

    pub(crate) fn reset(&mut self) {
        self.snake.clear();
        let center_x = GRID_WIDTH / 2;
        let center_y = GRID_HEIGHT / 2;
        for i in 0..INITIAL_SNAKE_LEN {
            self.snake.push_back(GridPos { x: center_x - i, y: center_y });
        }
        self.direction = Direction::Right;
        self.queued_directions.clear();
        self.score = 0;
        self.status = GameStatus::Playing;
        self.spawn_food();
    }

    /// Toggles pause or resets if the game has ended. Returns `true` as visual state always
    /// changes.
    pub(crate) fn toggle_pause(&mut self) -> bool {
        match self.status {
            GameStatus::Playing => self.status = GameStatus::Paused,
            GameStatus::Paused => self.status = GameStatus::Playing,
            GameStatus::GameOver | GameStatus::Won => self.reset(),
        }
        true
    }

    /// Enqueues a direction turn. Returns `true` if immediate visual state changed (e.g., reset or
    /// unpause), or `false` otherwise, whether the turn was queued or ignored.
    pub(crate) fn queue_direction(&mut self, next_dir: Direction) -> bool {
        if matches!(self.status, GameStatus::GameOver | GameStatus::Won) {
            self.reset();
            return true;
        }
        let mut visual_changed = false;
        if self.status == GameStatus::Paused {
            self.status = GameStatus::Playing;
            visual_changed = true;
        }
        if self.queued_directions.len() < MAX_QUEUED_DIRS {
            let reference_dir = self.queued_directions.back().copied().unwrap_or(self.direction);
            if next_dir != reference_dir && !reference_dir.is_opposite(next_dir) {
                self.queued_directions.push_back(next_dir);
            }
        }
        visual_changed
    }

    /// Steers the snake based on a pointer tap at `(grid_x, grid_y)` in fractional grid cell
    /// coordinates. Returns `true` if immediate visual state changed (e.g., reset or unpause).
    pub(crate) fn handle_pointer_tap(&mut self, grid_x: f32, grid_y: f32) -> bool {
        if matches!(self.status, GameStatus::GameOver | GameStatus::Won) {
            self.reset();
            return true;
        }

        let Some(head) = self.snake.front() else {
            return false;
        };
        let head_center_x = head.x as f32 + 0.5;
        let head_center_y = head.y as f32 + 0.5;
        let dx = grid_x - head_center_x;
        let dy = grid_y - head_center_y;

        let current_dir = self.queued_directions.back().copied().unwrap_or(self.direction);
        let desired = match current_dir {
            Direction::Left | Direction::Right => {
                if dy < 0.0 {
                    Direction::Up
                } else {
                    Direction::Down
                }
            }
            Direction::Up | Direction::Down => {
                if dx < 0.0 {
                    Direction::Left
                } else {
                    Direction::Right
                }
            }
        };
        self.queue_direction(desired)
    }

    /// Advances the game by one tick. Returns `true` if the visual state changed.
    pub(crate) fn step(&mut self) -> bool {
        if self.status != GameStatus::Playing {
            return false;
        }

        if let Some(next_dir) = self.queued_directions.pop_front() {
            if !self.direction.is_opposite(next_dir) {
                self.direction = next_dir;
            }
        }

        let Some(&head) = self.snake.front() else {
            return false;
        };
        let (dx, dy) = self.direction.delta();
        let next_head = GridPos { x: head.x + dx, y: head.y + dy };

        // Check wall collision.
        if next_head.x < 0
            || next_head.x >= GRID_WIDTH
            || next_head.y < 0
            || next_head.y >= GRID_HEIGHT
        {
            self.status = GameStatus::GameOver;
            return true;
        }

        let eating = next_head == self.food;

        // Check self-collision. If not eating, the current tail cell will be vacated this tick.
        let check_len = if eating { self.snake.len() } else { self.snake.len().saturating_sub(1) };
        if self.snake.iter().take(check_len).any(|&pos| pos == next_head) {
            self.status = GameStatus::GameOver;
            return true;
        }

        self.snake.push_front(next_head);

        if eating {
            self.score += 1;
            if self.score > self.high_score {
                self.high_score = self.score;
            }
            if self.snake.len() >= GRID_CELL_COUNT {
                self.status = GameStatus::Won;
            } else {
                self.spawn_food();
            }
        } else {
            self.snake.pop_back();
        }

        true
    }

    fn spawn_food(&mut self) {
        let mut occupied = [false; GRID_CELL_COUNT];
        let mut occupied_count = 0usize;
        for pos in &self.snake {
            if pos.x >= 0 && pos.x < GRID_WIDTH && pos.y >= 0 && pos.y < GRID_HEIGHT {
                let idx = (pos.y * GRID_WIDTH + pos.x) as usize;
                if !occupied[idx] {
                    occupied[idx] = true;
                    occupied_count += 1;
                }
            }
        }

        let empty_count = GRID_CELL_COUNT.saturating_sub(occupied_count);
        if empty_count == 0 {
            return;
        }

        let mut target = self.rng.next_usize(empty_count);
        for (idx, &is_occupied) in occupied.iter().enumerate() {
            if !is_occupied {
                if target == 0 {
                    let x = (idx as i32) % GRID_WIDTH;
                    let y = (idx as i32) / GRID_WIDTH;
                    self.food = GridPos { x, y };
                    return;
                }
                target -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_initial_state() {
        let game = SnakeGame::new(42);
        assert_eq!(game.status(), GameStatus::Playing);
        assert_eq!(game.snake().len(), INITIAL_SNAKE_LEN as usize);
        assert_eq!(game.direction(), Direction::Right);
        assert_eq!(game.score(), 0);
        assert!(!game.snake().contains(&game.food()));
    }

    #[fuchsia::test]
    fn test_step_moves_snake_and_turn_queue_rules() {
        let mut game = SnakeGame::new(42);
        let initial_head = *game.snake().front().unwrap();
        game.food = GridPos { x: 0, y: 0 };

        // Heading Right: queuing Left (opposite) is ignored, then Up is accepted; one step gives Up.
        assert!(!game.queue_direction(Direction::Left));
        assert!(!game.queue_direction(Direction::Up));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);
        assert_eq!(
            *game.snake().front().unwrap(),
            GridPos { x: initial_head.x, y: initial_head.y - 1 }
        );

        // Reset to heading Right; queue Up, duplicate Up (ignored), then Left; two steps give Left.
        game.reset();
        game.food = GridPos { x: 0, y: 0 };
        assert!(!game.queue_direction(Direction::Up));
        assert!(!game.queue_direction(Direction::Up));
        assert!(!game.queue_direction(Direction::Left));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Left);

        // Reset to heading Right; queue Up, Left, then Down (dropped because queue cap is 2);
        // three steps leave the snake heading Left.
        game.reset();
        game.food = GridPos { x: 0, y: 0 };
        assert!(!game.queue_direction(Direction::Up));
        assert!(!game.queue_direction(Direction::Left));
        assert!(!game.queue_direction(Direction::Down));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Left);
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Left);
    }

    #[fuchsia::test]
    fn test_eating_food_grows_snake_and_increments_score() {
        let mut game = SnakeGame::new(42);
        let head = *game.snake().front().unwrap();
        game.food = GridPos { x: head.x + 1, y: head.y };
        let prev_len = game.snake().len();

        assert!(game.step());
        assert_eq!(game.snake().len(), prev_len + 1);
        assert_eq!(game.score(), 1);
        assert_eq!(game.high_score(), 1);
        assert!(!game.snake().contains(&game.food()));
    }

    #[fuchsia::test]
    fn test_wall_collision_triggers_game_over_on_all_four_walls() {
        let cases = [
            ("right", GridPos { x: GRID_WIDTH - 1, y: 10 }, Direction::Right),
            ("left", GridPos { x: 0, y: 10 }, Direction::Left),
            ("top", GridPos { x: 10, y: 0 }, Direction::Up),
            ("bottom", GridPos { x: 10, y: GRID_HEIGHT - 1 }, Direction::Down),
        ];

        for (name, head_pos, dir) in cases {
            let mut game = SnakeGame::new(42);
            game.snake = VecDeque::from(vec![head_pos]);
            game.direction = dir;
            game.queued_directions.clear();

            assert!(game.step(), "wall case `{name}` should report state change");
            assert_eq!(game.status(), GameStatus::GameOver, "wall case `{name}` should end game");
            assert_eq!(
                *game.snake().front().unwrap(),
                head_pos,
                "wall case `{name}` should not move head outside board"
            );
        }
    }

    #[fuchsia::test]
    fn test_self_collision_and_tail_cell_vacation() {
        let mut game = SnakeGame::new(42);
        // 4-cell loop where head at (5, 5) is adjacent to tail at (5, 6).
        let loop_snake = VecDeque::from(vec![
            GridPos { x: 5, y: 5 },
            GridPos { x: 4, y: 5 },
            GridPos { x: 4, y: 6 },
            GridPos { x: 5, y: 6 },
        ]);

        // Moving into the tail cell when NOT eating is valid because the tail vacates (5, 6).
        game.snake = loop_snake.clone();
        game.direction = Direction::Right;
        game.food = GridPos { x: 0, y: 0 };
        game.queue_direction(Direction::Down);
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::Playing);
        assert_eq!(*game.snake().front().unwrap(), GridPos { x: 5, y: 6 });

        // Moving into the tail cell when food IS there causes self-collision because the tail stays.
        game.snake = loop_snake;
        game.direction = Direction::Right;
        game.food = GridPos { x: 5, y: 6 };
        game.queue_direction(Direction::Down);
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::GameOver);
    }

    #[fuchsia::test]
    fn test_pause_and_game_over_ignore_step_and_turn_resumes() {
        let mut game = SnakeGame::new(42);
        let initial_snake = game.snake().clone();

        assert!(game.toggle_pause());
        assert_eq!(game.status(), GameStatus::Paused);
        assert!(!game.step());
        assert_eq!(game.snake(), &initial_snake);

        // Queuing a turn while paused resumes immediately and applies the turn on the next step.
        assert!(game.queue_direction(Direction::Up));
        assert_eq!(game.status(), GameStatus::Playing);
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);

        // Stepping after GameOver returns false and does not move.
        game.snake = VecDeque::from(vec![GridPos { x: GRID_WIDTH - 1, y: 5 }]);
        game.direction = Direction::Right;
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::GameOver);
        assert!(!game.step());
    }

    #[fuchsia::test]
    fn test_restart_methods_preserve_high_score() {
        let mut game = SnakeGame::new(42);
        // Eat twice to reach score 2 and high_score 2.
        for _ in 0..2 {
            let head = *game.snake().front().unwrap();
            game.food = GridPos { x: head.x + 1, y: head.y };
            assert!(game.step());
        }
        assert_eq!(game.score(), 2);
        assert_eq!(game.high_score(), 2);

        // Hit wall -> GameOver, then restart via toggle_pause().
        game.snake = VecDeque::from(vec![GridPos { x: GRID_WIDTH - 1, y: 5 }]);
        game.direction = Direction::Right;
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::GameOver);
        assert!(game.toggle_pause());
        assert_eq!(game.status(), GameStatus::Playing);
        assert_eq!(game.snake().len(), INITIAL_SNAKE_LEN as usize);
        assert_eq!(game.score(), 0);
        assert_eq!(game.high_score(), 2);

        // Hit wall -> GameOver, then restart via queue_direction().
        game.snake = VecDeque::from(vec![GridPos { x: GRID_WIDTH - 1, y: 5 }]);
        game.direction = Direction::Right;
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::GameOver);
        assert!(game.queue_direction(Direction::Up));
        assert_eq!(game.status(), GameStatus::Playing);
        assert_eq!(game.snake().len(), INITIAL_SNAKE_LEN as usize);
        assert_eq!(game.score(), 0);
        assert_eq!(game.high_score(), 2);

        // Hit wall -> GameOver, then restart via handle_pointer_tap().
        game.snake = VecDeque::from(vec![GridPos { x: GRID_WIDTH - 1, y: 5 }]);
        game.direction = Direction::Right;
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::GameOver);
        assert!(game.handle_pointer_tap(5.0, 5.0));
        assert_eq!(game.status(), GameStatus::Playing);
        assert_eq!(game.snake().len(), INITIAL_SNAKE_LEN as usize);
        assert_eq!(game.score(), 0);
        assert_eq!(game.high_score(), 2);

        // Eating once more brings score to 1 and keeps high_score at 2.
        let head = *game.snake().front().unwrap();
        game.food = GridPos { x: head.x + 1, y: head.y };
        assert!(game.step());
        assert_eq!(game.score(), 1);
        assert_eq!(game.high_score(), 2);
    }

    #[fuchsia::test]
    fn test_spawn_food_on_last_cell_and_winning_board() {
        let mut game = SnakeGame::new(42);
        let empty_cell = GridPos { x: 3, y: 17 };
        game.snake.clear();
        for y in 0..GRID_HEIGHT {
            for x in 0..GRID_WIDTH {
                let pos = GridPos { x, y };
                if pos != empty_cell {
                    game.snake.push_back(pos);
                }
            }
        }
        game.spawn_food();
        assert_eq!(game.food(), empty_cell);

        // Put head adjacent to empty_cell facing it with food there; one step wins the game.
        let target = GridPos { x: 5, y: 5 };
        let start_head = GridPos { x: 4, y: 5 };
        game.snake.clear();
        game.snake.push_back(start_head);
        for y in 0..GRID_HEIGHT {
            for x in 0..GRID_WIDTH {
                let pos = GridPos { x, y };
                if pos != target && pos != start_head {
                    game.snake.push_back(pos);
                }
            }
        }
        game.direction = Direction::Right;
        game.queued_directions.clear();
        game.food = target;
        assert!(game.step());
        assert_eq!(game.status(), GameStatus::Won);
    }

    #[fuchsia::test]
    fn test_handle_pointer_tap_steers_all_directions_and_unpauses() {
        let mut game = SnakeGame::new(42);
        game.food = GridPos { x: 0, y: 0 };

        // Snake starts at (10, 10) moving Right; tapping above y=10.5 queues Up.
        assert!(!game.handle_pointer_tap(10.5, 2.0));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);

        // Now moving Up at (10, 9); tapping left of x=10.5 queues Left.
        assert!(!game.handle_pointer_tap(2.0, 9.5));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Left);

        // Now moving Left at (9, 9); tapping below y=9.5 queues Down.
        assert!(!game.handle_pointer_tap(9.5, 15.0));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Down);

        // Now moving Down at (9, 10); tapping right of x=9.5 queues Right.
        assert!(!game.handle_pointer_tap(15.0, 10.5));
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Right);

        // Pausing then tapping unpauses immediately AND queues the perpendicular turn.
        assert!(game.toggle_pause());
        assert_eq!(game.status(), GameStatus::Paused);
        assert!(game.handle_pointer_tap(10.5, 2.0));
        assert_eq!(game.status(), GameStatus::Playing);
        assert!(game.step());
        assert_eq!(game.direction(), Direction::Up);
    }
}
