# Defining Bazel rules, macros, and functions

[TOC]

## Overview

This page contains Fuchsia-specific style and best practices for writing rules,
macros, and functions. These are always defined in
[`.bzl` files][bzl-files-guide], and the guidance for those applies here as
well. In addition, guidance related to targets in
[`BUILD.bazel` files][build-files-guide] applies to targets defined by rules
and macros.

This page is part of the
[Bazel style guide and best practices][style-guide-landing-page].

## Consider whether new macros and functions are appropriate

As in the Bazel style guide, Fuchsia
[prefers DAMP (Descriptive and Meaningful Phrases) `BUILD` files over DRY
(Don't Repeat Yourself)][bazel-official-build-style-damp]{:.external}.

In particular, the Fuchsia build uses area-specific macros and functions much
less frequently in Bazel than it used such templates in GN.

Use shared, static, lists where things MUST be kept in sync (e.g. common
dependencies), instead of writing area-specific macros.

While area-specific templates can reduce typing at the initial creation of
targets, they become a maintenance and migration issue, especially for
automated tooling, as the individual targets are hidden from view by the
area-specific macros. Consult with the Build Team if you think you would
strongly benefit from the use of area-specific macros and functions (the bar is
high, based on past experience).

## Prefer using existing macros and rules where possible {:#prefer-using-existing-macros-and-rules-where-possible}

The importance of this increases with the potential extent of the use of such a
macro or rule.

Especially if your needs are limited to the following, consider whether they
can be accomplished using existing macro(s) and/or rule(s):

* Generating metadata

* Enforcing attribute values

* Minimizing, for example, the number of attributes callers must specify

In the first two cases, especially, consider whether a Bazel query,
[SHAC rule][fuchsia-static-analyzers], test, or some other mechanism can
satisfy your needs. Bazel queries can be run on dependency trees or all targets
defined in a directory tree. Can you write a test that runs a query and checks
the results? If your use case requires an extra attribute, consider using
[tags][bazel-common-tags]{:.external}.

Reasons to use existing rules and macros include:

* Easier to understand and less ramp-up time for developers

  * A developer familiar with Bazel can jump in and understand targets.

  * Developers in one part of the team can understand targets in other parts of
    the code base.

* AI is more likely to understand and produce Bazel files that use common rules
  and macros

* Fewer `load()` statements required

  * Built-in identifiers do not require any load statements.

  * Others may already be loaded for other targets in the file.

* Less opportunity for bugs

  * For example, aspects only work correctly when they traverse all relevant
    targets. A custom macro or rule is an opportunity to introduce a path that
    won't be followed. See
    [Use standard attribute names][use-standard-attribute-names].

## Prefer symbolic macros to legacy macros

Prefer writing [symbolic macros][bazel-symbolic-macros]{:.external}, which are
defined using `macro()`, over legacy macros (Python-like functions defined with
`def` that create targets). Symbolic macros provide clearer documentation of
attributes and their types, perform type checking on attributes, and ensure
labels are evaluated at the call site. They also support attribute inheritance
(via `inherit_attrs`). For further context, see
[Why you shouldn't use legacy macros][bazel-no-legacy-macros]{:.external}.

Never specify default values for arguments in a symbolic macro's implementation
function as the default value is defined by the `attrs` entry, and a value will
be provided for every attribute.

Note: As of 2026, symbolic macros are a relatively recent addition to Bazel, so
you may see legacy macros in projects, including the Fuchsia Bazel SDK, that
have been using Bazel for years.

There are some cases where using a legacy macro wrapper around a symbolic macro
is necessary, but these should be very rare for most developers.

## Visibility and access checks for targets defined within macros

### Overview

For the purposes of visibility, think of legacy macros as if they are expanded
inline wherever they are instantiated. When used in `BUILD.bazel` files, the
targets defined by a legacy macro are effectively defined in that file. As a
result:

* The defined targets' default `visibility` is the same as the instantiating
  package, including the package `default_visibility` if specified.

* The macro can use (e.g., add to `deps`) any target that is visible to the
  instantiating package.

  * The location of the `.bzl` file defining the legacy macro is irrelevant.

However, for the purposes of visibility, think of targets defined by symbolic
macros as if they are defined in a `BUILD.bazel` file in the same package
(directory) as the `.bzl` file defining the macro. As a result:

* The defined targets' default `visibility` is the package (directory)
  containing the `.bzl` file.

* The macro can use (e.g., add to `deps`) any target that is visible to the
  package (directory) containing the `.bzl` file.

  * This can be useful for, for example, FIDL bindings support libraries added
    by `fidl_library()`.

  * But it is problematic for things such as an HLCPP support library allowlist.

  * See [issue 446911800][fxbug-446911800]{:.external} for details.

### Specifying visibility for targets defined in macros

#### Public targets

Forward the `visibility` attribute passed to the macro to the main target
defined by the macro - the one to which `name` is passed. The `visibility`
attribute may also be forwarded to other defined public targets mentioned in
the macro's `doc` string as appropriate.

#### Private targets

In symbolic macros, the visibility of all other targets defined will default to
`["//visibility:private"]`. For legacy macros, however, you must
[Specify visibility for all targets defined by legacy
macros][specify-visibility-legacy-macros].

### Specify visibility for all targets defined by legacy macros {:#specify-visibility-for-all-targets-defined-by-legacy-macros}

Specify `visibility = ["//visibility:private"]` for all targets that do not use
the `visibility` attribute passed to the macro. This is necessary to prevent
them from [defaulting to][bazel-common-visibility]{:.external} the package's
`default_visibility` if specified.

## Use standard attribute names {:#use-standard-attribute-names}

Use standard attribute names in macros and rules. For example, use `"deps"`,
`"data"`, or even `"tools"` rather than `"images"` or `"scripts"`. See
[some generally applicable
attributes][bazel-official-bzl-style-rules-attrs]{:.external}.

Reasons for this include:

* For readability and other reasons similar to those in
  [Prefer using existing macros and rules where
  possible][prefer-existing-macros-rules].

* Aspects only work correctly when they traverse all relevant targets, and
  macros will generally be configured to be applied to `"deps"`, and other
  common attributes as appropriate. However, they are unlikely to be aware of,
  for example, `"rust_deps"`, and failing to be applied to that attribute could
  exclude targets relevant to the aspect.

## Require named arguments

Public functions and legacy macros (a special category of function) should
generally declare keyword-only arguments (all arguments are declared after
`*,`). Exceptions may be made for functions with at most a few arguments where
the arguments are clear from the symbol name, and the arguments will not be
mistakenly used in the wrong position (including due to
refactoring/reordering). Boolean arguments and arguments with default values
should always appear after the `*`.

While this is most important for symbols meant to be used by other parts of the
codebase, it also applies to public symbols in all `.bzl` files.

This helps enforce the Bazel `.bzl` Style Guide's
[guidance][bazel-official-bzl-style-macros]{:.external} that "When calling a
macro, use only keyword arguments. This is consistent with rules, and greatly
improves readability."

## Declare all arguments used by a macro

If a macro (optionally) uses an argument, explicitly declare that argument in
the macro implementation's parameters list rather than extracting it from
`kwargs`. For example, avoid the following:

```none {:.devsite-disable-click-to-copy}
# Do NOT do this:
testonly = kwargs.get("testonly", False),
```

Declaring the arguments makes it clearer which arguments are relevant to the
macro implementation (vs., for example, macros it calls) and provides a single
place to see the default value. For legacy macros, it also ensures that the
arguments are documented. (This is mostly relevant for the Fuchsia Bazel SDK.)

As with any other argument, but especially
[Attributes common to all build rules][bazel-common-attributes]{:.external} and
[Attributes common to all test rules
(_test)][bazel-common-attributes-tests]{:.external}, you must be sure to pass
the argument to all macros and rules that support it since these will not be in
`**kwargs`.

## Comments

* Provide function-level comments for all public functions (those that do not
  begin with an underscore).

* Provide `doc` strings for all rules and macros.

* Provide `doc` strings for all rule and macro attributes.

  * `doc` strings may be omitted for private attributes (those that begin
    with an underscore) where the meaning is obvious from the `default` value.

### doc strings

* In the Fuchsia platform, `doc` strings are meant to be read in the source
  file rather than in some generated documentation. Thus, prefer optimizing for
  that rather than how some generated documentation might look.

* Long `doc` strings:

  * Prefer writing a top-level single sentence description entirely on the same
    line as `doc=` where reasonable.

    * There is
      [no strict line length
      limit][bazel-official-build-style-python-diff]{:.external} in Bazel.

  * When multiple lines are necessary, write multiline strings using triple
    quotes.

    * Avoid appending regular strings.

* Multiline `doc` strings

  * Begin multiline strings on the same line as the `doc` argument
    (`doc = """Begin the comment...`).

  * Start subsequent lines under the `d` in `doc`, similar to how Python
    comments start the next line under the first `"`.

    * This optimizes for consistency and readability while accepting that it
      would not be ideal for generated text.

  * End multiline strings with triple quotes (`"""`) on a separate line aligned
    with `doc`.

## Rule and macro implementation function names

Name `implementation` functions for `rule()` and `macro()` instances using a
leading underscore, followed by the name of the rule or function, and ending
with `_impl`.

## Use variables when referencing target names within macros

If a macro defines a target then depends on it in another target, define a
variable with the former target's name and use that for its `name` attribute
and in the `deps` of the latter target.

This unambiguously links the two targets, especially in complex cases where
there are multiple levels of target names based on other target names, and is
easier to highlight and search for.

Do not use variables for targets not used internally unless it enhances
readability, such as when all target names are defined in one place.

## Wrapping built-in and common rules, macros, and functions {:#wrapping-built-in-and-common-rules-macros-and-functions}

When writing a macro to be used in place of a common Bazel rule, macro, or
function within the Fuchsia platform codebase, prefix the wrapped name with
`fx_`. For example, `fx_cc_library()` is to be used instead of `cc_library()`.
"Common" includes symbols built into Bazel (including `native.*`) as well as
those in common repositories such as `rules_cc`. Also use this prefix for other
conflicts, such as `fx_package()`, which is unrelated to `package`. Do not use
a `fuchsia_` prefix as
[`fuchsia_...` files and symbols are in the Fuchsia Bazel
SDK][fuchsia-prefix-sdk].

Note: `rustc_*` are an exception because these do not conflict with `rust_*`.

## Avoid configuration transitions

Fuchsia platform targets should already build in the right configuration. If
you think you need a transition, you most likely don't. Reach out to the Build
team; only the Build team should add transitions or new platforms.

## Pass tools as executable label attributes

A rule that runs a tool should take it as
`attr.label(executable = True, cfg = "exec")` and read it with
`ctx.executable`.

## Antipatterns

Note: These mostly come up in AI-generated code.

- Looking up tools through `PATH`. Infra sandboxes don't set it up, and Bazel
  can't track a tool it doesn't know about. Pass the tool as an executable
  label attribute instead.

- Passing JSON or dict blobs that embed labels into rules or macros. Bazel only
  sees labels in label-typed attributes, so it won't build or track those
  dependencies. Use label attributes and providers instead.

- Finding build outputs through the `bazel-bin` symlink. It points at one
  configuration, and a transition can put the output you want in a different
  `bazel-out` directory. Pass outputs through providers or report their paths
  explicitly.

- Relying on implementation details of upstream rulesets, such as the
  `_solib_` directory prefix that `rules_cc` uses. They change without notice
  when the ruleset is updated.

## Avoid caching or remoting large artifacts

See [Avoiding caching for large artifacts][avoiding-caching-large-artifacts].

<!-- Reference links -->

[avoiding-caching-large-artifacts]: /docs/development/build/bazel_disk_cache.md#avoiding-caching-large-artifacts
[bazel-common-attributes]: https://bazel.build/reference/be/common-definitions#common-attributes
[bazel-common-attributes-tests]: https://bazel.build/reference/be/common-definitions#common-attributes-tests
[bazel-common-tags]: https://bazel.build/reference/be/common-definitions#common.tags
[bazel-common-visibility]: https://bazel.build/reference/be/common-definitions#common.visibility
[bazel-no-legacy-macros]: https://bazel.build/extending/legacy-macros#no-legacy-macros
[bazel-official-build-style-damp]: https://bazel.build/build/style-guide#prefer-damp-build-files-over-dry
[bazel-official-build-style-python-diff]: https://bazel.build/build/style-guide#differences-python-style-guide
[bazel-official-bzl-style-macros]: https://bazel.build/rules/bzl-style#macros
[bazel-official-bzl-style-rules-attrs]: https://bazel.build/rules/bzl-style#rules:~:text=Some%20generally%20applicable%20attributes
[bazel-symbolic-macros]: https://bazel.build/extending/macros
[build-files-guide]: build_files.md
[bzl-files-guide]: bzl_files.md
[fuchsia-prefix-sdk]: common.md#fuchsia-files-and-symbols-are-in-the-fuchsia-bazel-sdk
[fuchsia-static-analyzers]: /docs/development/source_code/static_analyzers.md
[fxbug-446911800]: https://fxbug.dev/446911800
[prefer-existing-macros-rules]: #prefer-using-existing-macros-and-rules-where-possible
[specify-visibility-legacy-macros]: #specify-visibility-for-all-targets-defined-by-legacy-macros
[style-guide-landing-page]: README.md
[use-standard-attribute-names]: #use-standard-attribute-names
