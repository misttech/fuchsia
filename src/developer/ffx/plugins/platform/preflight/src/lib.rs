// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::config::{OperatingSystem, PreflightConfig};
use anyhow::{Result, anyhow};
use check::{PreflightCheck, PreflightCheckResult, RunSummary, summarize_results};
use ffx_preflight_args::PreflightCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use regex::Regex;
use std::io::{self, Write};
use termion::color;
use textwrap::{Options, WordSeparator, WordSplitter};

mod analytics;
mod check;
mod command_runner;
mod config;
mod json;

const SUCCESS_MARKER: &str = "[\u{2713}]";
const WARNING_MARKER: &str = "[!]";
const FAILURE_MARKER: &str = "[\u{2717}]";

const DEFAULT_OUTPUT_WIDTH: usize = 80;
// Right-align the status marker within this width, including a trailing space.
const RESULT_PREFIX_WIDTH: usize = 6;

// String constants for output.
static RUNNING_CHECKS_PREAMBLE: &str = "Running pre-flight checks...";
static SOME_CHECKS_FAILED_RECOVERABLE: &str =
    "Some checks failed :(. Follow the instructions above and try running again.";
static SOME_CHECKS_FAILED_FATAL: &str = "Some checks failed :(. Sorry!";
static EVERYTING_CHECKS_OUT: &str =
    "Everything checks out! Continue at https://fuchsia.dev/fuchsia-src/get-started";
static EVERYTING_CHECKS_OUT_WITH_WARNINGS: &str = "There were some warnings, but you can still carry on. Continue at https://fuchsia.dev/fuchsia-src/get-started";

#[cfg(target_os = "linux")]
fn get_operating_system() -> Result<OperatingSystem> {
    Ok(OperatingSystem::Linux)
}

#[cfg(target_os = "macos")]
fn get_operating_system() -> Result<OperatingSystem> {
    get_operating_system_macos(&command_runner::SYSTEM_COMMAND_RUNNER)
}

#[allow(dead_code)]
fn get_operating_system_macos(
    command_runner: &command_runner::CommandRunner,
) -> Result<OperatingSystem> {
    let (status, stdout, _) =
        (command_runner)(&vec!["defaults", "read", "loginwindow", "SystemVersionStampAsString"])
            .expect("Could not get MacOS version string");
    assert!(status.success());

    let re = Regex::new(r"(\d+)\.(\d+)(?:\.\d+)?")?;
    let caps =
        re.captures(&stdout).ok_or_else(|| anyhow!("unexpected output from `defaults read`"))?;
    let major: u32 = caps.get(1).unwrap().as_str().parse()?;
    let minor: u32 = caps.get(2).unwrap().as_str().parse()?;
    Ok(OperatingSystem::MacOS(major, minor))
}

#[derive(FfxTool)]
pub struct PreflightTool {
    #[command]
    pub cmd: PreflightCommand,
    pub context: ffx_config::EnvironmentContext,
}

fho::embedded_plugin!(PreflightTool);

#[async_trait::async_trait(?Send)]
impl FfxMain for PreflightTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        preflight_cmd_impl(&self.context, self.cmd, &mut writer).await.map_err(Into::into)
    }
}

pub async fn preflight_cmd_impl<W: Write>(
    context: &ffx_config::EnvironmentContext,
    cmd: PreflightCommand,
    writer: &mut W,
) -> Result<()> {
    let config = PreflightConfig { system: get_operating_system()? };
    let checks: Vec<Box<dyn PreflightCheck + '_>> = vec![
        Box::new(check::build_prereqs::BuildPrereqs::new(&command_runner::SYSTEM_COMMAND_RUNNER)),
        Box::new(check::femu_graphics::FemuGraphics::new(&command_runner::SYSTEM_COMMAND_RUNNER)),
        Box::new(check::emu_networking::EmuNetworking::new(&command_runner::SYSTEM_COMMAND_RUNNER)),
        Box::new(check::emu_acceleration::EmuAcceleration::new(
            &command_runner::SYSTEM_COMMAND_RUNNER,
        )),
        Box::new(check::ssh_checks::SshChecks::new(
            &command_runner::SYSTEM_COMMAND_RUNNER,
            context,
        )),
    ];

    let results = run_preflight_checks(&checks, &config).await?;
    if cmd.json {
        writeln!(writer, "{}", serde_json::to_string(&json::results_to_json(&results)?)?)?;
    } else {
        report_result_analytics(&results).await;
        let width = output_width(termion::terminal_size().ok());
        write_preflight_results(writer, &results, width)?;
    }

    Ok(())
}

