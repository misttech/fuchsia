// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// NOTE: The doc comments on `IntetgrationTestCmd` and its fields appear as the helptext of
// `fx testgen`. Please run that command to make sure the output looks correct before
// submitting changes.

use crate::common::*;
use crate::flags;
use anyhow::{bail, Error};
use argh::FromArgs;
use chrono::Datelike;
use log::info;
use std::path::PathBuf;

/// Generates an integration test for a Fuchsia component.
#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "integration_test")]
pub(crate) struct IntegrationTestCmd {
    /// the absolute the path to the component-under-test's manifest.
    #[argh(option, short = 'm')]
    pub component_manifest: PathBuf,

    /// the absolute GN label of the fuchsia_component target under test. Example: //src/path/to:component.
    #[argh(option, short = 'l')]
    pub component_gn_label: String,

    /// the root directory where code will be generated. This directory must not exist.
    #[argh(option, short = 'o')]
    pub test_root: PathBuf,
}

/// Generates an integration test for a Fuchsia component.
///
/// Code is generated by copying the template files in templates/integration_test.
/// Each template uses the [handlebars](https://docs.rs/handlebars/latest/handlebars/) syntax.
///
/// Template variables:
///
///   component_name
///   component_exposed_protocols
///   component_gn_label
///   fidl_rust_crate_name
///   fidl_library_name
///   realm_factory_binary_name
///   rel_component_url
///   test_binary_name
///   test_package_name
///
/// See [`TemplateVars`] for the meaning of each variable.
impl IntegrationTestCmd {
    // TODO(127973): Add back support for C++
    pub async fn run(&self, flags: &flags::Flags) -> Result<(), Error> {
        let test_root = self.test_root.clone();
        if test_root.exists() {
            bail!("{} already exists. Please choose a directory location that does not already exist.", test_root.display());
        }

        let component_manifest = self.component_manifest.clone();
        if !component_manifest.exists() {
            bail!("{} does not exist.", self.component_manifest.display());
        }
        if !component_manifest.extension().is_some_and(|x| x == "cml") {
            bail!("component manifest must be a .cml file");
        }

        let component_gn_label = self.component_gn_label.clone();
        if component_gn_label == "" {
            bail!("component_gn_label must not be empty");
        }
        if !component_gn_label.starts_with("//") {
            bail!("component_gn_label must be absolute and start with '//'")
        }

        info!("Generating an integration test at {}", test_root.display());

        // Initialize template variables and source code.
        let component_name = path_file_stem(&self.component_manifest);
        let cml = load_cml_file(&self.component_manifest)?;

        let year = match &flags.year_override {
            None => format!("{}", chrono::Utc::now().year()),
            Some(year) => year.clone(),
        };

        // rustfmt mangles this. long lines are easier to read.
        #[rustfmt::skip]
        let gen = CodeGenerator::new()
            .with_template_vars(TemplateVars{
                year: year,
                component_name: component_name.clone(),
                component_exposed_protocols: var_component_exposed_protocols(&cml),
                component_gn_label: self.component_gn_label.to_string(),
                rel_component_url: var_rel_component_url(&component_name),
                test_binary_name: var_test_binary_name(&component_name),
                test_package_name: var_test_package_name(&component_name),
                realm_factory_binary_name: var_realm_factory_binary_name(&component_name),
                fidl_rust_crate_name: var_fidl_rust_crate_name(&component_name),
                fidl_library_name: var_fidl_library_name(&component_name)
            })
            .with_template(hbrs_template_file!("integration_test", "tests/BUILD.gn"))
            .with_template(hbrs_template_file!("integration_test", "tests/meta/test-root.cml"))
            .with_template(hbrs_template_file!("integration_test", "tests/meta/test-suite.cml"))
            .with_template(hbrs_template_file!("integration_test", "tests/src/main.rs"))
            .with_template(hbrs_template_file!("integration_test", "testing/fidl/BUILD.gn"))
            .with_template(hbrs_template_file!("integration_test", "testing/fidl/realm_factory.test.fidl"))
            .with_template(hbrs_template_file!("integration_test", "testing/realm-factory/BUILD.gn"))
            .with_template(hbrs_template_file!("integration_test", "testing/realm-factory/meta/default.cml"))
            .with_template(hbrs_template_file!("integration_test", "testing/realm-factory/src/main.rs"));
        gen.generate(test_root)
    }
}
