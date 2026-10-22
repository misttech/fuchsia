# virtio_driver

Driver-side implementation of the virtio 1.x PCI transport, shared by Rust
virtio drivers, such as `virtio-gpu-display`.

The library is parked under `//src/graphics/display` because it is still
coupled to display. Once it is cleaned up, it may move to `//src/devices`.

The crate documentation in [`src/lib.rs`](src/lib.rs) introduces the concepts
used in the API.

## Testing

```posix-terminal
fx test virtio-driver-rust-test
```

## References

The code contains references to the following documents.

* `@ref(virtio): kind=doc title="Virtual I/O Device (VIRTIO) Specification" version=1.4 date=2026-04-08 url=https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html local=local/virtio-spec/all.md`
* `@ref(pci): kind=doc title="PCI Local Bus Specification" version=3.0 date=2004-02-03`

[The virtio-spec processing guide][spec-processing-guide] explains how to
prepare the local Markdown copy of the virtio specification.

[spec-processing-guide]: /src/graphics/display/drivers/virtio-gpu-display/docs/spec-processing.md
