# Fuchsia UI examples

This directory contains sample applications demonstrating how to build graphical
components on Fuchsia using Scenic's Flatland composition APIs, `sysmem2`
buffer allocation, Vulkan / Escher rendering, screen capture, and pointer /
keyboard input handling.

## Quick comparison

| Example | Language | Composition API | Rendering Backend | Scene Attachment | Input & Extra APIs |
| :--- | :--- | :--- | :--- | :--- | :--- |
| [`snake-flatland`](snake-flatland/) | Rust (2024) | `Flatland2` (`FlatlandFactory`) | Solid-color `Layer`s (no buffers) | `GraphicalPresenter` | Touch (swipes + taps), Mouse, Keyboard (`input3`), `Focuser` |
| [`simplest-app-flatland`](simplest-app-flatland/) | Rust (2024) | Classic `Flatland` | Single `CreateFilledRect` | `GraphicalPresenter` or `ViewProvider` | Touch, Mouse, Keyboard (`input3`), `Focuser`, Tracing |
| [`flatland-rainbow`](flatland-rainbow/) | Rust (2024) | Classic `Flatland` | CPU (`sysmem2`) or Vulkan (`ash`) | `ViewProvider` or `GraphicalPresenter` | Touch, Mouse (logged), `ViewRefFocused`, `ThroughputScheduler`, Tracing |
| [`simplest_sysmem`](simplest_sysmem/) | C++ (HLCPP) | Classic `Flatland` | CPU (`sysmem2` VMO / `libpng`) or `CreateFilledRect` | `ViewProvider` | Packaged PNG decoding (`libpng`) |
| [`screen_recording`](screen_recording/) | C++ (Natural / Wire) | Classic `Flatland` | Zero-copy `ScreenCapture` `sysmem2` + `CreateFilledRect` | `ViewProvider` | `fuchsia.ui.composition.internal.ScreenCapture` |
| [`escher-flatland`](escher-flatland/) | C++ (Natural) | Classic `Flatland` (`escher_flatland`) | Escher (Vulkan) | `GraphicalPresenter` | Minimal `escher_flatland` integration |
| [`escher`](escher/) (`rainfall`) | C++ (Natural) | `VkImagePipeSurfaceFUCHSIA` (swapchain layer) | Escher `RectangleCompositor` | `GraphicalPresenter` (via layer) or raw display | Escher built-in flatland shaders, `DemoHarness` (`_on_scenic` / `_on_fb`) |

## Which example should I try?

| Use case | Recommended example | Rendering strategy | Input capabilities |
| :--- | :--- | :--- | :--- |
| I want to exercise `Flatland2` (layers and layer stacks) or write a modern Rust UI app | [`snake-flatland`](snake-flatland/) | Solid-color `Flatland2` `Layer`s & `LayerStack`s (`CompositionMode::SolidColor`, no buffers) | Touch (taps transformed via `ViewParameters`; swipes via viewport-space deltas), Mouse (`ViewParameters` transform), Keyboard (`input3`), `Focuser` |
| I want to compare `GraphicalPresenter` vs. `ViewProvider` in a minimal Rust skeleton | [`simplest-app-flatland`](simplest-app-flatland/) | Single classic `Flatland` solid rectangle (`CreateFilledRect` / `SetSolidFill`) | Touch, Mouse, Keyboard (`input3`), `Focuser` |
| I want to exercise CPU software rendering or raw Vulkan (`ash`) with `sysmem2` in Rust | [`flatland-rainbow`](flatland-rainbow/) | Dual backends: CPU software rendering into `sysmem2` VMOs or raw Vulkan (`ash`) with `VK_FUCHSIA_buffer_collection` | Touch, Mouse (logged to syslog); `ViewRefFocused` (adjusts color saturation) |
| I want to allocate host-visible `sysmem2` buffers or decode PNGs in C++ | [`simplest_sysmem`](simplest_sysmem/) | CPU-writable `sysmem2` VMO uploads (`--png` via `libpng`, `--block` BGRA32) or `CreateFilledRect` (`--rect`) | None (output-only) |
| I want to capture screen frames in C++ (Natural / Wire bindings) | [`screen_recording`](screen_recording/) | Zero-copy `sysmem2` buffer collection populated by Scenic (`ScreenCapture::GetNextFrame`, `kNoneUsage`) composited via `Flatland::CreateImage` alongside a `CreateFilledRect` bouncing square | None (output-only) |
| I want to exercise GPU rendering with Escher in C++ (minimal Flatland setup) | [`escher-flatland`](escher-flatland/) | Vulkan GPU rendering via `escher::Escher` presented through `//src/ui/lib/escher/escher_flatland` | None (output-only) |
| I want to exercise Escher `RectangleCompositor` with built-in flatland shaders via Vulkan swapchain layers (Scenic or direct display FB) | [`escher`](escher/) (`rainfall`) | Vulkan GPU rendering via `escher::RectangleCompositor` + Escher built-in flatland shaders over `VkImagePipeSurfaceFUCHSIA` (`VK_LAYER_FUCHSIA_imagepipe_swapchain` or `_fb`) | None on Fuchsia (`/dev/class/input-report` stubbed in `DemoHarness`) |

