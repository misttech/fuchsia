// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TestEnv;
use assert_matches::assert_matches;
use fidl_fuchsia_component_decl as fcomponent_decl;
use fidl_fuchsia_component_resolution as fcomponent_resolution;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_pkg_testing::{PackageBuilder, SystemImageBuilder};

#[fuchsia::test]
async fn resolve_base_package() {
    let base_pkg = PackageBuilder::new("a-base-package").build().await.unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&base_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&base_pkg])
        .await
        .build()
        .await;

    let (resolved, _) = env.resolve_base("fuchsia-pkg://fuchsia.com/a-base-package").await.unwrap();
    let () = base_pkg.verify_contents(&resolved).await.unwrap();
}

#[fuchsia::test]
async fn resolve_system_image() {
    let env = TestEnv::builder().build().await;

    let (resolved, _) = env.resolve_base("fuchsia-pkg://fuchsia.com/system_image/0").await.unwrap();

    assert_eq!(
        fuchsia_pkg::PackageDirectory::from_proxy(resolved)
            .meta_package()
            .await
            .unwrap()
            .into_path(),
        system_image::SystemImage::package_path()
    );
}

#[fuchsia::test]
async fn resolve_with_context_absolute_url_package() {
    let base_pkg = PackageBuilder::new("a-base-package").build().await.unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&base_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&base_pkg])
        .await
        .build()
        .await;

    let (resolved, _) = env
        .resolve_with_context_base(
            "fuchsia-pkg://fuchsia.com/a-base-package/0",
            &fpkg::ResolutionContext { bytes: vec![] },
        )
        .await
        .unwrap();

    let () = base_pkg.verify_contents(&resolved).await.unwrap();
}

#[fuchsia::test]
async fn resolve_with_context_relative_url_package() {
    let sub_sub_pkg = PackageBuilder::new("sub-sub-package").build().await.unwrap();
    let sub_pkg = PackageBuilder::new("sub-package")
        .add_subpackage("sub-sub-package-url", &sub_sub_pkg)
        .build()
        .await
        .unwrap();
    let super_pkg = PackageBuilder::new("super-package")
        .add_subpackage("sub-package-url", &sub_pkg)
        .build()
        .await
        .unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&super_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&super_pkg])
        .await
        .build()
        .await;
    let (_dir, context) =
        env.resolve_base("fuchsia-pkg://fuchsia.com/super-package/0").await.unwrap();

    let (resolved, context) =
        env.resolve_with_context_base("sub-package-url", &context).await.unwrap();
    let () = sub_pkg.verify_contents(&resolved).await.unwrap();

    let (resolved, _) =
        env.resolve_with_context_base("sub-sub-package-url", &context).await.unwrap();
    let () = sub_sub_pkg.verify_contents(&resolved).await.unwrap();
}

#[fuchsia::test]
async fn manipulated_context_rejected() {
    let sub_pkg = PackageBuilder::new("sub-package").build().await.unwrap();
    let super_pkg = PackageBuilder::new("super-package")
        .add_subpackage("sub-package-url", &sub_pkg)
        .build()
        .await
        .unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&super_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&super_pkg])
        .await
        .build()
        .await;
    let (_, mut context) =
        env.resolve_base("fuchsia-pkg://fuchsia.com/super-package/0").await.unwrap();
    context.bytes[0] = !context.bytes[0];

    assert_matches!(
        env.resolve_with_context_base("sub-package-url", &context).await,
        Err(fpkg::ResolveError::InvalidContext)
    );
}

