#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

import skill_linter


class TestSkillLinter(unittest.TestCase):
    def test_suggest_valid_name(self) -> None:
        self.assertEqual(
            skill_linter._suggest_valid_name("Valid-Name"), "valid-name"
        )
        self.assertEqual(
            skill_linter._suggest_valid_name("Name_With_Underscores"),
            "name-with-underscores",
        )
        self.assertEqual(
            skill_linter._suggest_valid_name("Invalid@Chars!"), "invalidchars"
        )
        self.assertEqual(skill_linter._suggest_valid_name("a" * 100), "a" * 64)

    def test_pre_process(self) -> None:
        # Test list item indentation standardization
        text = "-   item"
        self.assertEqual(skill_linter._pre_process(text, 4), "- item")

        text = "1.   item"
        self.assertEqual(skill_linter._pre_process(text, 2), "1. item")
        self.assertEqual(skill_linter._pre_process(text, 4), "1.  item")

    def test_post_process(self) -> None:
        # Test trailing whitespace removal and newlines
        text = "line with spaces    \nline 2"
        self.assertEqual(
            skill_linter._post_process(text), "line with spaces  \nline 2\n"
        )

        text = "line\n\n\nline2"
        self.assertEqual(skill_linter._post_process(text), "line\n\nline2\n")

    def test_validate_name(self) -> None:
        meta = {"name": "valid-name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=False)
        self.assertEqual(errors, [])
        self.assertEqual(fixes, [])

        meta = {"name": "Invalid Name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=False)
        self.assertTrue(len(errors) > 0)

        meta = {"name": "Invalid Name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=True)
        self.assertEqual(meta["name"], "invalidname")
        self.assertTrue(len(fixes) > 0)

    def test_validate_description(self) -> None:
        meta = {"description": "Valid description"}
        errors, fixes = skill_linter._validate_description(meta, fixit=False)
        self.assertEqual(errors, [])
        self.assertEqual(fixes, [])

        meta = {"description": "Invalid description with <tags>"}
        errors, fixes = skill_linter._validate_description(meta, fixit=False)
        self.assertTrue(len(errors) > 0)

        meta = {"description": "Invalid description with <tags>"}
        errors, fixes = skill_linter._validate_description(meta, fixit=True)
        self.assertEqual(meta["description"], "Invalid description with")
        self.assertTrue(len(fixes) > 0)

    def test_format_markdown_paragraphs(self) -> None:
        # Test paragraph wrapping
        text = "This is a long paragraph that should be wrapped to fit within the eighty character limit that is imposed by the linter."
        formatted = skill_linter.format_markdown(text, width=40)
        expected = (
            "This is a long paragraph that should be\n"
            "wrapped to fit within the eighty\n"
            "character limit that is imposed by the\n"
            "linter.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_lists(self) -> None:
        # Test list formatting and indentation
        text = "- Item 1\n- Item 2 with a very long description that should wrap correctly under the marker."
        formatted = skill_linter.format_markdown(text, width=20)
        # Note: textwrap behavior might differ slightly from hand-calculated expected
        expected = (
            "- Item 1\n"
            "- Item 2 with a very\n"
            "  long description\n"
            "  that should wrap\n"
            "  correctly under\n"
            "  the marker.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_blocks(self) -> None:
        # Test that code blocks are preserved
        text = "Para before.\n\n```python\ndef foo():\n    pass\n```\n\nPara after."
        formatted = skill_linter.format_markdown(text)
        self.assertIn("```python\ndef foo():\n    pass\n```", formatted)

    def test_format_markdown_tables(self) -> None:
        # Test that tables are preserved and not wrapped
        text = "| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |"
        formatted = skill_linter.format_markdown(text, width=10)
        # Width 10 should normally wrap lines, but tables should be skipped
        self.assertEqual(formatted, text + "\n")

    def test_format_markdown_headers(self) -> None:
        # Test that headers are preserved
        text = "# Header 1\n\n## Header 2\n\nContent."
        formatted = skill_linter.format_markdown(text)
        self.assertIn("# Header 1\n", formatted)
        self.assertIn("## Header 2\n", formatted)

    def test_format_markdown_hard_breaks(self) -> None:
        # Test that Markdown hard line breaks (two spaces) are preserved
        text = "Line 1  \nLine 2"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, "Line 1  \nLine 2\n")


if __name__ == "__main__":
    unittest.main()
