# display-tool - Display Driver Testing & Verification Tool

Command-line testing utility for Fuchsia display drivers.

## Overview & Architecture

The tool's design prioritizes minimizing the System Under Test (SUT) size.

Key decisions:

* Use the `fuchsia.hardware.display/Coordinator` FIDL interface directly.
* Use the testing client priority, so the tool takes over the display from any
  active product session.
* Support every configuration that includes the display drivers stack.

## Build configuration

`display-tool` is included in the `tools` bundle for the display drivers stack.

```posix-terminal
fx add-test //src/graphics/display:tools
```

If you're iterating on the tool, include the display drivers' `tests` bundle.

```posix-terminal
fx add-test //src/graphics/display:tests
```

Use `balanced` or `release` builds for frame rate testing. The current scene
rendering logic can bottleneck the CPU in `debug`.

## Running

Recommended method:

```posix-terminal
ffx target ssh -- display-tool <command> [options]
```

To terminate continuous animation or vsync monitoring commands, send `SIGINT`
(Ctrl+C).

### Selecting a display mode

By default, every command uses the display's preferred mode (mode 0 in `info`).
The top-level `--refresh-rate <hz>` option goes before the command. It selects
the mode whose refresh rate is closest to `<hz>`, among the modes with the
preferred mode's resolution, and prints the selected mode. It also prints a
warning if the selected mode's rate is more than 2% away from `<hz>`.

```posix-terminal
ffx target ssh -- display-tool --refresh-rate 60 frame-rate-test
```

The option applies to the command's display (the first display if the command
does not name one). The mode takes effect when the command commits its first
configuration, so `info` ignores it. `frame-rate-test` prints the
selected mode's rate as its expected frame rate.

---

## Intended use for display testing

### Step 1: Display detection and timings

Test that the display drivers detect connected panels and register the correct
display modes and pixel formats.

```posix-terminal
ffx target ssh -- display-tool info
```

#### Verification

Add the `--fidl` flag to inspect the raw FIDL structure.

Checklist:

* At least one display is reported.
* Reported resolution matches the panel or emulator specifications.
* Supported pixel formats include expected types.
* Vertical refresh rates match specifications.
  Example: `60000 mHz` (in millihertz) for 60 Hz.

### Step 2: One solid color fill layer

Test the driver's ability to handle solid color fill layers. This test does not
depend on Sysmem integration or IOMMU configuration.

```posix-terminal
ffx target ssh -- display-tool color --color ff0000
```

#### Verification

* The display is filled with the requested color. Example: `ff0000` is pure red.
* The terminal outputs live refresh rate statistics:
  `Display 1 config is applied, refresh rate 60.00 Hz (16.66667 ms)`
* Vsync events arrive at a steady rate.

### Step 3: One image layer

Test that the display driver integrates with Sysmem and manages its IOMMU
correctly.

```posix-terminal
ffx target ssh -- display-tool vsync --color 00ff00 --pixel-format bgra32
```

#### Verification

* The display is filled with the chosen color. Example: `00ff00` is pure green.
* The terminal shows a stable VSync frequency. No dropped event warnings.
* System logs do not include any Sysmem buffer negotiation constraint errors.

### Step 4: Swapchain with double-buffering

Test that the display driver submits a sequence of display configurations
correctly to the hardware.

```posix-terminal
ffx target ssh -- display-tool squares
```

#### Expected visuals

Four colored squares animate and bounce off the display borders against a black
background.

* **Orange square** (`#ff6400`): starts at top-left
* **Fuchsia square** (`#ff00ff`): starts at top-right
* **Green square** (`#64ff00`): starts at bottom-left
* **Blue square** (`#0064ff`): starts at bottom-right

#### Verification

* Smooth animation. No screen tearing, jitter, or display corruption.
* The terminal outputs continuous FPS updates matching the display refresh rate.
  Example: `Display 60.00 fps (16.66667 ms) render 2.104 ms clean 1.302 ms`.
  `render` is the CPU time spent drawing a frame, and `clean` is the CPU time
  spent cleaning caches so the frame reaches memory. All values are moving
  averages over recent frames.
* No `Timed out while waiting` output. See `--vsync-timeout-ms <ms>` for tuning
  the timeout value.

### Step 5: Display compositing (multiple layers)

Test the display engine driver's support for multi-layer hardware composition,
layer z-ordering, and hardware alpha blending modes.

Start with two layers, to test basic blender configuration.

```posix-terminal
ffx target ssh -- display-tool multilayer-squares --layer-count 2
```

Follow-up with a test covering the maximum layer count supported by the
hardware, to catch DRAM bandwidth configuration issues.

#### Expected visuals

Each layer displays one square. The layers are described below, from the bottom
of the Z-order to the top.

* **Bottom layer**: Bouncing **fuchsia square** (`#ff00ff`), four times as large
  as the smaller square described below. Fully opaque (alpha blending
  disabled).
* Bouncing **green square** (`#00ff64`), semi-transparent (alpha set to 150).
  Premultiplied alpha blending.
* Bouncing **blue square** (`#0064ff`), 4x size, fully opaque.
* Bouncing **orange square** (`#ff6400`), semi-transparent (alpha set to 150).
  Premultiplied alpha blending.
* Bouncing **cyan square** (`#00ffff`), 4x size, fully opaque.
* Bouncing **yellow square** (`#ffff00`), semi-transparent (alpha set to 150).
  Premultiplied alpha blending.
