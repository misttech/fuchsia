#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from collections.abc import Iterable, Sequence

import textpb


def unlex(tokens: Iterable[textpb.Token]) -> str:
    """Concatenates tokens' text."""
    return "".join(token.text for token in tokens)


class LexTests(unittest.TestCase):
    def _test_tokens(self, tokens: Sequence[textpb.Token]) -> None:
        text = unlex(tokens)
        lexed = list(textpb._lex_line(text))
        self.assertEqual(lexed, tokens)

    def test_start_block(self) -> None:
        self._test_tokens(
            [textpb.Token(text="<", type=textpb.TokenType.START_BLOCK)]
        )
        self._test_tokens(
            [textpb.Token(text="{", type=textpb.TokenType.START_BLOCK)]
        )

    def test_end_block(self) -> None:
        self._test_tokens(
            [textpb.Token(text=">", type=textpb.TokenType.END_BLOCK)]
        )
        self._test_tokens(
            [textpb.Token(text="}", type=textpb.TokenType.END_BLOCK)]
        )

    def test_empty_block(self) -> None:
        self._test_tokens(
            [
                textpb.Token(text="<", type=textpb.TokenType.START_BLOCK),
                textpb.Token(text=">", type=textpb.TokenType.END_BLOCK),
            ]
        )
        self._test_tokens(
            [
                textpb.Token(text="{", type=textpb.TokenType.START_BLOCK),
                textpb.Token(text="}", type=textpb.TokenType.END_BLOCK),
            ]
        )

    def test_spaces(self) -> None:
        self._test_tokens([textpb.Token(text=" ", type=textpb.TokenType.SPACE)])
        self._test_tokens(
            [textpb.Token(text="  ", type=textpb.TokenType.SPACE)]
        )
        self._test_tokens(
            [textpb.Token(text="\t", type=textpb.TokenType.SPACE)]
        )
        self._test_tokens(
            [textpb.Token(text="\t\t", type=textpb.TokenType.SPACE)]
        )

    def test_newlines(self) -> None:
        self._test_tokens(
            [textpb.Token(text="\n", type=textpb.TokenType.NEWLINE)]
        )
        self._test_tokens(
            [textpb.Token(text="\r\n", type=textpb.TokenType.NEWLINE)]
        )

    def test_field_name(self) -> None:
        self._test_tokens(
            [textpb.Token(text="field:", type=textpb.TokenType.FIELD_NAME)]
        )
        self._test_tokens(
            [textpb.Token(text="key:", type=textpb.TokenType.FIELD_NAME)]
        )
        self._test_tokens(
            [textpb.Token(text="value:", type=textpb.TokenType.FIELD_NAME)]
        )

    def test_strings(self) -> None:
        self._test_tokens(
            [textpb.Token(text='""', type=textpb.TokenType.STRING_VALUE)]
        )
        self._test_tokens(
            [textpb.Token(text='"string"', type=textpb.TokenType.STRING_VALUE)]
        )
        self._test_tokens(
            [
                textpb.Token(
                    text='"with space"', type=textpb.TokenType.STRING_VALUE
                )
            ]
        )
        self._test_tokens(
            [
                textpb.Token(
                    text='"\\"escaped\\""', type=textpb.TokenType.STRING_VALUE
                )
            ]
        )
        self._test_tokens(
            [
                textpb.Token(
                    text='"-DFOO=\\"https://b.f.org/e.cgi\\\\?prod=gShoe\\" "',
                    type=textpb.TokenType.STRING_VALUE,
                )
            ]
        )

    def test_nonstring_values(self) -> None:
        self._test_tokens(
            [textpb.Token(text="ENUM", type=textpb.TokenType.OTHER_VALUE)]
        )
        self._test_tokens(
            [textpb.Token(text="OTHER_ENUM", type=textpb.TokenType.OTHER_VALUE)]
        )
        self._test_tokens(
            [textpb.Token(text="123", type=textpb.TokenType.OTHER_VALUE)]
        )

    def test_combination(self) -> None:
        self._test_tokens(
            [
                textpb.Token(
                    text="field_name:", type=textpb.TokenType.FIELD_NAME
                ),
                textpb.Token(text=" ", type=textpb.TokenType.SPACE),
                textpb.Token(text="{", type=textpb.TokenType.START_BLOCK),
                textpb.Token(text="\n", type=textpb.TokenType.NEWLINE),
                textpb.Token(text="key:", type=textpb.TokenType.FIELD_NAME),
                textpb.Token(text=" ", type=textpb.TokenType.SPACE),
                textpb.Token(
                    text='"path/to/something"',
                    type=textpb.TokenType.STRING_VALUE,
                ),
                textpb.Token(text="\n", type=textpb.TokenType.NEWLINE),
                textpb.Token(text="value:", type=textpb.TokenType.FIELD_NAME),
                textpb.Token(text=" ", type=textpb.TokenType.SPACE),
                textpb.Token(text="9827364", type=textpb.TokenType.OTHER_VALUE),
                textpb.Token(text="\n", type=textpb.TokenType.NEWLINE),
                textpb.Token(text="}", type=textpb.TokenType.END_BLOCK),
                textpb.Token(text="\n", type=textpb.TokenType.NEWLINE),
            ]
        )