async fn run_preflight_checks(
    checks: &[Box<dyn PreflightCheck + '_>],
    config: &PreflightConfig,
) -> Result<Vec<check::PreflightCheckResult>> {
    let mut results = vec![];
    for check in checks {
        results.push(check.run(&config).await?);
    }
    Ok(results)
}

async fn report_result_analytics(results: &Vec<PreflightCheckResult>) {
    let summary = summarize_results(results);
    let action = match summary {
        RunSummary::Success => analytics::ANALYTICS_ACTION_SUCCESS,
        RunSummary::Warning => analytics::ANALYTICS_ACTION_WARNING,
        RunSummary::RecoverableFailure => analytics::ANALYTICS_ACTION_FAILURE_RECOVERABLE,
        RunSummary::Failure => analytics::ANALYTICS_ACTION_FAILURE,
    };
    analytics::report_preflight_analytics(action).await;
}

fn output_width(terminal_size: Option<(u16, u16)>) -> usize {
    // termion queries stdout, so redirected output also uses the fallback width.
    terminal_size
        .filter(|(columns, _)| *columns > 0)
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(DEFAULT_OUTPUT_WIDTH)
}

fn write_preflight_results<W: Write>(
    writer: &mut W,
    results: &Vec<PreflightCheckResult>,
    width: usize,
) -> Result<()> {
    writeln!(writer, "{}", textwrap::fill(RUNNING_CHECKS_PREAMBLE, wrap_options(width)))?;
    writeln!(writer)?;
    for result in results.iter() {
        write_preflight_result(writer, result, width)?;
        writeln!(writer)?;
    }

    let summary = summarize_results(results);

    match summary {
        RunSummary::Success => {
            writeln!(writer, "{}", textwrap::fill(EVERYTING_CHECKS_OUT, wrap_options(width)))?;
        }
        RunSummary::Warning => {
            writeln!(
                writer,
                "{}",
                textwrap::fill(EVERYTING_CHECKS_OUT_WITH_WARNINGS, wrap_options(width))
            )?;
        }
        RunSummary::RecoverableFailure => {
            anyhow::bail!("{}", SOME_CHECKS_FAILED_RECOVERABLE);
        }
        RunSummary::Failure => {
            anyhow::bail!("{}", SOME_CHECKS_FAILED_FATAL);
        }
    };
    Ok(())
}

fn wrap_options(width: usize) -> Options<'static> {
    // Keep URLs and command-line tokens intact, even when they exceed the width.
    // Avoid inserting line breaks into URLs so terminals can recognize the complete link.
    Options::new(width)
        .word_separator(WordSeparator::AsciiSpace)
        .word_splitter(WordSplitter::NoHyphenation)
        .break_words(false)
}

fn wrap_text(input: &str, width: usize, indent_all: bool) -> String {
    let content_width = width.saturating_sub(RESULT_PREFIX_WIDTH).max(1);
    let indent = " ".repeat(RESULT_PREFIX_WIDTH);
    let lines = textwrap::wrap(input, wrap_options(content_width));
    let mut indented_lines = vec![];
    for line in lines {
        indented_lines.push(textwrap::indent(
            &line,
            if indent_all || !indented_lines.is_empty() { &indent } else { "" },
        ));
    }
    indented_lines.join("\n")
}

