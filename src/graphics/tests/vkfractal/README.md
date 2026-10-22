# vkfractal

vkfractal is a Vulkan benchmark that renders an animated Julia set. Each pixel runs up to
`--max-iterations` iterations in a fragment shader that reads no textures or buffers, so the
frame time depends on the GPU's shader ALU throughput rather than its memory bandwidth.

vkfractal runs as a system test. It takes over the display while it runs and returns it when it
exits. It prints how long the timed frames took, and fails if a Vulkan call fails or the
validation layers report an error.

## Running

```
fx test vkfractal
fx test vkfractal -- --frames=100 --validation=false
fx test vkfractal -- --resolution=1024x1024
fx test vkfractal -- --offscreen
```

The result line looks like this:

```
vkfractal: 1200 frames (1200 presented) in 24.000 s: 20.000 ms/frame, 50.0 fps
```

`presented` is the number of timed frames that reached the display (see
[Presentation](#presentation)).

## Options

| Option | Default | Description |
| --- | --- | --- |
| `--frames=N` | 1200 | Frames per loop, at most 1000000. |
| `--loops=N` | 1 | Loops to render and time, or -1 to loop until killed. |
| `--warmup-frames=N` | 10 | Untimed frames rendered first, so that one-time driver work is not timed. |
| `--max-iterations=N` | 256 | Iteration cap per pixel. |
| `--fixed-iterations` | false | Runs `--max-iterations` iterations on every pixel, even after it escapes. |
| `--frames-in-flight=N` | 3 | Frames kept queued on the GPU, at most 16. |
| `--validation=BOOL` | true | Enables the Vulkan validation layers. |
| `--gpu-timing` | false | Also times each loop on the GPU with timestamp queries. |
| `--offscreen` | false | Renders without the display. |
| `--resolution=WxH` | The display's size | Points iterated per frame. |

`fx test vkfractal -- --help` prints the same list.

## The animation

Each frame shows the Julia set of z² + c for one value of the parameter c. Over a loop of
`--frames` frames, c goes once clockwise around the edge of the Mandelbrot set's main cardioid,
with a small dip inside near the cusp at c = 1/4 so that the loop starts with a rounded
cauliflower-like set, and detours counterclockwise around the edges of the 2/3 bulb (rabbit-like
sets), the 1/2 bulb (basilica-like sets), and the 1/3 bulb (the mirror-image rabbit). The last
frame leads back into the first, and every loop renders the same frames. The path is built in
`JuliaPath()` in `vkfractal.cc`, so changing that function changes the animation.

Points inside the filled Julia set never escape and run all `--max-iterations` iterations, while
points outside escape after a few, so the frame time follows the size of the filled area. For a
steady frame rate, pass `--fixed-iterations`: every pixel then runs `--max-iterations` iterations,
and the image stays the same. Iteration counts above 256 get the same black as points that never
escape, so caps above 256 render the same image as the default.

The view is centered on 0, and the complex plane is stretched along each axis so that the loop's
Julia sets stay 5% of the image's shorter side from each edge (for example, extents of 3.655x2.560
on a 1920x1080 display). vkfractal prints the extents in use before it starts.

## Colors

Each pixel's color comes from its smooth iteration count, a fractional count that removes the
bands between whole counts. The logarithm of the count picks a color from a palette that rises
through the magma colormap from black to pale yellow, then falls back to black. Points that never
escape are black too. The palette is a list of 21 colors in `vkfractal.cc`.

## Timing

vkfractal times each loop on the CPU, from an idle GPU until the GPU is idle again after the
loop's last frame. With `--gpu-timing`, it also times the same frames on the GPU with timestamp
queries and adds the GPU time to each result line. It fails if the GPU's queue has no timestamps.
With `--loops` other than 1, it prints a line for each loop, followed by the total. With both
options, a loop's line looks like this:

```
vkfractal: loop 1: 1200 frames (1200 presented) in 24.000 s: 20.000 ms/frame, 50.0 fps; GPU 23.940 s: 19.950 ms/frame
```

## Comparing devices

The default resolution is the display's size, so the default workload differs between devices.
Because the extents depend only on the rendered image's aspect ratio, passing `--resolution`
explicitly runs the same workload on every device, on screen or offscreen. On screen, the whole
image is stretched to fill the display.

The display swapchain shows images at their own size. When the resolution differs from the
display's, each frame renders into an offscreen image, and a bilinear pass (`shaders/scale.frag`)
stretches it over the display. On a display that is taller than it is wide, the image is also
rotated 90 degrees clockwise, and the default resolution is the display's size swapped (for
example, 1920x1080 on a 1080x1920 display).

With `--offscreen`, vkfractal creates no surface or swapchain and renders every frame into an
image that is never shown, 1024x1024 by default.

## Presentation

vkfractal never waits for the display. It keeps `--frames-in-flight` frames queued on the GPU,
and before recording a frame it waits only for the frame that last used the same slot. Each frame
takes a swapchain image only if one is already free. If it gets one, the frame is drawn into it,
directly or through the bilinear pass, and presented right away. Otherwise the frame renders into
an offscreen image and is discarded. At each vsync, the display shows the newest frame that has
finished rendering and returns the older ones. The swapchain has `--frames-in-flight` + 2 images,
at most 10, so when rendering is slower than the display, every frame is shown.

## Shaders

The GLSL shaders in `shaders/` are compiled to SPIR-V headers at build time by `BUILD.gn`.
