---
name: suspend-resume-migration-integration
description: >-
  How to use driver framework-provided power elements to implement
  suspend/resume support in the driver. Covers migrating a driver which
  currently uses SuspendBlockers and drivers without any current suspend/resume
  support.
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
drivers or the `fdf_power::SuspendableDriver` trait for Rust drivers. In
these cases, look at suspend_resume_migration.md.

For other cases look at how to do an integration in suspend_resume_integration.md.