#[fuchsia::test]
async fn resolve_component() {
    let manifest = fidl::persist(&fcomponent_decl::Component {
        config: Some(fcomponent_decl::ConfigSchema {
            value_source: Some(fcomponent_decl::ConfigValueSource::PackagePath(
                "meta/config-data.cvf".to_string(),
            )),
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    let config_data = fidl::persist(&fcomponent_decl::ConfigValuesData::default()).unwrap();
    let base_pkg =
        PackageBuilder::new_with_abi_revision("a-base-package", 0x601665c5b1a89c7f.into())
            .add_resource_at("meta/manifest.cm", &*manifest)
            .add_resource_at("meta/config-data.cvf", &*config_data)
            .build()
            .await
            .unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&base_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&base_pkg])
        .await
        .build()
        .await;

    let fcomponent_resolution::Component {
        decl,
        package,
        config_values,
        resolution_context,
        abi_revision,
        ..
    } = env
        .resolve_base_component("fuchsia-pkg://fuchsia.com/a-base-package/0#meta/manifest.cm")
        .await
        .unwrap();

    assert_eq!(mem_util::bytes_from_data(decl.as_ref().unwrap()).unwrap(), manifest);
    let fcomponent_resolution::Package { url, directory, .. } = package.unwrap();
    assert_eq!(url.unwrap(), "fuchsia-pkg://fuchsia.com/a-base-package/0");
    let () = base_pkg.verify_contents(&directory.unwrap().into_proxy()).await.unwrap();
    assert_eq!(mem_util::bytes_from_data(config_values.as_ref().unwrap()).unwrap(), config_data);
    assert!(resolution_context.is_some());
    assert_eq!(abi_revision, Some(0x601665c5b1a89c7f));
}

#[fuchsia::test]
async fn resolve_with_context_component() {
    let manifest = fidl::persist(&fcomponent_decl::Component::default()).unwrap();
    let sub_sub_pkg = PackageBuilder::new("a-sub-sub-package")
        .add_resource_at("meta/manifest.cm", &*manifest)
        .build()
        .await
        .unwrap();
    let sub_pkg = PackageBuilder::new("a-sub-package")
        .add_resource_at("meta/manifest.cm", &*manifest)
        .add_subpackage("sub-sub-package-url", &sub_sub_pkg)
        .build()
        .await
        .unwrap();
    let base_pkg = PackageBuilder::new("a-base-package")
        .add_resource_at("meta/manifest.cm", &*manifest)
        .add_subpackage("sub-package-url", &sub_pkg)
        .build()
        .await
        .unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&base_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&base_pkg])
        .await
        .build()
        .await;

    let component = env
        .resolve_with_context_base_component(
            "fuchsia-pkg://fuchsia.com/a-base-package/0#meta/manifest.cm",
            &fcomponent_resolution::Context { bytes: vec![] },
        )
        .await
        .unwrap();
    let component = env
        .resolve_with_context_base_component(
            "sub-package-url#meta/manifest.cm",
            &component.resolution_context.unwrap(),
        )
        .await
        .unwrap();

    let fcomponent_resolution::Component {
        decl,
        package,
        config_values,
        resolution_context,
        abi_revision,
        ..
    } = env
        .resolve_with_context_base_component(
            "sub-sub-package-url#meta/manifest.cm",
            &component.resolution_context.unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(mem_util::bytes_from_data(decl.as_ref().unwrap()).unwrap(), manifest);
    let fcomponent_resolution::Package { url, directory, .. } = package.unwrap();
    assert_eq!(url.unwrap(), "sub-sub-package-url");
    let () = sub_sub_pkg.verify_contents(&directory.unwrap().into_proxy()).await.unwrap();
    assert_eq!(config_values, None);
    assert!(resolution_context.is_some());
    assert_eq!(abi_revision, Some(0xeccea2f70acd6fc0));
}

#[fuchsia::test]
async fn resolve_component_with_percent_encoded_resource() {
    // The resolver must look up the manifest at the percent-decoded resource path.
    let manifest = fidl::persist(&fcomponent_decl::Component::default()).unwrap();
    let sub_pkg = PackageBuilder::new("a-sub-package")
        .add_resource_at("meta/a b%c.cm", &*manifest)
        .build()
        .await
        .unwrap();
    let base_pkg = PackageBuilder::new("a-base-package")
        .add_resource_at("meta/a b%c.cm", &*manifest)
        .add_subpackage("sub-package-url", &sub_pkg)
        .build()
        .await
        .unwrap();
    let system_image_package =
        SystemImageBuilder::new().static_packages(&[&base_pkg]).build().await;
    let env = TestEnv::builder()
        .blobfs_from_system_image_and_extra_packages(&system_image_package, &[&base_pkg])
        .await
        .build()
        .await;

    let component = env
        .resolve_base_component("fuchsia-pkg://fuchsia.com/a-base-package/0#meta/a%20b%25c.cm")
        .await
        .unwrap();
    assert_eq!(mem_util::bytes_from_data(component.decl.as_ref().unwrap()).unwrap(), manifest);

    let fcomponent_resolution::Component { decl, .. } = env
        .resolve_with_context_base_component(
            "sub-package-url#meta/a%20b%25c.cm",
            &component.resolution_context.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mem_util::bytes_from_data(decl.as_ref().unwrap()).unwrap(), manifest);
}