* Bouncing **white square** (`#ffffff`), 4x size, fully opaque.
* Bouncing **gray square** (`#808080`), semi-transparent (alpha set to 150).
  Premultiplied alpha blending.

#### Verification

* The semi-transparent green square blends correctly with the bottom layer.
* The squares on the lower layers show through the transparent areas of the
  upper layers.
* The terminal outputs a steady frame rate.

### Step 6: Frame rate

Test that the display engine does not drop or skip committed frames.

```posix-terminal
ffx target ssh -- display-tool frame-rate-test
```

#### Expected visuals

10x6 rectangular grid. A single colored cell traverses every grid location
sequentially following a continuous Hamiltonian cycle.

Cell color:

* **First loop frame**: **Blue** (`#0000ff`).
* **Intermediate frames**: **White** (`#ffffff`).
* **Final frame of loop**: **Red** (`#ff0000`).

#### Verification

* The terminal outputs the selected display mode rate.
  Example:  `Expected frame rate: 60.000 fps`.
* The moving cell advances smoothly by exactly one position per frame. No
  skipped cells. No stuttering.

### Step 7: Power cycling while submitting frames

Test that the display driver handles submitted configurations while the display
hardware is powered on and off.

```posix-terminal
ffx target ssh -- display-tool squares --power-cycle-frame-count 300 \
    --power-cycle-off-frame-count 60
```

#### Expected visuals

See the swapchain section above for a description of the `squares` visual
output. Additionally, the display powers off and back on every few seconds.

#### Verification

See the swapchain section above for verifying `squares`. The conditions
described there must continue to hold after power cycling.

Additional checks:

* Every `SetDisplayPowerMode` line reports `-> Ok(Ok(()))`.
* After each power on, a `first vsync after power on` line appears.

### Step 8: Refresh cadence

Test that the display driver delivers vsync events at the selected mode's rate,
and shows each submitted frame promptly, whatever the client's frame rate. This
step matters most for displays with several refresh rates or a variable refresh
rate.

```posix-terminal
ffx target ssh -- display-tool --refresh-rate 120 cadence --fps 24 \
    --burst-frames 48 --idle-ms 3000 --duration-s 20 --verbose
```

`cadence` presents frames on a timer that does not depend on vsync, and records
every vsync event. Before the measurement starts, it presents a black warm-up
frame and waits until a vsync reports it, plus one more vsync. So a mode switch
that `--refresh-rate` causes, and the display pipeline's start-up, stay out of
the measurement. Each frame is rendered as soon as its image is free, and
committed when it is due. Options:

* `--fps <f>`: frame rate. Defaults to 60. `0` presents one frame and then
  idles, which shows the vsync cadence of a static screen.
* `--burst-frames <n> --idle-ms <ms>`: present `n` frames, pause for `ms`
  milliseconds (more than 0), and repeat. If frames fall behind the schedule, a
  burst starts `ms` milliseconds after the last frame of the previous burst was
  committed, rather than back to back with it, and the rest of the schedule
  moves with it.
* `--duration-s <s>`: length of the measurement. Defaults to 10.
* `--verbose`: also report every second. Each report covers the second that
  just ended. A frame counts as shown in the report of the second it was
  presented in, so a frame whose first vsync comes after the report counts as
  not shown there, but as shown in the final report.
* `--log-vsync`: print a line for every vsync event, with its time, the interval
  since the previous vsync, the displayed config stamp, and the frame it shows.

The final report includes:

* the vsync count, the number of vsyncs with a stale config stamp (0, or lower
  than an earlier one), and the mean vsync rate;
* the minimum, median, 90th percentile and maximum vsync interval;
* a histogram of vsync intervals, with buckets that separate common refresh
  rates;
* the number of frames presented, shown (reported by a vsync), and late
  (committed more than half a frame period after their scheduled time, because
  the display had not shown a later frame yet, so the frame's image was not
  free, or because rendering the frame did not finish in time);
* the minimum, median, 90th percentile and maximum latency from committing a
  frame to the first vsync that reports it.

The same values are printed on one `cadence_summary` line (`cadence_window`
for `--verbose` reports) of space-separated `key=value` pairs, for scripts.
Missing values are `nan`.

#### Expected visuals

The same 10x6 grid as the frame rate test. The moving cell's color changes every
frame, cycling through red, green, blue, yellow, cyan, magenta and white. A bar
below the grid shows the same color.

#### Verification

* In a fixed-rate mode, vsync intervals cluster around the mode's period.
* With a variable refresh rate whose vsyncs follow the panel's refresh, vsync
  intervals may stretch while no frames are presented. Then, the first vsync
  after a frame is submitted arrives within about one period of the fastest
  rate. A mode whose vsyncs come from a fixed-rate tearing effect signal, such
  as the adaptive refresh mode of a command-mode panel (120 Hz), keeps a fixed
  vsync interval even when the panel itself refreshes more slowly.
* `frames_shown` is close to `frames_presented` when `--fps` is clearly below
  the selected mode's refresh rate. Near the refresh rate, the phase between the
  frame timer and vsync drifts, and some frames are replaced before a vsync
  reports them. Check the rate the tool printed when it selected the mode: for
  example, `--refresh-rate 60` may select a 58.897 Hz mode, which is below the
  default `--fps 60`. The frame committed last may also still be waiting for its
  vsync when the measurement ends.
* `vsyncs_stale` is 0.
* The moving cell advances by one position per frame, and the color changes on
  every frame.

