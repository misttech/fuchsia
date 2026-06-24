// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::{RawWordPart, Token, tokenize};
use bstr::{BStr, ByteSlice};

#[test]
fn test_tokenizer_words_quotes_escapes() {
    // Unquoted words
    let tokens = tokenize(b"hello world").unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0].as_unquoted_bstr(), Some(BStr::new(b"hello")));
    assert_eq!(tokens[1].as_unquoted_bstr(), Some(BStr::new(b"world")));

    // Escaped spaces in unquoted word
    let tokens = tokenize(b"hello\\ world").unwrap();
    assert_eq!(tokens.len(), 1);
    if let Token::Word(parts) = &tokens[0] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::Literal("hello world".into()));
    } else {
        panic!("Expected Token::Word");
    }

    // Single quotes (preserve everything)
    let tokens = tokenize(b"'hello $world'").unwrap();
    assert_eq!(tokens.len(), 1);
    if let Token::Word(parts) = &tokens[0] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::QuotedLiteral("hello $world".into()));
    } else {
        panic!("Expected Token::Word");
    }

    // Double quotes (variables and substitutions)
    let tokens = tokenize(b"\"hello $world $(cmd)\"").unwrap();
    assert_eq!(tokens.len(), 1);
    if let Token::Word(parts) = &tokens[0] {
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], RawWordPart::QuotedLiteral("hello ".into()));
        assert_eq!(parts[1], RawWordPart::QuotedVar("world".into()));
        assert_eq!(parts[2], RawWordPart::QuotedLiteral(" ".into()));
        assert_eq!(parts[3], RawWordPart::QuotedCommandSubstitution("cmd".into()));
    } else {
        panic!("Expected Token::Word");
    }

    // Empty double quotes
    let tokens = tokenize(b"\"\"").unwrap();
    assert_eq!(tokens.len(), 1);
    if let Token::Word(parts) = &tokens[0] {
        assert_eq!(parts.len(), 0);
    } else {
        panic!("Expected Token::Word");
    }
}

#[test]
fn test_tokenizer_operators() {
    let tokens = tokenize(b"a; b & c | d && e || f ;; g\nh").unwrap();
    assert_eq!(tokens.len(), 15);
    assert!(matches!(tokens[0], Token::Word(_)));
    assert_eq!(tokens[1], Token::Semi);
    assert!(matches!(tokens[2], Token::Word(_)));
    assert_eq!(tokens[3], Token::Ampersand);
    assert!(matches!(tokens[4], Token::Word(_)));
    assert_eq!(tokens[5], Token::Pipe);
    assert!(matches!(tokens[6], Token::Word(_)));
    assert_eq!(tokens[7], Token::And);
    assert!(matches!(tokens[8], Token::Word(_)));
    assert_eq!(tokens[9], Token::Or);
    assert!(matches!(tokens[10], Token::Word(_)));
    assert_eq!(tokens[11], Token::DoubleSemi);
    assert!(matches!(tokens[12], Token::Word(_)));
    assert_eq!(tokens[13], Token::Newline);
    assert!(matches!(tokens[14], Token::Word(_)));
}

#[test]
fn test_tokenizer_redirects() {
    let tokens = tokenize(b"cmd >file <file2 >>append 2>&1 2<&-").unwrap();
    assert_eq!(tokens.len(), 11);
    assert!(matches!(tokens[0], Token::Word(_)));

    assert_eq!(tokens[1], Token::RedirectOut(None));
    assert_eq!(tokens[2].as_unquoted_bstr(), Some(BStr::new(b"file")));

    assert_eq!(tokens[3], Token::RedirectIn(None));
    assert_eq!(tokens[4].as_unquoted_bstr(), Some(BStr::new(b"file2")));

    assert_eq!(tokens[5], Token::RedirectAppend(None));
    assert_eq!(tokens[6].as_unquoted_bstr(), Some(BStr::new(b"append")));

    assert_eq!(tokens[7], Token::RedirectDupOut(Some(2)));
    assert_eq!(tokens[8].as_unquoted_bstr(), Some(BStr::new(b"1")));

    assert_eq!(tokens[9], Token::RedirectDupIn(Some(2)));
    assert_eq!(tokens[10].as_unquoted_bstr(), Some(BStr::new(b"-")));
}

#[test]
fn test_tokenizer_subshells_and_vars() {
    let tokens = tokenize(b"$(simple) `backtick` $var ${var2} $((1+1))").unwrap();
    assert_eq!(tokens.len(), 5);

    if let Token::Word(parts) = &tokens[0] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::CommandSubstitution("simple".into()));
    } else {
        panic!();
    }

    if let Token::Word(parts) = &tokens[1] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::CommandSubstitution("backtick".into()));
    } else {
        panic!();
    }

    if let Token::Word(parts) = &tokens[2] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::Var("var".into()));
    } else {
        panic!();
    }

    if let Token::Word(parts) = &tokens[3] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::Var("var2".into()));
    } else {
        panic!();
    }

    if let Token::Word(parts) = &tokens[4] {
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], RawWordPart::Arithmetic("1+1".into()));
    } else {
        panic!();
    }
}

#[test]
fn test_tokenizer_heredocs() {
    // Simple heredoc
    let tokens = tokenize(b"cmd <<EOF\nline1\nline2\nEOF\n").unwrap();
    assert_eq!(tokens.len(), 3);
    assert!(matches!(tokens[0], Token::Word(_)));

    if let Token::RedirectHereDoc { src_fd, delimiter, body, expand } = &tokens[1] {
        assert_eq!(*src_fd, None);
        assert_eq!(delimiter.len(), 1);
        assert_eq!(delimiter[0], RawWordPart::Literal("EOF".into()));
        assert_eq!(body.as_bstr(), BStr::new(b"line1\nline2\n"));
        assert!(*expand);
    } else {
        panic!("Expected RedirectHereDoc, got {:?}", tokens[1]);
    }
    assert_eq!(tokens[2], Token::Newline);

    let tokens = tokenize(b"cmd <<\"EOF\"\nline1 $var\nEOF\n").unwrap();
    assert_eq!(tokens.len(), 3);
    if let Token::RedirectHereDoc { src_fd, delimiter, body, expand } = &tokens[1] {
        assert_eq!(*src_fd, None);
        assert_eq!(delimiter.len(), 1);
        assert_eq!(delimiter[0], RawWordPart::QuotedLiteral("EOF".into()));
        assert_eq!(body.as_bstr(), BStr::new(b"line1 $var\n"));
        assert!(!*expand);
    } else {
        panic!();
    }
    assert_eq!(tokens[2], Token::Newline);

    // Strip tabs
    let tokens = tokenize(b"cmd <<-EOF\n\tline1\n\tEOF\n").unwrap();
    assert_eq!(tokens.len(), 3);
    if let Token::RedirectHereDoc { src_fd, delimiter, body, expand } = &tokens[1] {
        assert_eq!(*src_fd, None);
        assert_eq!(delimiter.len(), 1);
        assert_eq!(delimiter[0], RawWordPart::Literal("EOF".into()));
        assert_eq!(body.as_bstr(), BStr::new(b"line1\n"));
        assert!(*expand);
    } else {
        panic!();
    }
    assert_eq!(tokens[2], Token::Newline);
}
