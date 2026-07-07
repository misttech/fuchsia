// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::ast::{Command, CommandTag, WordPart, WordPartTag};
use crate::relative;
use bstr::{BString, ByteVec};

/// Formats an AST `Command` node into a readable byte string (`BString`).
///
/// Used for command execution tracing (`set -x` / verbose mode) and error debugging.
/// Recursively formats pipelines (`|`), subshells (`(...)`), background jobs (`&`), and
/// individual command word parts (literals, variables, command substitutions, arithmetic).
pub fn command_to_bstring(command: &Command, buffer: &relative::Buffer) -> BString {
    let mut formatted = BString::default();
    match command.tag {
        CommandTag::SIMPLE => {
            let arguments = command.simple_args.as_slice(buffer);
            for (i, argument) in arguments.iter().enumerate() {
                if i > 0 {
                    formatted.push_byte(b' ');
                }
                formatted
                    .extend_from_slice(&argument_to_bstring(argument.as_slice(buffer), buffer));
            }
        }
        CommandTag::PIPELINE => {
            formatted.extend_from_slice(&command_to_bstring(command.left.as_ref(buffer), buffer));
            formatted.push_str(" | ");
            formatted.extend_from_slice(&command_to_bstring(command.right.as_ref(buffer), buffer));
        }
        CommandTag::REDIRECT => {
            formatted.extend_from_slice(&command_to_bstring(command.left.as_ref(buffer), buffer));
            formatted.push_str(" <redirection>");
        }
        CommandTag::SUBSHELL => {
            formatted.push_str("( ");
            formatted.extend_from_slice(&command_to_bstring(command.left.as_ref(buffer), buffer));
            formatted.push_str(" )");
        }
        CommandTag::BACKGROUND => {
            formatted.extend_from_slice(&command_to_bstring(command.left.as_ref(buffer), buffer));
            formatted.push_str(" &");
        }
        _ => unreachable!("invalid CommandTag: {}", command.tag.0),
    }
    formatted
}

fn argument_to_bstring(argument: &[WordPart], buffer: &relative::Buffer) -> BString {
    let mut formatted = BString::default();
    for word_part in argument {
        match word_part.tag {
            WordPartTag::LITERAL | WordPartTag::QUOTED_LITERAL => {
                formatted.extend_from_slice(word_part.text.as_bstr(buffer));
            }
            WordPartTag::VAR | WordPartTag::QUOTED_VAR => {
                formatted.push_byte(b'$');
                formatted.extend_from_slice(word_part.text.as_bstr(buffer));
            }
            WordPartTag::COMMAND_SUBSTITUTION | WordPartTag::QUOTED_COMMAND_SUBSTITUTION => {
                formatted.push_str("$(");
                formatted.extend_from_slice(&command_to_bstring(
                    word_part.command.as_ref(buffer),
                    buffer,
                ));
                formatted.push_byte(b')');
            }
            WordPartTag::ARITHMETIC | WordPartTag::QUOTED_ARITHMETIC => {
                formatted.push_str("$((");
                formatted.extend_from_slice(word_part.text.as_bstr(buffer));
                formatted.push_str("))");
            }
            _ => unreachable!("invalid WordPartTag: {}", word_part.tag.0),
        }
    }
    formatted
}
