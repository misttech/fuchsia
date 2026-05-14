# The Zircon build

## Kernel switchsets, or how the kernel is compiled and linked

- **What**: A suite of kernel-specific GN toolchains parameterized by CPU
  architecture, representing compiler and linker flags (e.g., optimization
  levels, debug levels, asserts, defines).

- **Where in source**: Switchsets are defined at
  `//zircon/kernel/switch/set/$name:kernel_$cpu.$name` using the
  [kernel_switchset()](/zircon/kernel/kernel_switchset.gni) template.

- **Where in the build directory**: The compilation artifacts for a given
  switchset will be placed under `<build_dir>/kernel_$cpu.$name/`

- **Constraint**: A switchset must be _defined_ within the default toolchain.

**Current switchsets**:

- [lk_debug_level_0](/zircon/kernel/switch/set/lk_debug_level_0/BUILD.gn):
  Production-like configuration with minimal debug-like features, setting
  `LK_DEBUGLEVEL=0`;
- [lk_debug_level_2](/zircon/kernel/switch/set/lk_debug_level_2/BUILD.gn):
  Development-like configuration with full debug-like features, setting
  `LK_DEBUGLEVEL=2`.

## Kernel executables

- **What**: ELF-producing targets representing the kernel binary itself. Defines
  what is included in the kernel link (e.g., tests and debug commands).

- **Where in source**: Defined in `//zircon/kernel/bin/BUILD.gn` using the
  [kernel_executable()](/zircon/kernel/bin/BUILD.gn) template.

- **Where in the build directory**: The unstripped ELF binary compiled under a
  given switchset is placed at `<build_dir>/kernel_$cpu.$switchset/$executable`.

- **Constraint**: Must only be evaluated within a kernel switchset toolchain.

**Current kernel executables**:

- `vmzircon`: Standard kernel intended for production (no tests or extra debug
  commands). This is what is packaged in user-like products.
- `vmzircon.with-tests`: Standard kernel intended for development (compiled with
  unit tests and additional debug commands). This is what is packaged in eng
  products.

## Kernel Images (A.K.A. kernel ZBIs)

- **What**: Minimal bootable ZBIs containing a kernel (by definition) and
  everything needing to boot it. They package a kernel executable (evaluated
  under a specific switchset), along with things like boot options, code
  patching inputs, userboot, and a vDSO.

- **Where in source**: Defined at `//zircon/kernel/image/$name:$name` using the
  [kernel_image()](/zircon/kernel/kernel_image.gni) template

- **Where in the build directory**: The ZBI is placed at
  `<build_dir>/kernel.$name.zbi`.

**Current kernel images**:

- [eng](/zircon/kernel/image/eng/BUILD.gn): Assembled into eng products. Uses
  `vmzircon.with-tests` under the `lk_debug_level_2` switchset, with
  debug/serial syscalls enabled.

- [user](/zircon/kernel/image/user/BUILD.gn): Assembled into user products. Uses
  `vmzircon` under the `lk_debug_level_0` switchset, with serial/debug syscalls
  disabled.

- [userdebug](/zircon/kernel/image/userdebug/BUILD.gn): Assembled into userdebug
  products. Uses `vmzircon` under the `lk_debug_level_0` switchset, but with
  serial output enabled.

- [eng.lk_debug_level_0](/zircon/kernel/image/eng.lk_debug_level_0/BUILD.gn):
  Eng-like test image to verify that debug assertions are not load-bearing. Uses
  `vmzircon.with-tests` under `lk_debug_level_0`.

## Source organization

While it might well be more convenient to have consolidated more of these
definitions in fewer build files, the current organization is intended to
minimize the amount of unnecessary GN evaluation that happens at `gen` time.
(Recall that if GN sees a redirect to another toolchain in a build file, it will
follow it, and so on.)

- **Switchsets** are isolated in subdirectories so they are only evaluated when
  explicitly referenced.

- **Executables** are centralized in `//zircon/kernel/bin/BUILD.gn` for
  readability and to ensure they are only evaluated under kernel switchsets.

- **Images** are separated to allow different images to depend on different
  switchsets without accidentally referencing unused switchsets.

## FAQ

### Which kernel does Product Assembly end up using?

Product Assembly knows about three particular kernel images: `eng`, `user`, and
`userdebug`, and only by label. Its contract with the kernel build is simply
that it will pick out the appropriate kernel image based based on a product's
build type.

### Which kernel do core tests end up running against?

By default, 'kernel ZBI tests' (e.g., core tests) run against the `eng` kernel
image, which in turn packages the `vmzircon.with-tests` executable under the
`lk_debug_level_2` switchset. To run these tests against additional kernel
images (e.g., `eng.lk_debug_level_0`), set the `extra_kernel_test_images` GN arg
in your `args.gn`:

```gn
extra_kernel_test_images = [ "eng.lk_debug_level_0" ]
```

Generally, this will cause
[kernel_zbi_test()](/build/testing/boot_tests/kernel_zbi_test.gni) targets to
also generate test variants for the specified images (e.g.,
`path/to/test:test.user` alongside `path/to/test:test.eng`), which can then be
run via `fx run-boot-test`. The above example is what all bringup builders set.

For core tests in particular, one may run
`fx core-tests --kernel=$kernel_image_name` to locally run core tests against a
given kernel image under QEMU. Here `$kernel_image_name` must be one of `eng`
(the default) or an entry in your local `extra_kernel_test_images`.

### How to enable boot options in core tests only?

`//zircon/kernel/image/$name:$name.test-data-deps` are data dependencies that
every instance of
[kernel_zbi_test()](/build/testing/boot_tests/kernel_zbi_test.gni) (e.g., core
tests) includes. This target may be updated to depend on desired
[`kernel_cmdline()`](/build/zbi/kernel_cmdline.gni) instances to enable
test-only boot options.

### How to disassemble the kernel?

It is most practical to run `fx dis vmzircon` or `fx dis vmzircon.with-tests` to
disassemble `vmzircon` or `vmzircon.with-tests` across all switchsets in your
build. One can also run `fx dis 'vmzircon*'` include both executables.

### I just want to build the kernel

There is no longer a singular 'kernel' to build. For a more minimal build while
iterating on kernel build, it is recommended that one build kernel images
directly (e.g., `fx build '//zircon/kernel/image/eng'` or
`fx build -- kernel.eng.zbi`).
