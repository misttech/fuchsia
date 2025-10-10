// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_dash as fdash;
use fuchsia_component::client::connect_to_protocol;

#[fuchsia::test]
pub async fn unknown_tools_package() {
    let (_stdio, stdio_server) = zx::Socket::create_stream();

    let launcher = connect_to_protocol::<fdash::LauncherMarker>().unwrap();

    let urls = &["fuchsia-pkg://fuchsia.com/bar".to_string()];
    let result = launcher
        .explore_component_over_socket(
            ".",
            stdio_server,
            urls,
            None,
            fdash::DashNamespaceLayout::NestAllInstanceDirs,
        )
        .await
        .unwrap();

    assert!(result.is_ok());

    let mut buf = [0u8; 1024];
    let bytes_read = _stdio.read(&mut buf).unwrap();
    let output = std::str::from_utf8(&buf[..bytes_read]).unwrap();

    assert!(output.contains("Failed to load at least one tool package"));
    assert!(output.contains("fuchsia-pkg://fuchsia.com/bar"));
}

#[fuchsia::test]
pub async fn bad_moniker() {
    let (_stdio, stdio_server) = zx::Socket::create_stream();

    let launcher = connect_to_protocol::<fdash::LauncherMarker>().unwrap();

    // Give a string that won't parse correctly as a moniker.
    let err = launcher
        .explore_component_over_socket(
            "!@#$%^&*(",
            stdio_server,
            &[],
            None,
            fdash::DashNamespaceLayout::NestAllInstanceDirs,
        )
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err, fdash::LauncherError::BadMoniker);
}

#[fuchsia::test]
pub async fn instance_not_found() {
    let (_stdio, stdio_server) = zx::Socket::create_stream();

    let launcher = connect_to_protocol::<fdash::LauncherMarker>().unwrap();

    // Give a moniker to an instance that does not exist.
    let err = launcher
        .explore_component_over_socket(
            "./does_not_exist",
            stdio_server,
            &[],
            None,
            fdash::DashNamespaceLayout::NestAllInstanceDirs,
        )
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err, fdash::LauncherError::InstanceNotFound);
}

#[fuchsia::test]
pub async fn bad_url() {
    let (_stdio, stdio_server) = zx::Socket::create_stream();

    let launcher = connect_to_protocol::<fdash::LauncherMarker>().unwrap();

    let urls = &["#".to_string()];
    let result = launcher
        .explore_component_over_socket(
            ".",
            stdio_server,
            urls,
            None,
            fdash::DashNamespaceLayout::NestAllInstanceDirs,
        )
        .await
        .unwrap();

    assert!(result.is_ok());

    let mut buf = [0u8; 1024];
    let bytes_read = _stdio.read(&mut buf).unwrap();
    let output = std::str::from_utf8(&buf[..bytes_read]).unwrap();

    assert!(output.contains("Failed to load at least one tool package"));
    assert!(output.contains("while parsing tool url #"));
}
