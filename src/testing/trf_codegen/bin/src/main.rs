// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// Entry point for the TRF Codegen CLI. Orchestrates parsing, linting, and artifact generation.

mod consistency_checker;
// mod generator;
mod parser;

use anyhow::{Context, Result};
use argh::FromArgs;
use consistency_checker::{ComponentManifest, check_consistency};
use parser::parse_source;
use std::io::{self, Read};

#[derive(FromArgs, Default)]
/// TRF Codegen Tool
struct Args {
    /// source file path (if not provided, reads from stdin)
    #[argh(option)]
    source: Option<String>,

    /// manifest file path
    #[argh(option)]
    manifest: Option<String>,

    /// out dir
    #[argh(option)]
    out_dir: Option<String>,

    /// target name for GN integration
    #[argh(option)]
    target_name: Option<String>,

    /// component under test name
    #[argh(option)]
    cut_component_name: Option<String>,

    /// mock paths
    #[argh(option)]
    mock: Vec<String>,

    /// fallback positional manifest file path
    #[argh(positional)]
    manifest_pos: Option<String>,
}

fn load_primary_source(source_path: Option<&String>) -> Result<String> {
    if let Some(path) = source_path {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read source code file at {}", path))
    } else {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("Failed to read Rust source code from standard input")?;
        Ok(input)
    }
}

fn load_mock_sources(mock_paths: &[String]) -> Result<Vec<String>> {
    let mut mock_sources = Vec::with_capacity(mock_paths.len());
    for path in mock_paths {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read mock source file at {}", path))?;
        mock_sources.push(content);
    }
    Ok(mock_sources)
}

struct CodeGenInputs<'a> {
    source_code: &'a str,
    mock_sources: &'a [String],
    manifest_content: Option<&'a str>,
    target_name: Option<&'a str>,
    cut_component_name: Option<&'a str>,
    mock_paths: &'a [String],
}

fn process_pipeline(inputs: CodeGenInputs<'_>) -> Result<()> {
    let parse_result = parse_source(inputs.source_code, inputs.mock_sources)
        .map_err(|err| anyhow::anyhow!("Failed to parse TRF AST annotations: {}", err))?;

    let manifest = inputs
        .manifest_content
        .map(|content| {
            serde_json::from_str::<ComponentManifest>(content)
                .context("Failed to parse component manifest JSON")
        })
        .transpose()?;

    let _auto_routes = check_consistency(&parse_result, manifest.as_ref()).map_err(|errors| {
        let error_messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        anyhow::anyhow!("Manifest linting errors:\n{}", error_messages.join("\n"))
    })?;

    // Suppress unused variable warnings for the fields that are only used in later CLs.
    let _ = inputs.target_name;
    let _ = inputs.cut_component_name;
    let _ = inputs.mock_paths;

    Ok(())
}

fn run_pipeline(args: Args) -> Result<()> {
    let source_code = load_primary_source(args.source.as_ref())?;
    let mock_sources = load_mock_sources(&args.mock)?;

    let manifest_content = args
        .manifest
        .as_ref()
        .or(args.manifest_pos.as_ref())
        .map(|path| {
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read manifest file at {}", path))
        })
        .transpose()?;

    let inputs = CodeGenInputs {
        source_code: &source_code,
        mock_sources: &mock_sources,
        manifest_content: manifest_content.as_deref(),
        target_name: args.target_name.as_deref(),
        cut_component_name: args.cut_component_name.as_deref(),
        mock_paths: &args.mock,
    };

    process_pipeline(inputs)?;

    if let Some(_dir) = args.out_dir {
        // File writing logic goes here ...
    }

    Ok(())
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    run_pipeline(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_mock_sources_empty() {
        let result = load_mock_sources(&[]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_process_pipeline_validates_basic_flow() {
        let source_code = "
            #[trf::test]
            fn test_foo() {}
        ";
        let inputs = CodeGenInputs {
            source_code,
            mock_sources: &[],
            manifest_content: None,
            target_name: Some("test_foo"),
            cut_component_name: Some("test_foo"),
            mock_paths: &[],
        };
        let result = process_pipeline(inputs);
        assert!(result.is_ok());
    }
}
