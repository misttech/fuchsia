# virtio-gpu guest Display Driver

This is a display driver for [the `virtio-gpu` device][virtio-spec-gpu-section]
described in the [virtio specification][virtio-spec].

## Display / GPU split

Conceptually, the `virtio-gpu` device is a combination of a display engine and a
GPU.

The `virtio_gpu_ctrl_type` enumeration in the
["Device Operation: Request header" section][virtio-spec-gpu-request-section]
lists all the commands implemented by the `virtio-gpu` device. Conceptually, the
2D commands and the cursor commands map to the display engine, while the 3D
commands map to the GPU.

The display guest driver (this driver) binds to the `virtio-gpu` device, and
mediates the GPU guest driver's access to the `virtio-gpu` device.

## Resources

Blob resources may be supported by the virtio-gpu device. If supported, the
driver prefers blob resources because the image stride can be specified for
scanout (otherwise, stride is assumed to be packed).

## Development process

These instructions work with a `workbench_eng.x64` build that includes the
GN targets mentioned below. Debug builds are recommended for iterating on the
driver code.

```posix-terminal
fx set workbench_eng.x64 --debug --with //src/graphics/display:tools \
    --with //src/graphics/display:tests
```

### Preparation

Follow [the virtio-spec processing guide][spec-processing-guide] to prepare a
Markdown version of the virtio specification in the `local/virtio-spec/`
directory.

### Iterating on driver changes

Follow [the change validation guide][change-validation-guide] to quickly check
your change under QEMU.

### Manual testing

We do not currently have automated integration tests.

Before uploading a behavior-changing CL for review, follow
[the manual testing process][manual-testing-process] to ensure that the CL doesn't
regress key development workflows.

## References

The code contains references to the following documents.

* [OASIS Virtual I/O Device (VIRTIO)][virtio-spec] specification - version
  1.4, Committee Specification 01, dated 8 April 2026

[change-validation-guide]: ./docs/change-validation.md
[manual-testing-process]: ./docs/manual-testing.md
[spec-processing-guide]: ./docs/spec-processing.md
[virtio-spec]: https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html
[virtio-spec-gpu-section]: https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html#x1-4730007
[virtio-spec-gpu-request-header]: https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html#x1-4880007
