// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::{BString, ByteSlice};

/// Parsed command line arguments for the shell.
#[derive(Default, Debug, Clone)]
pub struct Args {
    /// Command to execute. If set, the shell executes this command and exits.
    /// Maps to the `-c` command-line option.
    pub command: Option<BString>,

    /// Read commands from the standard input.
    /// Maps to the `-s` command-line option, or triggers if no script name is provided
    /// and standard input is not a TTY.
    pub stdin: bool,

    /// Run as a login shell.
    /// Maps to the `-l` command-line option.
    pub login: bool,

    /// Shell options to enable.
    /// Maps to the `-o option_name` command-line option.
    pub options_to_set: Vec<BString>,

    /// Shell options to disable.
    /// Maps to the `+o option_name` command-line option.
    pub options_to_clear: Vec<BString>,

    // Single letter flags (can be set with `-` or cleared with `+`)
    /// Automatically export all variables that are defined or modified.
    /// Maps to the `-a` / `+a` option.
    pub opt_allexport: Option<bool>,

    /// Report the status of terminated background jobs immediately.
    /// Maps to the `-b` / `+b` option.
    pub opt_notify: Option<bool>,

    /// Prevent existing files from being overwritten by redirection.
    /// Maps to the `-C` / `+C` option.
    pub opt_noclobber: Option<bool>,

    /// Exit immediately if a command exits with a non-zero status.
    /// Maps to the `-e` / `+e` option.
    pub opt_errexit: Option<bool>,

    /// Disable pathname expansion (globbing).
    /// Maps to the `-f` / `+f` option.
    pub opt_noglob: Option<bool>,

    /// Force the shell to run interactively.
    /// Maps to the `-i` command-line option.
    pub opt_interactive: bool,

    /// Ignore EOF (Ctrl-D) when reading from stdin in an interactive shell.
    /// Maps to the `-I` / `+I` option.
    pub opt_ignoreeof: Option<bool>,

    /// Enable job control.
    /// Maps to the `-m` / `+m` option.
    pub opt_monitor: Option<bool>,

    /// Read commands but do not execute them. Useful for syntax checking.
    /// Maps to the `-n` / `+n` option.
    pub opt_noexec: Option<bool>,

    /// Print commands and their arguments as they are executed.
    /// Maps to the `-x` / `+x` option (xtrace).
    pub opt_xtrace: Option<bool>,

    /// Print shell input lines as they are read.
    /// Maps to the `-v` / `+v` option.
    pub opt_verbose: Option<bool>,

    /// Enable vi-style line editing.
    /// Maps to the `-V` / `+V` option.
    pub opt_vi: Option<bool>,

    /// Enable emacs-style line editing.
    /// Maps to the `-E` / `+E` option.
    pub opt_emacs: Option<bool>,

    /// Treat unset variables as an error when performing parameter expansion.
    /// Maps to the `-u` / `+u` option.
    pub opt_nounset: Option<bool>,

    /// The name of the script to run, if not running a command via `-c`.
    /// This becomes `$0` in the script.
    pub script_name: Option<BString>,

    /// Positional arguments passed to the script or command.
    /// These become `$1`, `$2`, etc.
    pub positional_args: Vec<BString>,
}

/// Parses the command line arguments into an `Args` structure.
///
/// POSIX shell argument parsing rules are followed:
/// - Arguments starting with `-` set options, while arguments starting with `+` clear them.
/// - Options can be grouped (e.g. `-xive`).
/// - The `-c` and `-o` options require arguments. They can be attached (e.g. `-cecho`)
///   or detached (e.g. `-c "echo"`).
/// - `--` marks the end of options. Subsequent arguments are treated as positionals.
/// - `-` forces reading from stdin and ends option parsing.
pub fn parse_args(args: &[BString]) -> Result<Args, String> {
    let mut result = Args::default();
    let mut iter = args.iter().skip(1); // skip argv[0]

    while let Some(arg) = iter.next() {
        let bytes = arg.as_bytes();
        if bytes == b"--" {
            break;
        }
        if bytes == b"-" {
            result.stdin = true;
            break;
        }
        if (bytes.starts_with(b"-") || bytes.starts_with(b"+")) && bytes.len() > 1 {
            let enable = bytes[0] == b'-';
            let mut chars = bytes[1..].iter().copied();
            while let Some(c) = chars.next() {
                match c {
                    b'c' => {
                        if !enable {
                            return Err("cannot unset -c".to_string());
                        }
                        let val = {
                            let rem: Vec<u8> = chars.collect();
                            if !rem.is_empty() {
                                BString::from(rem)
                            } else {
                                if let Some(next_arg) = iter.next() {
                                    next_arg.clone()
                                } else {
                                    return Err("-c requires an argument".to_string());
                                }
                            }
                        };
                        result.command = Some(val);
                        break;
                    }
                    b'o' => {
                        let val = {
                            let rem: Vec<u8> = chars.collect();
                            if !rem.is_empty() {
                                BString::from(rem)
                            } else {
                                if let Some(next_arg) = iter.next() {
                                    next_arg.clone()
                                } else {
                                    return Err(format!(
                                        "{}o requires an argument",
                                        if enable { "-" } else { "+" }
                                    ));
                                }
                            }
                        };
                        if enable {
                            result.options_to_set.push(val);
                        } else {
                            result.options_to_clear.push(val);
                        }
                        break;
                    }
                    b'a' => result.opt_allexport = Some(enable),
                    b'b' => result.opt_notify = Some(enable),
                    b'C' => result.opt_noclobber = Some(enable),
                    b'e' => result.opt_errexit = Some(enable),
                    b'f' => result.opt_noglob = Some(enable),
                    b'I' => result.opt_ignoreeof = Some(enable),
                    b'i' => result.opt_interactive = enable,
                    b'm' => result.opt_monitor = Some(enable),
                    b'n' => result.opt_noexec = Some(enable),
                    b's' => result.stdin = enable,
                    b'x' => result.opt_xtrace = Some(enable),
                    b'v' => result.opt_verbose = Some(enable),
                    b'V' => result.opt_vi = Some(enable),
                    b'E' => result.opt_emacs = Some(enable),
                    b'u' => result.opt_nounset = Some(enable),
                    b'l' => result.login = enable,
                    _ => {
                        return Err(format!(
                            "unknown option: {}{}",
                            if enable { "-" } else { "+" },
                            c as char
                        ));
                    }
                }
            }
        } else {
            let mut pos = vec![arg.clone()];
            pos.extend(iter.cloned());

            if result.command.is_some() {
                if !pos.is_empty() {
                    result.script_name = Some(pos.remove(0));
                    result.positional_args = pos;
                }
            } else if result.stdin {
                result.positional_args = pos;
            } else {
                if !pos.is_empty() {
                    result.script_name = Some(pos.remove(0));
                    result.positional_args = pos;
                }
            }
            return Ok(result);
        }
    }

    let mut pos: Vec<BString> = iter.cloned().collect();
    if result.command.is_some() {
        if !pos.is_empty() {
            result.script_name = Some(pos.remove(0));
            result.positional_args = pos;
        }
    } else if result.stdin {
        result.positional_args = pos;
    } else {
        if !pos.is_empty() {
            result.script_name = Some(pos.remove(0));
            result.positional_args = pos;
        }
    }

    Ok(result)
}

#[cfg(test)]
impl Args {
    /// Creates an `Args` structure with only positional arguments.
    /// Useful for testing and programmatic shell execution.
    pub fn with_positionals(script_name: BString, positional_args: Vec<BString>) -> Self {
        Self { script_name: Some(script_name), positional_args, ..Default::default() }
    }
}
