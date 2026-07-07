// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::format::command_to_bstring;
use crate::parser::ast::{ASTBuilder, CommandTag, ResolvedWordPart, WordPartTag};
use crate::parser::{parse_script, tokenize};
use bstr::{BStr, BString};

fn parse_cmd(s: &str) -> String {
    let mut builder = ASTBuilder::new();
    let tokens = tokenize(BStr::new(s)).unwrap();
    let cmds = parse_script(&mut builder, &tokens).unwrap();
    let cmd_ptr = builder.add_sequence_or_single(&cmds);
    let bstr = command_to_bstring(builder.get_ref(cmd_ptr), &builder);
    bstr.to_string()
}

#[test]
fn test_fmt_simple() {
    assert_eq!(parse_cmd("echo foo bar"), "echo foo bar");
}

#[test]
fn test_fmt_pipe() {
    assert_eq!(parse_cmd("echo foo | grep bar"), "echo foo | grep bar");
}

#[test]
fn test_fmt_redir() {
    assert_eq!(parse_cmd("echo foo > bar"), "echo foo <redirection>");
}

#[test]
fn test_fmt_subshell() {
    assert_eq!(parse_cmd("( echo sub )"), "( echo sub )");
}

#[test]
fn test_fmt_bg() {
    assert_eq!(parse_cmd("echo bg &"), "echo bg &");
}

#[test]
fn test_fmt_vars() {
    assert_eq!(parse_cmd("echo $VAR \"$QUOTED\""), "echo $VAR $QUOTED");
}

#[test]
fn test_fmt_cmdsub() {
    assert_eq!(parse_cmd("echo $(ls) \"$(pwd)\""), "echo $(ls) $(pwd)");
}

#[test]
fn test_fmt_arith() {
    assert_eq!(parse_cmd("echo $((1 + 2)) \"$((3 + 4))\""), "echo $((1 + 2)) $((3 + 4))");
}

#[test]
#[should_panic(expected = "invalid CommandTag: 99")]
fn test_fallback_command_tag() {
    let mut builder = ASTBuilder::new();
    let child_ptr = builder.add_empty_simple_command();
    let cmd_ptr = builder.add_unary_command(CommandTag(99), child_ptr);
    let _ = command_to_bstring(builder.get_ref(cmd_ptr), &builder);
}

#[test]
#[should_panic(expected = "invalid WordPartTag: 99")]
fn test_fallback_word_part_tag() {
    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("test"))];
    let w_slice = builder.add_resolved_word(&parts);
    let parts_mut = builder.get_mut(w_slice.cast());
    parts_mut.tag = WordPartTag(99);
    let simple_ptr = builder.add_simple_command(&[w_slice]);
    let _ = command_to_bstring(builder.get_ref(simple_ptr), &builder);
}
