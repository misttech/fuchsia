# Bazel

[Bazel][bazel]{:.external} is the long-term build system for Fuchsia.
As of late 2026, Fuchsia is [migrating][bazel-migration-guidelines] its build
system from GN to Bazel.

- [Bazel style guide and best practices][style-guide-landing-page]

  - [Common][common-guide]

  - [`BUILD.bazel` files][build-files-guide]

  - [`.bzl` files][bzl-files-guide]

  - [Defining rules, macros, and functions][rules-macros-guide]

  - [Tests and test suites][tests-guide]

<!-- Reference links -->

[bazel]: https://bazel.build/
[bazel-migration-guidelines]: /docs/development/build/bazel_migration_guidelines.md
[build-files-guide]: style/build_files.md
[bzl-files-guide]: style/bzl_files.md
[common-guide]: style/common.md
[rules-macros-guide]: style/rules_macros.md
[style-guide-landing-page]: style/README.md
[tests-guide]: style/tests.md