class AutoDictTests(unittest.TestCase):
    def test_empty(self) -> None:
        self.assertEqual(textpb._auto_dict([]), [])

    def test_not_dict(self) -> None:
        v = ["VAL1", "VAL2"]
        self.assertEqual(textpb._auto_dict(v), v)

    def test_single(self) -> None:
        self.assertEqual(
            textpb._auto_dict(
                [
                    {
                        "key": [
                            textpb.Token(
                                text='"foo"', type=textpb.TokenType.STRING_VALUE
                            )
                        ],
                        "value": ["BAR"],
                    }
                ]
            ),
            {"foo": "BAR"},
        )

    def test_multiple(self) -> None:
        self.assertEqual(
            textpb._auto_dict(
                [
                    {
                        "key": [
                            textpb.Token(
                                text='"foo"', type=textpb.TokenType.STRING_VALUE
                            )
                        ],
                        "value": ["BAR"],
                    },
                    {
                        "key": [
                            textpb.Token(
                                text='"moo"', type=textpb.TokenType.STRING_VALUE
                            )
                        ],
                        "value": ["COW"],
                    },
                ]
            ),
            {
                "foo": "BAR",
                "moo": "COW",
            },
        )


class ParseTests(unittest.TestCase):
    def test_empty(self) -> None:
        self.assertEqual(
            textpb.parse([]),
            dict(),
        )

    def test_one_field(self) -> None:
        self.assertEqual(
            textpb.parse(["cost: 24"]),
            {
                "cost": [
                    textpb.Token(text="24", type=textpb.TokenType.OTHER_VALUE)
                ]
            },
        )

    def test_one_field_empty_struct(self) -> None:
        self.assertEqual(
            textpb.parse(["tools: {}"]),
            {"tools": [dict()]},
        )

    def test_repeated_field(self) -> None:
        self.assertEqual(
            textpb.parse(
                [
                    'alias: "ab"',
                    'alias: "cd"',
                ]
            ),
            {
                "alias": [
                    textpb.Token(
                        text='"ab"', type=textpb.TokenType.STRING_VALUE
                    ),
                    textpb.Token(
                        text='"cd"', type=textpb.TokenType.STRING_VALUE
                    ),
                ]
            },
        )

    def test_deeply_nested(self) -> None:
        self.assertEqual(
            textpb.parse(
                [
                    "outer: {" "  inner: {",
                    "    status: GOOD",
                    "  }",
                    "}",
                ]
            ),
            {
                "outer": [
                    {
                        "inner": [
                            {
                                "status": [
                                    textpb.Token(
                                        text="GOOD",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ]
                            }
                        ]
                    }
                ]
            },
        )

    def test_mixed_levels(self) -> None:
        self.assertEqual(
            textpb.parse(
                [
                    "outer: {" "  inner: {",
                    "    status: GOOD",
                    "  }",
                    "  middle: 77",
                    "  middle: 55",
                    "}",
                    'magic: "wand"',
                ]
            ),
            {
                "outer": [
                    {
                        "inner": [
                            {
                                "status": [
                                    textpb.Token(
                                        text="GOOD",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ]
                            }
                        ],
                        "middle": [
                            textpb.Token(
                                text="77", type=textpb.TokenType.OTHER_VALUE
                            ),
                            textpb.Token(
                                text="55", type=textpb.TokenType.OTHER_VALUE
                            ),
                        ],
                    }
                ],
                "magic": [
                    textpb.Token(
                        text='"wand"', type=textpb.TokenType.STRING_VALUE
                    )
                ],
            },
        )

    def test_nested_dictionaries(self) -> None:
        self.assertEqual(
            textpb.parse(
                [
                    "outer: {" "  inner: {",
                    '    key: "aaa"',
                    "    value: {",
                    "      size: 44",
                    "      status: BAD",
                    "      attributes: {",
                    '        key: "secret"',
                    '        value: "de4db33f"',
                    "      }",
                    "      attributes: {",
                    '        key: "output"',
                    '        value: "ff/gg"',
                    "      }",
                    "    }",
                    "  }",
                    "  inner: {",
                    '    key: "bbb"',
                    "    value: {",
                    "      size: 33",
                    "      status: GOOD",
                    "      attributes: {",
                    '        key: "secret"',
                    '        value: "0000aaaa"',
                    "      }",
                    "      attributes: {",
                    '        key: "output"',
                    '        value: "pp/qq"',
                    "      }",
                    "    }",
                    "  }",
                    "}",
                ]
            ),
            {
                "outer": [
                    {
                        "inner": {
                            "aaa": {
                                "size": [
                                    textpb.Token(
                                        text="44",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ],
                                "status": [
                                    textpb.Token(
                                        text="BAD",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ],
                                "attributes": {
                                    "secret": textpb.Token(
                                        text='"de4db33f"',
                                        type=textpb.TokenType.STRING_VALUE,
                                    ),
                                    "output": textpb.Token(
                                        text='"ff/gg"',
                                        type=textpb.TokenType.STRING_VALUE,
                                    ),
                                },
                            },
                            "bbb": {
                                "size": [
                                    textpb.Token(
                                        text="33",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ],
                                "status": [
                                    textpb.Token(
                                        text="GOOD",
                                        type=textpb.TokenType.OTHER_VALUE,
                                    )
                                ],
                                "attributes": {
                                    "secret": textpb.Token(
                                        text='"0000aaaa"',
                                        type=textpb.TokenType.STRING_VALUE,
                                    ),
                                    "output": textpb.Token(
                                        text='"pp/qq"',
                                        type=textpb.TokenType.STRING_VALUE,
                                    ),
                                },
                            },
                        }
                    }
                ]
            },
        )

    def test_unbalanced_close_block(self) -> None:
        with self.assertRaisesRegex(
            textpb.ParseError, "Unexpected end-of-block"
        ):
            textpb.parse(["}"])

    def test_unbalanced_open_block(self) -> None:
        with self.assertRaisesRegex(textpb.ParseError, "Unexpected EOF"):
            textpb.parse(["foo: {"])

    def test_missing_value(self) -> None:
        with self.assertRaisesRegex(textpb.ParseError, "Unexpected EOF"):
            textpb.parse(["foo: "])

    def test_expected_field(self) -> None:
        with self.assertRaisesRegex(textpb.ParseError, "Expected a field name"):
            textpb.parse(['"value"'])

    def test_expected_value(self) -> None:
        with self.assertRaisesRegex(
            textpb.ParseError, "Unexpected token: .*END_BLOCK"
        ):
            textpb.parse(["foo: }"])

    def test_comma_and_semicolon_separators(self) -> None:
        self.assertEqual(
            textpb.parse(["foo: 1, bar: 2; baz: 3"]),
            {
                "foo": [textpb.Token("1", type=textpb.TokenType.OTHER_VALUE)],
                "bar": [textpb.Token("2", type=textpb.TokenType.OTHER_VALUE)],
                "baz": [textpb.Token("3", type=textpb.TokenType.OTHER_VALUE)],
            },
        )


class ParseLazyTests(unittest.TestCase):
    def test_empty(self) -> None:
        lazy = textpb.parse_lazy("")
        self.assertEqual(len(lazy), 0)
        self.assertEqual(lazy, {})

    def test_scalar_fields(self) -> None:
        text = 'name: "foo"\ncount: 42\nstatus: ACTIVE\n'
        lazy = textpb.parse_lazy(text)
        self.assertEqual(
            lazy["name"],
            [textpb.Token(text='"foo"', type=textpb.TokenType.STRING_VALUE)],
        )
        self.assertEqual(
            lazy["count"],
            [textpb.Token(text="42", type=textpb.TokenType.OTHER_VALUE)],
        )
        self.assertEqual(
            lazy["status"],
            [textpb.Token(text="ACTIVE", type=textpb.TokenType.OTHER_VALUE)],
        )
        self.assertEqual(lazy, textpb.parse(text.splitlines()))

    def test_empty_message(self) -> None:
        text = "empty: {}"
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse([text]))
        self.assertEqual(len(lazy["empty"][0]), 0)

    def test_repeated_fields(self) -> None:
        text = 'arg: "one"\narg: "two"\n'
        lazy = textpb.parse_lazy(text)
        self.assertEqual(
            lazy["arg"],
            [
                textpb.Token(text='"one"', type=textpb.TokenType.STRING_VALUE),
                textpb.Token(text='"two"', type=textpb.TokenType.STRING_VALUE),
            ],
        )
        self.assertEqual(lazy, textpb.parse(text.splitlines()))

    def test_nested_messages(self) -> None:
        text = """
        parent: {
            child: {
                value: 123
            }
        }
        """
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse(text.splitlines()))
        self.assertEqual(
            lazy["parent"][0]["child"][0]["value"],
            [textpb.Token(text="123", type=textpb.TokenType.OTHER_VALUE)],
        )

    def test_angle_brackets(self) -> None:
        text = "sub: < val: 99 >"
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse([text]))

    def test_key_value_auto_dict(self) -> None:
        text = """
        metadata: {
            key: "color"
            value: "blue"
        }
        metadata: {
            key: "size"
            value: "large"
        }
        """
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse(text.splitlines()))
        self.assertIn("color", lazy["metadata"])
        self.assertEqual(lazy["metadata"]["color"].text, '"blue"')
        self.assertEqual(lazy["metadata"]["size"].text, '"large"')

    def test_braces_in_strings_and_comments(self) -> None:
        text = """
        command: {
            args: "awk '{print $1}'"
            args: "echo \\\" } < \\\" "
            # comment with { and }
            trailing: 1
        }
        other: "outside"
        """
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy["other"][0].text, '"outside"')
        self.assertEqual(lazy["command"][0]["trailing"][0].text, "1")
        self.assertEqual(lazy, textpb.parse(text.splitlines()))

    def test_escaped_quotes_and_backslashes(self) -> None:
        text = r"""
        msg: {
            simple: "hello"
            escaped_quote: "foo \" bar"
            escaped_backslash: "foo \\"
            escaped_backslash_and_quote: "foo \\\" bar"
            brace_after_escaped_quote: "brace: \"} inside"
            complex_flag: "-DFOO=\"https://b.f.org/e.cgi\\?prod=gShoe\" "
        }
        """
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse(text.splitlines()))
        sub = lazy["msg"][0]
        self.assertEqual(sub["simple"][0].text, '"hello"')
        self.assertEqual(sub["escaped_quote"][0].text, r'"foo \" bar"')
        self.assertEqual(sub["escaped_backslash"][0].text, r'"foo \\"')
        self.assertEqual(
            sub["escaped_backslash_and_quote"][0].text, r'"foo \\\" bar"'
        )
        self.assertEqual(
            sub["brace_after_escaped_quote"][0].text, r'"brace: \"} inside"'
        )

    def test_unterminated_string_in_block(self) -> None:
        with self.assertRaisesRegex(textpb.ParseError, "Unterminated string"):
            len(textpb.parse_lazy('msg: { a: "unclosed string'))
        with self.assertRaisesRegex(textpb.ParseError, "Unterminated string"):
            len(textpb.parse_lazy('msg: { a: "unclosed string with brace }'))

    def test_lazy_evaluation_unaccessed_block(self) -> None:
        text = """
        good_block: {
            value: "ok"
        }
        bad_block: {
            this is totally invalid syntax inside but delimiters are balanced: { }
        }
        """
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy["good_block"][0]["value"][0].text, '"ok"')
        with self.assertRaises(textpb.ParseError):
            _ = lazy["bad_block"][0]["anything"]

    def test_mapping_methods(self) -> None:
        text = 'a: 1\nb: "two"\n'
        lazy = textpb.parse_lazy(text)
        self.assertEqual(set(lazy.keys()), {"a", "b"})
        self.assertEqual(len(lazy), 2)
        self.assertIn("a", lazy)
        self.assertNotIn("c", lazy)
        self.assertEqual(lazy.get("c", "default"), "default")
        self.assertEqual(dict(lazy.items()), lazy.to_dict())

    def test_unbalanced_delimiters(self) -> None:
        with self.assertRaisesRegex(
            textpb.ParseError, "Unexpected end-of-block"
        ):
            len(textpb.parse_lazy("}"))
        with self.assertRaisesRegex(textpb.ParseError, "Unexpected EOF"):
            len(textpb.parse_lazy("msg: {"))
        with self.assertRaisesRegex(
            textpb.ParseError, "Mismatched end-of-block"
        ):
            len(textpb.parse_lazy("msg: { >"))

    def test_comma_and_semicolon_separators(self) -> None:
        text = "foo: 1, bar: 2; baz: 3"
        lazy = textpb.parse_lazy(text)
        self.assertEqual(lazy, textpb.parse([text]))
        self.assertEqual(lazy["foo"][0].text, "1")
        self.assertEqual(lazy["bar"][0].text, "2")
        self.assertEqual(lazy["baz"][0].text, "3")


if __name__ == "__main__":
    unittest.main()
