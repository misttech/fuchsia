## dt-schema

This is an implementation of the [dt-schema](https://github.com/devicetree-org/dt-schema) python tool in Rust.

Currently it only supports validating schemas against devicetree blobs.

Example usage:

```
cargo run validate my-schema.yaml my-other-schema.yaml --dtb my-devicetree.dtb
```
