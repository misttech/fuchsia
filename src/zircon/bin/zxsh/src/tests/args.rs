// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::parse_args;
use bstr::BString;

fn to_bstrings(args: &[&str]) -> Vec<BString> {
    args.iter().map(|&s| BString::from(s)).collect()
}

#[test]
fn test_parse_empty() {
    let args = to_bstrings(&["zxsh"]);
    let parsed = parse_args(&args).unwrap();
    assert!(parsed.command.is_none());
    assert!(!parsed.stdin);
    assert!(!parsed.opt_interactive);
    assert!(parsed.script_name.is_none());
    assert!(parsed.positional_args.is_empty());
}

#[test]
fn test_parse_c_option() {
    // -c command
    let args = to_bstrings(&["zxsh", "-c", "echo foo"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.command.as_ref().unwrap(), "echo foo");
    assert!(parsed.script_name.is_none());
    assert!(parsed.positional_args.is_empty());

    // -c command script_name args...
    let args = to_bstrings(&["zxsh", "-c", "echo foo", "my_script", "arg1", "arg2"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.command.as_ref().unwrap(), "echo foo");
    assert_eq!(parsed.script_name.as_ref().unwrap(), "my_script");
    assert_eq!(parsed.positional_args, to_bstrings(&["arg1", "arg2"]));

    // Attached -ccommand
    let args = to_bstrings(&["zxsh", "-cecho foo", "my_script", "arg1"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.command.as_ref().unwrap(), "echo foo");
    assert_eq!(parsed.script_name.as_ref().unwrap(), "my_script");
    assert_eq!(parsed.positional_args, to_bstrings(&["arg1"]));
}

#[test]
fn test_parse_flags() {
    let args = to_bstrings(&["zxsh", "-xive", "script.sh"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.opt_xtrace, Some(true));
    assert_eq!(parsed.opt_interactive, true);
    assert_eq!(parsed.opt_verbose, Some(true));
    assert_eq!(parsed.opt_errexit, Some(true));
    assert_eq!(parsed.script_name.as_ref().unwrap(), "script.sh");
}

#[test]
fn test_parse_plus_flags() {
    let args = to_bstrings(&["zxsh", "+x", "-e", "script.sh"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.opt_xtrace, Some(false));
    assert_eq!(parsed.opt_errexit, Some(true));
    assert_eq!(parsed.script_name.as_ref().unwrap(), "script.sh");
}

#[test]
fn test_parse_o_option() {
    let args = to_bstrings(&["zxsh", "-o", "xtrace", "+o", "noglob", "script.sh"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.options_to_set, to_bstrings(&["xtrace"]));
    assert_eq!(parsed.options_to_clear, to_bstrings(&["noglob"]));
    assert_eq!(parsed.script_name.as_ref().unwrap(), "script.sh");

    // Attached -ooption
    let args = to_bstrings(&["zxsh", "-oxtrace", "script.sh"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.options_to_set, to_bstrings(&["xtrace"]));
    assert_eq!(parsed.script_name.as_ref().unwrap(), "script.sh");
}

#[test]
fn test_parse_stdin() {
    // -s
    let args = to_bstrings(&["zxsh", "-s", "arg1", "arg2"]);
    let parsed = parse_args(&args).unwrap();
    assert!(parsed.stdin);
    assert!(parsed.script_name.is_none());
    assert_eq!(parsed.positional_args, to_bstrings(&["arg1", "arg2"]));

    // -
    let args = to_bstrings(&["zxsh", "-", "arg1"]);
    let parsed = parse_args(&args).unwrap();
    assert!(parsed.stdin);
    assert!(parsed.script_name.is_none());
    assert_eq!(parsed.positional_args, to_bstrings(&["arg1"]));
}

#[test]
fn test_parse_double_dash() {
    let args = to_bstrings(&["zxsh", "-x", "--", "-c", "script.sh"]);
    let parsed = parse_args(&args).unwrap();
    assert_eq!(parsed.opt_xtrace, Some(true));
    assert!(parsed.command.is_none());
    assert_eq!(parsed.script_name.as_ref().unwrap(), "-c");
    assert_eq!(parsed.positional_args, to_bstrings(&["script.sh"]));
}
