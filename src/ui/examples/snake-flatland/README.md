# Snake Flatland App

A Snake game built purely with Fuchsia `Flatland2` solid-color layers
(`CreateLayer`, `SetLayerProperties` with `CompositionMode::SolidColor`,
`CreateLayerStack`, and `SetStackLayers`), without any extra drawing libraries
or image buffers.

## Controls

- **Keyboard**:
  - `Arrow Keys` or `W` / `A` / `S` / `D`: Steer the snake
  - `Space` or `P`: Pause / Resume
  - `Enter` or `R`: Restart
- **Touch / Mouse**:
  - Use the on-screen D-Pad buttons, swipe in any direction, or tap/click
    relative to the snake's head to turn perpendicular to its current heading
    (or tap on Game Over / Victory to restart).

## Running

Add as a session element with an explicit `--name` so it can be easily removed later:

```sh
ffx session add --name snake-flatland fuchsia-pkg://fuchsia.com/flatland-examples#meta/snake-flatland.cm
```

To stop and remove the element from the session:

```sh
ffx session remove snake-flatland
```

