---
name: suspend-resume-migration-integration
description: >
  Add or migrate suspend/resume support in a Fuchsia driver using Driver-
  Framework power elements. Use when a driver implements
  fuchsia.power.system/SuspendBlocker, fdf_power::Suspendable, or the Rust
  fdf_power::SuspendableDriver trait and must move to framework power elements
  (migration), or has no suspend/resume support yet and needs it added
  (integration). Covers the required component manifest (.cml), BUILD.gn/Bazel
  deps, and implementation in both C++ and Rust; routes to
  suspend_resume_migration.md vs suspend_resume_integration.md.
---

## Overview

This guide covers how to add or update support for suspend and resume in
drivers. This covers necessary updates to the component manifest, build files,
and implementation both in C++ and Rust.

## Guide

### Determine migration or integration

First determine if we need to do a migration or an integration.

For migrations look to see if there are implementations of
`fuchsia.power.system/SuspendBlocker` or `fdf_power::Suspendable` for C++
drivers or the `fdf_power::SuspendableDriver` trait for Rust drivers. In these
cases, look at suspend_resume_migration.md.

For other cases look at how to do an integration in
suspend_resume_integration.md.
