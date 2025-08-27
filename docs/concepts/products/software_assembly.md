# Software Assembly

**Software Assembly** enables developers to quickly build a product using a
customized operating system. Concretely, Software Assembly is a set of tools
that produce a Product Bundle (PB) from inputs including a set of Fuchsia
packages, a kernel, and config files.

## Product Bundle {:#product-bundle}

A **Product Bundle** is a directory of well-specified artifacts that can be
shipped to any environment and is used to flash, update, or emulate a Fuchsia
target. It's not expected that developers inspect the contents of a Product
Bundle directly, instead they can rely on the tools provided by Fuchsia. For
example, the `ffx` tool is used to flash or emulate a device using a Product
Bundle:

```shell {:.devsite-disable-click-to-copy}
# Flash a hardware target with the product bundle.
$ ffx target flash --product-bundle <PATH/TO/PRODUCT_BUNDLE>

# Start a new emulator instance with the product bundle.
$ ffx emu start <PATH/TO/PRODUCT_BUNDLE>
```

A Product Bundle may contain both a **main** and a **recovery** product, which
are distinct bootable experiences. Oftentimes `main` is used for the real
product experience, and `recovery` is used if `main` is malfunctioning and
cannot boot. `recovery` is generally a lightweight experience that is
capable of addressing issues in the `main` slot using a factory reset or an
Over-The-Air (OTA) update. Typically, an end-user can switch which product to
boot into by holding a physical button down on a device during boot up.

Software Assembly offers build tools for constructing Product Bundles, defines
the format of the inputs (platform, product config, board config), and provides
build rules to construct those inputs.

![A product bundle containing both a main product and a recovery product](images/software_assembly_01.svg "Diagram a main and recovery product in a product bundle"){: width="600"}

**Figure 1**. A Product Bundle may contain both a main product and a recovery
product.

Note: The terms _product, product image, system_, and _slot_ are often used
interchangeably.

## Platform, product config, and board config {:#platform-product-config-and-board-config}

The three inputs to assembly (**platform**, **product config**, **board
config**) are directories of artifacts. The internal format is subject to
change and shouldn't be depended on. Developers need to use the provided Bazel
or GN build rules to construct and use these inputs.

