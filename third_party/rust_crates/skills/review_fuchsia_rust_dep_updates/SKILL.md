---
name: review-fuchsia-rust-dep-updates
description: >-
  Review updates to third-party Rust crate dependencies in Fuchsia and check for
  common errors, unintended changes, and missed steps in the process.
---

# Reviewing Fuchsia Rust Dependency updates

This top-level skill describes the process for checking changes to third-party
Rust crate dependencies under `third_party/rust_crates` to make sure that the
documented processes were correctly followed.

---

## 1. Identify which crates were intended to be changed

Sometimes, crates get updated unintentionally by our tooling. Read the commit
message and look for a description of which crates were intended to be updated.
For updates to a single crate or small collection of crates, the specific crate
names may be listed. For updates to a larger "family" of related crates, the
changes may be summarized under a common prefix or colloquial name. For example,
this commit message describes a family of crates and some of the specific ones
that were updated:

```
[rust-3p] Update foobar to 1.2.3

* Updates foobar to 1.2.3
* Updates foobar-fmt to 1.2.5
* Adds foobar-tracing at 1.2.0
* Removes foobar-cli at 1.0.0
```

When a family of crates is described, it is acceptable to not explicitly list
every crate.

---

## 2. Verify that only the identified crates were changed

Each changed crate will be added, updated, or removed, and those changes will be
reflected in the `third_party/rust_crates/vendor` directory. Check each changed
directory in `vendor` and make sure that it's properly described by the commit
message.

---

## 3. Ensure that OSRB processes were followed

### When OSRB processes need to be followed

In some situations, the Open-Source Review Board must approve additions to the
set of third-party crates. This is needed when any of the following conditions
are met:

1. A new crate is added that was not already vendored at an older version. Any
new crates need OSRB approval, but this doesn't apply to crates that are already
in-tree at an older version.
2. A crate is updated in a way that changes its licensing. Sometimes, updating a
crate will change its licensing. For example, a crate update may change its
licensing from `MIT OR Apache-2.0` to `LGPL` - this requires OSRB approval.

Some kinds of licensing changes are more serious problems than others. Licensing
changes that add more allowed licenses are not uncommon, and are typically not a
serious issue. An example of this would be changing from `MIT` to
`MIT OR Apache-2.0`. We previously used the crate under the `MIT` license and
can continue to do so. Other kinds of licensing changes are serious issues. If a
crate changes its licensing completely (for example, `MIT` to `LGPL`) then the
OSRB must approve the crate update. Another serious issue is if a crate changes
its licensing to add an unapproved license. Look in
`tools/check-licenses/assets/patterns` for licenses which are `Approved`,
`Notice`, `Permissive`, `Unencumbered`, or `Unclassified` - all of these are
approved by default. Anything other license should be considered a serious
issue.

### How to ensure that OSRB processes were followed

Check the commit message for evidence that the OSRB approved any relevant
changes. This may include issue numbers - make sure to verify that the issues
ask for OSRB approval for the required updates.

---

## 4. Check that `build.rs` changes were accounted for

Some third-party crates have `build.rs` scripts. These scripts cannot be
executed by our build system. Instead, the effects of these scripts are computed
and written to `third_party/rust_crates/Cargo.toml` under `[gn.package.*]`
sections. Make sure that any modifications to `build.rs` scripts for a crate
have a sensible corresponding change to its `gn.package` section. Often, the
only change that will be needed is updating the version of the crate in the
section. It is important that `build.rs` changes are reflected in `Cargo.toml`
even if a crate is an unbuilt dependency because unbuilt dependencies may start
being built in the future.

---

Report any findings from these steps concisely. The findings from this analysis
are less valuable when they contain extraneous details or flowery language. A
single sentence is sufficient for most findings.