fn write_preflight_result<W: Write>(
    writer: &mut W,
    result: &PreflightCheckResult,
    width: usize,
) -> io::Result<()> {
    let marker_width = RESULT_PREFIX_WIDTH - 1;
    match result {
        PreflightCheckResult::Success(message) => write!(
            writer,
            "{}{SUCCESS_MARKER:>marker_width$}{} {}",
            color::Fg(color::Green),
            color::Fg(color::Reset),
            wrap_text(message, width, false)
        ),
        PreflightCheckResult::Warning(message) => write!(
            writer,
            "{}{WARNING_MARKER:>marker_width$}{} {}",
            color::Fg(color::Yellow),
            color::Fg(color::Reset),
            wrap_text(message, width, false)
        ),
        PreflightCheckResult::Failure(message, resolution) => {
            write!(
                writer,
                "{}{FAILURE_MARKER:>marker_width$}{} {}",
                color::Fg(color::Red),
                color::Fg(color::Reset),
                wrap_text(message, width, false),
            )?;
            match resolution {
                Some(resolution_message) => {
                    write!(writer, "\n\n{}", wrap_text(resolution_message, width, true))
                }
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::command_runner::ExitStatus;
    use async_trait::async_trait;

    #[fuchsia::test]
    fn output_width_uses_terminal_columns_or_falls_back() {
        assert_eq!(output_width(Some((120, 24))), 120);
        assert_eq!(output_width(Some((40, 24))), 40);
        assert_eq!(output_width(Some((0, 0))), DEFAULT_OUTPUT_WIDTH);
        assert_eq!(output_width(None), DEFAULT_OUTPUT_WIDTH);
    }

    #[fuchsia::test]
    fn wrap_text_wraps_and_indents_long_messages() {
        let message = "KVM is not enabled for the current user. This will prevent emulator \
                       acceleration from working with the Fuchsia emulator.";

        for width in [40, 80, 120] {
            for indent_all in [false, true] {
                let output = wrap_text(message, width, indent_all);
                let lines: Vec<_> = output.lines().collect();

                assert!(lines.len() > 1, "Expected a wrapped message: {output:?}");
                assert_eq!(lines[0].starts_with("      "), indent_all);
                assert!(lines[1..].iter().all(|line| line.starts_with("      ")));
                for (index, line) in lines.iter().enumerate() {
                    let prefix_width =
                        if index == 0 && !indent_all { RESULT_PREFIX_WIDTH } else { 0 };
                    assert!(
                        line.len() + prefix_width <= width,
                        "Line exceeds {width} columns: {line:?}"
                    );
                }
                assert_eq!(
                    lines.iter().map(|line| line.trim_start()).collect::<Vec<_>>().join(" "),
                    message
                );
            }
        }
    }

    #[fuchsia::test]
    fn wrap_text_uses_wide_terminals() {
        let message = "This message is longer than eighty columns but fits on a wider terminal \
                       without wrapping.";
        assert!(wrap_text(message, 80, false).contains('\n'));
        assert_eq!(wrap_text(message, 120, false), message);
    }

    #[fuchsia::test]
    fn wrap_text_handles_widths_smaller_than_the_indent() {
        for width in [0, 1, 6] {
            assert_eq!(wrap_text("a b", width, false), "a\n      b");
            assert_eq!(wrap_text("ab", width, false), "ab");
        }
    }

    #[fuchsia::test]
    fn wrap_text_preserves_urls() {
        for url in [
            "https://fuchsia.dev/fuchsia-src/development/build/emulator#supported-hardware",
            "https://fuchsia.dev/fuchsia-src/get-started/set_up_femu#enable-vm-acceleration",
            "http://example.com/long-path?query=some-value#section-name",
        ] {
            let message = format!("See {url} for more information.");
            for width in [40, 80, 120] {
                for indent_all in [false, true] {
                    let output = wrap_text(&message, width, indent_all);
                    assert!(output.contains(url), "URL was split at width {width}: {output:?}");
                    assert_eq!(
                        output.lines().map(str::trim_start).collect::<Vec<_>>().join(" "),
                        message
                    );
                }
            }
        }
    }

    #[fuchsia::test]
    fn wrap_text_preserves_paragraphs() {
        let message = "Warning message.\n\nSuggested resolution.";

        assert_eq!(
            wrap_text(message, DEFAULT_OUTPUT_WIDTH, false),
            "Warning message.\n\n      Suggested resolution."
        );
        assert_eq!(
            wrap_text(message, DEFAULT_OUTPUT_WIDTH, true),
            "      Warning message.\n\n      Suggested resolution."
        );
    }

    #[fuchsia::test]
    fn preflight_report_wraps_prose_and_preserves_links() -> Result<()> {
        let message = "This message is longer than eighty columns but fits on a wider terminal \
                       without wrapping.";
        let results = vec![
            PreflightCheckResult::Success(message.to_string()),
            PreflightCheckResult::Warning(message.to_string()),
        ];
        let color_escape = Regex::new(r"\x1b\[[0-9;]*m")?;

        for width in [40, 80, 120] {
            let mut buffer = Vec::new();
            write_preflight_results(&mut buffer, &results, width)?;
            let output = String::from_utf8(buffer)?;
            let output = color_escape.replace_all(&output, "");
            let summary_url = "https://fuchsia.dev/fuchsia-src/get-started";
            assert!(output.contains(summary_url));
            for line in output.lines() {
                if line.chars().count() > width {
                    assert_eq!(line.trim_start(), summary_url);
                }
            }
            if width == 120 {
                assert!(output.contains(&format!("  [!] {message}\n")));
            }
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_parse_macos_version() -> Result<()> {
        let mut run_command: command_runner::CommandRunner = |args| {
            assert_eq!(
                args.to_vec(),
                vec!["defaults", "read", "loginwindow", "SystemVersionStampAsString"]
            );
            Ok((ExitStatus(0), "10.15.17\n\n".to_string(), "".to_string()))
        };

        assert_eq!(OperatingSystem::MacOS(10, 15), get_operating_system_macos(&run_command)?);

        run_command = |args| {
            assert_eq!(
                args.to_vec(),
                vec!["defaults", "read", "loginwindow", "SystemVersionStampAsString"]
            );
            Ok((ExitStatus(0), "11.1\n\n".to_string(), "".to_string()))
        };

        assert_eq!(OperatingSystem::MacOS(11, 1), get_operating_system_macos(&run_command)?);
        Ok(())
    }

    struct SuccessCheck {}

    #[async_trait(?Send)]
    impl PreflightCheck for SuccessCheck {
        async fn run(&self, _config: &PreflightConfig) -> Result<PreflightCheckResult> {
            Ok(PreflightCheckResult::Success("This check passed!".to_string()))
        }
    }

    #[fuchsia::test]
    async fn run_checks_success() -> Result<()> {
        let config = PreflightConfig { system: OperatingSystem::Linux };
        let checks: Vec<Box<dyn PreflightCheck>> = vec![Box::new(SuccessCheck {})];
        let mut buf = Vec::new();
        let results = run_preflight_checks(&checks, &config).await?;
        let result = write_preflight_results(&mut buf, &results, DEFAULT_OUTPUT_WIDTH);
        let output = String::from_utf8(buf)?;
        // Check for the various output strings.
        assert!(output.starts_with(RUNNING_CHECKS_PREAMBLE));
        assert!(output.contains("This check passed!"));
        assert!(output.contains("Everything checks out!"));
        result
    }

    struct FailPermanentCheck {}
    struct FailRecoverableCheck {}

    #[async_trait(?Send)]
    impl PreflightCheck for FailPermanentCheck {
        async fn run(&self, _config: &PreflightConfig) -> Result<PreflightCheckResult> {
            Ok(PreflightCheckResult::Failure("Oh no...".to_string(), None))
        }
    }

    #[async_trait(?Send)]
    impl PreflightCheck for FailRecoverableCheck {
        async fn run(&self, _config: &PreflightConfig) -> Result<PreflightCheckResult> {
            Ok(PreflightCheckResult::Failure(
                "We will get through this.".to_string(),
                Some("Take a deep breath and try again.".to_string()),
            ))
        }
    }

    #[fuchsia::test]
    async fn run_checks_fail_nonrecoverable() -> Result<()> {
        let config = PreflightConfig { system: OperatingSystem::Linux };
        let checks: Vec<Box<dyn PreflightCheck>> = vec![
            Box::new(SuccessCheck {}),
            Box::new(FailPermanentCheck {}),
            Box::new(FailRecoverableCheck {}),
        ];
        let mut buf = Vec::new();
        let results = run_preflight_checks(&checks, &config).await?;
        let result = write_preflight_results(&mut buf, &results, DEFAULT_OUTPUT_WIDTH);
        let output = String::from_utf8(buf)?;
        // Check for the various output strings.
        assert!(output.starts_with(RUNNING_CHECKS_PREAMBLE), "{:?}", output);
        assert!(output.contains("This check passed!"), "{:?}", output);
        assert!(output.contains("Oh no..."), "{:?}", output);
        match result {
            Err(error) => {
                assert!(
                    error.to_string().contains(SOME_CHECKS_FAILED_FATAL),
                    "{}",
                    error.to_string()
                );
                assert!(
                    !error.to_string().contains(SOME_CHECKS_FAILED_RECOVERABLE),
                    "{}",
                    error.to_string()
                );
                Ok(())
            }
            Ok(_) => unreachable!(),
        }
    }

    #[fuchsia::test]
    async fn run_checks_fail_recoverable() -> Result<()> {
        let config = PreflightConfig { system: OperatingSystem::Linux };
        let checks: Vec<Box<dyn PreflightCheck>> = vec![
            Box::new(SuccessCheck {}),
            Box::new(FailRecoverableCheck {}),
            Box::new(FailRecoverableCheck {}),
        ];
        let mut buf = Vec::new();
        let results = run_preflight_checks(&checks, &config).await?;
        let result = write_preflight_results(&mut buf, &results, DEFAULT_OUTPUT_WIDTH);
        let output = String::from_utf8(buf)?;
        // Check for the various output strings.
        assert!(output.starts_with(RUNNING_CHECKS_PREAMBLE), "{:?}", output);
        assert!(output.contains("This check passed!"), "{:?}", output);
        assert!(output.contains("We will get through this."), "{:?}", output);
        assert!(output.contains("Take a deep breath and try again."), "{:?}", output);
        match result {
            Err(error) => {
                assert!(
                    !error.to_string().contains(SOME_CHECKS_FAILED_FATAL),
                    "{}",
                    error.to_string()
                );
                assert!(
                    error.to_string().contains(SOME_CHECKS_FAILED_RECOVERABLE),
                    "{}",
                    error.to_string()
                );
                Ok(())
            }
            Ok(_) => unreachable!(),
        }
    }
}