The **platform** is produced by the Fuchsia team. It contains every bit of
compiled platform code that any Fuchsia product may want to use. The Fuchsia
team releases the platform to
[https://chrome-infra-packages.appspot.com/p/fuchsia/assembly/platform][cipd-platform].

The **product config** is produced by a developer defining the end-user
experience. It may contain flags indicating which features of the platform to
include. For example, the product config can set `platform.fonts.enabled=true`,
resulting in assembly including the relevant fonts support from the platform.
See [this reference][platform-flags] for all the available flags. The product
config can additionally include custom code for building the user experience.

The **board config** is produced by a developer supporting a particular
hardware target. It includes all the necessary drivers to boot on that hardware.
Additionally, the board config can declare which hardware is available to be
used by the platform. For example, if the hardware has a Real Time Clock (RTC),
the board config can indicate that by setting the
`provided_features=["fuchsia::real_time_clock"]` flag. Assembly reads this
flag and includes the necessary code from the Platform for using this piece of
hardware. The Fuchsia team maintains a small set of board configs and releases
them to
[https://chrome-infra-packages.appspot.com/p/fuchsia/assembly/boards][cipd-boards].

## Environments {:#environments}

Customization is supported by other operating systems, but Fuchsia Software
Assembly has the unique ability to run in any conceivable environment and do so
quickly. A Fuchsia product can be customized and assembled in less than a
minute, which is much faster than other operating systems.

Fuchsia Software Assembly is currently supported in the following environments
(while no technical limitation prevents Fuchsia from extending support in the
future):

- Bazel
- CLI (experimental)
- GN (in `fuchsia.git`)

  Note: The details of the GN environment are left out of this page because
  Fuchsia is actively moving to [Bazel][fuchsia-product-bundle].

**Bazel**:

```bazel {:.devsite-disable-click-to-copy}
# A product bundle can contain both 'main' and 'recovery' products (systems/slots).
fuchsia_product_bundle(
    name = "my_product_bundle",
    main = ":main_product",
    recovery = "...",
)

# A product is a single bootable experience that is built by combining
# a platform, product, and board.
fuchsia_product(
    name = "main_product",
    platform = "//platform:x64",
    product = ":my_product",
    board = "//boards:x64",
)

# A product configuration defines the user experience by enabling
# platform features and including custom product code.
fuchsia_product_configuration(
    name = "my_product",
    product_config_json = {
        platform = {
            fonts = {
                enabled = True,
            },
        },
    },

    # The product code is included as packages.
    base_packages = [ ... ],
)
```

For a complete example, see the [`getting-started`][getting-started-repo]
repository.

**Command Line Interface**:

```shell {:.devsite-disable-click-to-copy}
ffx product-bundle create --platform 28.20250718.3.1 \
                          --product-config <PATH/TO/MY_PRODUCT_CONFIG> \
                          --board-config cipd://fuchsia/assembly/boards/x64@version:28.20250718.3.1
```

The [`ffx product-bundle create`][ffx-product-bundle-create] command can be run to produce
a new product bundle using already built platform, board, and product artifacts.

## Size and scrutiny {:#size-and-scrutiny}

Software Assembly provides tools for verifying the quality of a Product Bundle.

The **size check** tool informs the user whether the Product Bundle fits within
the partition size constraints of the target hardware. A [product size
report][size-check] can be generated using the following Bazel rules:

```bazel {:.devsite-disable-click-to-copy}
fuchsia_product_size_check(
    name = "main_product_size_report",
    product_image = ":main_product",
)

fuchsia_product(
    name = "main_product",
    ...
)
```

The **scrutiny** tool ensures that the Product Bundle meets a set of security
standards. If a developer provides the necessary scrutiny configs,
[scrutiny][scrutiny] runs during the construction of a Product Bundle. See the
following scrutiny configuration example:

```bazel {:.devsite-disable-click-to-copy}
fuchsia_product_bundle(
    name = "my_product_bundle",
    main = ":main_product",
    main_scrutiny_config = ":main_scrutiny_config",
)

fuchsia_scrutiny_config(
    name = "main_scrutiny_config",
    base_packages = [ ... ],    # Allowlist of base packages to expect.
    kernel_cmdline = [ ... ],   # Allowlist of kernel arguments to expect.
    pre_signing_policy = "...", # File containing the policies to check before signing.
)
```

Note: Fuchsia doesn't yet support running size checks or scrutiny as part of
the `ffx product create` command.

## Implementing a platform feature {:#implementing-a-platform-feature}

This section explains how to implement a new feature in the platform that can be
enabled by either a product config or a board config.

A platform feature is **always** implemented in fuchsia.git and must be generic
enough that it can be enabled on multiple products or boards. If the feature is
specific to a product or board, consider putting it inside the product or board
config instead.

Implementing a platform feature often involves the following steps:

1. [Write your feature flag in the product config or board config](#write-your-feature-flag).
1. [Prepare a subsystem that can read the flag](#prepare-a-subsystem).
1. [Write and enable your feature code inside the subsystem](#enable-your-feature-code).

![Diagram showing how a platform feature is implemented](images/software_assembly_02.svg "Diagram showing how a platform feature is implemented by adding a feature flag to a product config"){: width="800"}

**Figure 2**. Implementing a platform feature and enabling it in a product
config.

### 1. Write your feature flag (if necessary) {:#write-your-feature-flag}

Platform features are not often added to all products by default, but are
typically added when the product or board config has a specific flag set. The
first step to writing a platform feature is to determine when your feature needs
to be enabled, and therefore what product or board flags are necessary to turn
on the feature.

For example, if you want to allow the product config to enable a feature called
`foo.bar`, the product config could be written like the following:

```bazel {:.devsite-disable-click-to-copy}
fuchsia_product_configuration(
    name = "my_product",
    product_config_json = {
        platform = {
            foo = {
                bar = True,
            },
        },
    },
)
```

If you have many configuration options and need to organize them in a way that
is not easy to fit into the product config, you can consider a separate config
file. A config file can be any format, but most teams choose `json`. The config
file can be passed into the product config as a single file like the following:

```bazel {:.devsite-disable-click-to-copy}
fuchsia_product_configuration(
    name = "my_product",
    product_config_json = {
        platform = {
            foo = {
                bar_config = "LABEL(//path/to/config:file.json)",
            },
        },
    },
)
```

Note: The `LABEL()` syntax ensures that Bazel adds the proper dependency
that will rebuild the product config when `//path/to/config:file.json` changes.

Here are some examples of common situations:

| Include feature in | When product config flag is | And board config flag is |
| ----------- | ----------- | ----------- |
| All `eng` products by default | `platform.build_type = eng` | |
| Products that ask for it | `platform.<SUBSYSTEM>.<FEATURE> = true` | |
| Products that use a board that supports the hardware || `provided_features = [ "<HARDWARE>" ]` |

Assembly platform feature flags are declared in
[`//src/lib/assembly/config_schema`][config-schema]. ([Here][fonts-config] is
the fonts config that was mentioned previously.)

### 2. Prepare a subsystem {:#prepare-a-subsystem}

An **assembly subsystem** is a group of similar platform features (for example,
`connectivity, diagnostics`, and `fonts` are all separate subsystems). The job
of a subsystem is to read product and board config flags and decide when and how
to include platform features.

Note: [`//src/lib/assembly/platform_configuration/src/subsystems`][subsystems]
contains a list of subsystems that likely already suit your needs. Define a new
one only when no existing one is appropriate.

Below is a simple subsystem. This assumes that you have defined a new feature
flag inside `FooConfig` and are making it available in the
`define_configuration` function. Your subsystem needs to read those flags and
call assembly APIs to include your feature.

```rust {:.devsite-disable-click-to-copy}
use crate::subsystems::prelude::*;
use assembly_config_schema::platform_config::foo_config::FooConfig;

pub(crate) struct FooSubsystem;
impl DefineSubsystemConfiguration<FooConfig> for FooSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        foo_config: &FooConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {

        // Read the flag and enable the feature code.
        // See the below sections for more APIs.
        if foo_config.bar {
            builder.platform_bundle("foo");
        }

        Ok(())
    }
}
```

After declaring a new subsystem, you can call its
[`define_configuration`][define-configuration] in `subsystems.rs` and pass it
your config.

See the next section to learn about the available APIs to enable your feature
inside the subsystem.

### 3. Enable your feature code {:#enable-your-feature-code}

Determine if your code needs to be enabled at **build-time** or **runtime**.
Enabling your feature at build-time means that assembly will fully exclude your
code from the product when the product does not need it. But it also means that
in order to turn on your feature, the build rules must be updated. Enabling your
feature at runtime makes it easier to turn on and off without updating the build
rules, but results in adding unnecessary code to products that never want the
feature.

Whenever possible, build-time enablement is preferred in order to save space,
tighten security, enable static analysis, and increase performance for other
products that do not need the feature.

#### Build time {:#build-time}

Assembly organizes build-time features using Assembly Input Bundles (AIBs). A
feature owner can insert many types of artifacts into a single AIB, and Assembly
can be instructed when and how to add that AIB to a product. All AIBs are defined
in [`//bundles/assembly/BUILD.gn`][assembly-build]. Here is an example:

```gn {:.devsite-disable-click-to-copy}
# Declares a new AIB with the name "foo".
assembly_input_bundle("foo") {
  # Include this package into the "base package set".
  # See RFC-0212 for an explanation on package sets.
  # The provided targets must be fuchsia_package().
  base_packages = [ "//path/to/code:my_package" ]

  # Include this file into BootFS.
  # The provided targets must be bootfs_files_for_assembly().
  bootfs_files_labels = [ "//path/to/code:my_bootfs_file" ]
}
```

To include the AIB, use the following method in your subsystem:

```rust {:.devsite-disable-click-to-copy}
builder.platform_bundle("foo");
```

Note: If you're not adding a new feature flag, you can likely add your code
to an existing AIB instead of writing a new AIB. For example, the
`embeddable_eng` AIB is already added to every `eng` product, so if you want to
add a feature to all `eng` products, the feature code can be added to
`embeddable_eng`.

If you add a new AIB, don't forget to add it to the appropriate list in
[`//bundles/assembly/platform_aibs.gni`][platform-aibs-gni], or you will get an
error at build-time indicating that the AIB cannot be found.

#### Runtime {:#runtime}

Assembly supports multiple types of runtime configuration. These types are
listed in order of preference.

**Config capabilities**: A Fuchsia component can read the value of [config
capabilities][config-capabilities] at runtime, while Assembly sets the default
value for those capabilities at build time, for example:

```rust {:.devsite-disable-click-to-copy}
// Add a config capability named `fuchsia.foo.bar` to the config package.
builder.set_config_capability(
    "fuchsia.foo.bar",
    Config::new(ConfigValueType::String { max_size: 512 }, "my_string".into()),
)?;
```

Assembly will add all default config capabilities to a config package in BootFS,
therefore the capability will need to be routed from the `/root` component realm
to your component.

**Domain configs**: For complex configurations or those requiring custom types,
domain configs are preferable to config capabilities. Domain configs are Fuchsia
packages that provide a config file for your component to be read and parsed at
runtime, for example:

```rust {:.devsite-disable-click-to-copy}
// Create a new domain config in BlobFS with a file at "my_directory/foo_config.json".
builder.add_domain_config(PackageSetDestination::Blob(PackageDestination::FooConfigPkg))
      .directory("my_directory")
      .entry(FileEntry {
          source: config_src,
          destination: "foo_config.json".into(),
      })?;
```

Your component must launch the domain config package as a child and `use` the
directory, for example:

```json {:.devsite-disable-click-to-copy}
{
    children: [
        {
            name: "my-config",
            url: "fuchsia-pkg://fuchsia.com/foo-config#meta/foo-config.cm",
        },
    ],
    use: [
       {
            directory: "my_directory",
            from: "#foo-config",
            path: "/my_directory",
        },
    ],
}
```

**Kernel argument**: A kernel argument is only used for enabling kernel
features. Assembly constructs a command line to pass to the kernel at runtime,
for example:

```rust {:.devsite-disable-click-to-copy}
builder.kernel_arg(KernelArg::MyArgument);
```

## Appendix: Developer overrides {:#developer-overrides}

Developers oftentimes want to locally test something on an existing product
by adding new code or flipping a feature flag. Modifying the product or board
configs is undesirable because it pollutes the git-tree (`fuchsia.git`).
Assembly supports a method of locally modifying an existing product without
polluting the git-tree using [developer overrides][developer-overrides].

<!-- Reference links -->

[cipd-platform]: https://chrome-infra-packages.appspot.com/p/fuchsia/assembly/platform
[platform-flags]: https://fuchsia.dev/reference/assembly/PlatformSettings
[cipd-boards]: https://chrome-infra-packages.appspot.com/p/fuchsia/assembly/boards
[fuchsia-product-bundle]: https://fuchsia.dev/reference/bazel_sdk/fuchsia_product_bundle
[getting-started-repo]: https://fuchsia.googlesource.com/sdk-samples/getting-started
[ffx-product-bundle-create]: https://fuchsia.dev/reference/tools/sdk/ffx#ffx_product-bundle_create
[size-check]: https://fuchsia.dev/reference/bazel_sdk/fuchsia_product_size_check
[scrutiny]: https://fuchsia.dev/reference/bazel_sdk/fuchsia_scrutiny_config
[config-schema]: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/lib/assembly/config_schema/
[fonts-config]: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/lib/assembly/config_schema/src/platform_config/fonts_config.rs;l=17;drc=cda41b20c536a7803e14e76902d279048c5a203d
[subsystems]: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/lib/assembly/platform_configuration/src/subsystems/
[define-configuration]: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/lib/assembly/platform_configuration/src/subsystems.rs;l=267;drc=623ecca5878d9b8b1442486e1d29f752af26e447
[assembly-build]: https://cs.opensource.google/fuchsia/fuchsia/+/main:bundles/assembly/BUILD.gn
[platform-aibs-gni]: https://cs.opensource.google/fuchsia/fuchsia/+/main:bundles/assembly/platform_aibs.gni
[config-capabilities]: /docs/concepts/components/v2/capabilities/configuration.md
[developer-overrides]: /docs/development/build/assembly_developer_overrides.md