## Example catalog

### snake-flatland
* **Path:** [`//src/ui/examples/snake-flatland`](snake-flatland/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/snake-flatland.cm`
* **Description:** A complete, playable Snake game built purely from
  `Flatland2` `CompositionMode::SolidColor` layers—no `sysmem` buffer
  allocations or texture uploads required. Demonstrates `FlatlandFactory`
  session creation with `use_flatland2: true`, 7-segment score rendering with
  solid layers, responsive viewport layout via `ParentViewportWatcher`,
  on-screen D-pad hit testing, tap and mouse click handling with
  `ViewParameters.viewport_to_view_transform` coordinate conversion (plus
  swipe detection using viewport-space deltas against `SWIPE_THRESHOLD_PX`),
  and keyboard/focus integration.

### simplest-app-flatland
* **Path:** [`//src/ui/examples/simplest-app-flatland`](simplest-app-flatland/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/simplest-app-flatland.cm`
    (passes `graphical_presenter`)
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/simplest-app-flatland-session.cm`
    (passes `view_provider`)
* **Description:** The smallest interactive classic `Flatland` application in
  Rust. Renders a single solid-colored rectangle (`CreateFilledRect` /
  `SetSolidFill`) that changes color on each touch, mouse, or keyboard event,
  while cleanly isolating `TouchSource`, `MouseSource`, and `KeyboardListener`
  event loops and showing both `GraphicalPresenter` and `ViewProvider` startup
  paths.

### flatland-rainbow
* **Path:** [`//src/ui/examples/flatland-rainbow`](flatland-rainbow/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/flatland-rainbow.cm`
    (CPU software renderer)
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/flatland-rainbow-vulkan.cm`
    (passes `--use-vulkan` and `--use-graphical-presenter`)
* **Description:** Side-by-side CPU (`render_cpu.rs`) and Vulkan
  (`render_vk.rs` via `ash`) rendering backends sharing a single Flatland
  presentation loop. Shows how to negotiate `fuchsia.sysmem2` buffer
  collections, define a custom `ash` extension loader for
  `VK_FUCHSIA_buffer_collection` (importing memory via
  `VK_FUCHSIA_external_memory`; `VK_FUCHSIA_external_semaphore` is currently
  commented out in `render_vk.rs` where `VulkanRenderer::render_rgba` submits
  with a `vk::Fence` and waits on the CPU via `wait_for_fences` rather than
  importing external semaphores or passing fences to `Flatland::Present`),
  pace frames with `flatland_frame_scheduling_lib::ThroughputScheduler`
  (`OnNextFrameBegin` / `OnFramePresented`), adjust color saturation based on
  focus state via `fuchsia.ui.views.ViewRefFocused`, and log touch/mouse
  events to syslog rather than mutating the scene. *(Note: Vulkan mode
  currently renders a single solid color rather than four quadrants; see
  `b/42055867`.)*

### simplest_sysmem
* **Path:** [`//src/ui/examples/simplest_sysmem`](simplest_sysmem/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/simplest_sysmem#meta/simplest_sysmem.cm`
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/simplest_sysmem.cm`
* **Description:** A focused C++ reference for host-visible `sysmem2` VMO
  allocation and CPU pixel writes without Vulkan. Supports three modes via
  component program `args`: `--png` (decodes a packaged PNG with `libpng` into
  RGBA pixels), `--block` (generates four BGRA32 color blocks), and `--rect`
  (displays a solid `CreateFilledRect`).

### screen_recording
* **Path:** [`//src/ui/examples/screen_recording`](screen_recording/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/screen_recording#meta/screen_recording.cm`
  * `fuchsia-pkg://fuchsia.com/flatland-examples#meta/screen_recording.cm`
* **Description:** Demonstrates live screen capture via
  `fuchsia.ui.composition.internal.ScreenCapture` (`view_provider.cc` uses
  C++ Natural bindings, while `screen_capture_helper.cc` also uses
  `fidl::WireClient<fuchsia_sysmem2::Allocator>`). Rather than mapping or
  writing to `sysmem2` buffers on the CPU, Scenic
  (`ScreenCapture::GetNextFrame`) writes directly into the shared `sysmem2`
  buffer collection (`kNoneUsage`,
  `RegisterBufferCollectionUsages::kDefault | kScreenshot`), and the app
  composites those buffers zero-copy via `Flatland::CreateImage` alongside a
  `CreateFilledRect` bouncing square.

### escher-flatland
* **Path:** [`//src/ui/examples/escher-flatland`](escher-flatland/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/escher-flatland#meta/escher-flatland.cm`
* **Description:** A minimal C++ entry point showing how to bootstrap
  `escher::Escher`, clear the swapchain image, and blit a bouncing digital
  clock string via `escher::DebugFont::Blit` into a child swapchain viewport
  managed by `//src/ui/lib/escher/escher_flatland` (`escher::EscherFlatland`).

### escher (rainfall)
* **Path:** [`//src/ui/examples/escher`](escher/)
* **Components:**
  * `fuchsia-pkg://fuchsia.com/escher_rainfall_on_scenic#meta/escher_rainfall_on_scenic.cm`
  * `fuchsia-pkg://fuchsia.com/escher_rainfall_on_fb#meta/escher_rainfall_on_fb.cm`
* **Description:** An Escher demo (`DemoHarness` supporting Scenic and
  direct display framebuffer access) rendering animated falling rectangles
  with `escher::RectangleCompositor` and Escher's built-in flatland shaders
  packaged from `//src/ui/lib/escher/shaders/flatland/` (rather than custom
  GLSL shaders). Rather than calling Flatland FIDL APIs directly in the app,
  `DemoHarnessFuchsia` creates a `VkImagePipeSurfaceFUCHSIA` and relies on
  `VK_LAYER_FUCHSIA_imagepipe_swapchain` (Scenic mode) or
  `VK_LAYER_FUCHSIA_imagepipe_swapchain_fb` (direct display framebuffer
  mode). Accompanied by an automated Vulkan smoke test
  (`//src/ui/examples/escher/test:escher_examples_test`).

## Building and running

Add the examples and their test targets to your `fx set` configuration (for example, on `workbench_eng.x64`):

```sh
fx set workbench_eng.x64 --with //src/ui/examples --with-test //src/ui/examples:tests
fx build
```

Launch any session-compatible example on a running device or emulator with `ffx session add`:

```sh
ffx session add --name snake-flatland fuchsia-pkg://fuchsia.com/flatland-examples#meta/snake-flatland.cm
```

To run the unit tests for the examples (`//src/ui/examples:tests` currently includes `flatland-rainbow-tests` and `snake-flatland-tests`; `//src/ui/examples/escher/test:escher_examples_test` can be built and run separately):

```sh
fx test //src/ui/examples:tests
```
