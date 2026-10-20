#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import logging
import os
import shutil
import tempfile
import unittest
from unittest import mock

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

        # Test that code blocks are ignored
        text = '```diff\ndeps = [\n-  "//src/lib/ddk"\n]\n```'
        self.assertEqual(skill_linter._pre_process(text, 4), text)

        text = "```markdown\n-   Item 1\n*   Item 2\n+   Item 3\n1.   Numbered\n```"
        self.assertEqual(skill_linter._pre_process(text, 4), text)

        text = "```markdown\n# Header 1\n## Header 2\n```"
        self.assertEqual(skill_linter._pre_process(text, 4), text)

    def test_pre_process_nested_list_under_numbered_list(self) -> None:
        # Test that 3-space nested bullets, 5-space continuations, and 3-space code
        # blocks under numbered lists are normalized to 4, 6, and 4 spaces.
        text = (
            "1. First item\n"
            "   * Child bullet with a longer description that wraps onto the"
            " next line to verify indentation\n"
            "     Continuation line\n"
            "   ```shell\n"
            "   command\n"
            "   ```\n"
            "   Continuation paragraph\n"
            "2. Second item\n"
        )
        expected_4 = (
            "1.  First item\n"
            "    * Child bullet with a longer description that wraps onto the"
            " next line to verify indentation\n"
            "      Continuation line\n"
            "    ```shell\n"
            "    command\n"
            "    ```\n"
            "    Continuation paragraph\n"
            "2.  Second item\n"
        )
        self.assertEqual(skill_linter._pre_process(text, 4), expected_4)

        # Test that already properly indented lists are preserved.
        self.assertEqual(skill_linter._pre_process(expected_4, 4), expected_4)

        # Test formatting end-to-end with format_markdown
        formatted = skill_linter.format_markdown(text, tab_size=4)
        self.assertIn("    * Child bullet", formatted)
        self.assertIn("      verify indentation", formatted)
        self.assertIn("    ```shell", formatted)
        self.assertIn("    Continuation paragraph", formatted)

    def test_pre_process_code_blocks_inside_list(self) -> None:
        # Test that code blocks inside numbered lists are properly indented.
        text = (
            "1. Step One:\n"
            "   ```bash\n"
            "   ffx target list\n"
            "   fx status\n"
            "   ```\n"
            "2. Step Two:\n"
            "   ```bash\n"
            "   fx add-test //src/tests:foo\n"
            "   fx build\n"
            "   ```\n"
        )
        expected = (
            "1.  Step One:\n"
            "    ```bash\n"
            "    ffx target list\n"
            "    fx status\n"
            "    ```\n"
            "2.  Step Two:\n"
            "    ```bash\n"
            "    fx add-test //src/tests:foo\n"
            "    fx build\n"
            "    ```\n"
        )
        self.assertEqual(skill_linter._pre_process(text, 4), expected)
        formatted = skill_linter.format_markdown(text, tab_size=4)
        self.assertIn("    ```bash\n    ffx target list", formatted)
        self.assertIn("    ```bash\n    fx add-test", formatted)

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

        # Test complex markdown structures inside a code block
        text = "```markdown\n-   item 1\n\n| col 1 | col 2 |\n|---|---|\n| a | b |\n\n# Header\n```"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, text + "\n")

        # Test preservation of purely whitespace lines in code blocks
        text = "```rust\nfn main() {\n    let x = 5;\n    \n    println!(x);\n}\n```"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, text + "\n")

    def test_format_markdown_tables(self) -> None:
        # Test that tables are preserved and not wrapped
        text = "| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |"
        formatted = skill_linter.format_markdown(text, width=10)
        # Width 10 should normally wrap lines, but tables should be skipped
        self.assertEqual(formatted, text + "\n")

    def test_format_markdown_multiline_tables(self) -> None:
        # Test that multi-line tables with colon row continuations are preserved
        text = (
            "| Situation              | Action"
            "                                            |\n"
            "| :--------------------- |"
            " :------------------------------------------------ |\n"
            "| **P0 Bug**             | Assign an owner IMMEDIATELY. Ensure"
            " it's Accepted |\n"
            ":                        : ASAP."
            "                                             :\n"
            "| **Semaphore Hang**     | Use"
            " `fuchsia-diagnose-vksemaphore-hangs` to       |\n"
            ":                        : identify client KOID. If display Vsync"
            " stalled,   :\n"
            ":                        : move to `componentid\\:1335242`   "
            "                 :\n"
            ":                        : (`tq-display-bugs@`).            "
            "                 :\n"
            "| **Timekeeping**        | Assign to `fmil@`."
            "                                |\n"
        )
        formatted = skill_linter.format_markdown(text, width=10)
        self.assertEqual(formatted, text)
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertEqual(issues, [])

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

    def test_format_markdown_with_issues_table_boundaries(self) -> None:
        # Test that lines immediately following a table are correctly validated for width
        text = "| Header |\n| --- |\n| Cell |\nThis is a very long text line immediately following the table with no newline."
        _, issues = skill_linter.format_markdown_with_issues(text, width=20)
        self.assertIn("lines exceeding 20 characters", issues)

    def test_format_markdown_with_issues_tables_ignored(self) -> None:
        text = "| Extremely long table header that exceeds eighty characters |\n| --- |\n| Cell without trailing spaces |\n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=20)
        self.assertEqual(issues, [])

    def test_format_markdown_with_issues_code_blocks_ignored(self) -> None:
        text = "```python\ndef foo():\n\n\n    print('Trailing space here:   ')\n```\n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertEqual(issues, [])

    def test_format_markdown_with_issues_normal_text(self) -> None:
        text = "Normal paragraph.\n\n\nAnother normal paragraph with trailing space    \n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertIn("consecutive empty lines", issues)
        self.assertIn("trailing whitespace", issues)

    def test_format_markdown_missing_empty_line_before_list(self) -> None:
        # Paragraph followed immediately by numbered list without empty line
        text = (
            "To configure your build, complete the following steps:\n"
            "1. Select your target device.\n"
            "2. Build the package.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = (
            "To configure your build, complete the following steps:\n\n"
            "1.  Select your target device.\n"
            "2.  Build the package.\n"
        )
        self.assertEqual(formatted, expected)

        # Paragraph followed immediately by bullet list without empty line
        text_bullet = (
            "Available components:\n" "* Component A\n" "* Component B\n"
        )
        (
            formatted_bullet,
            issues_bullet,
        ) = skill_linter.format_markdown_with_issues(text_bullet)
        self.assertIn("missing empty line before list", issues_bullet)
        expected_bullet = (
            "Available components:\n\n" "* Component A\n" "* Component B\n"
        )
        self.assertEqual(formatted_bullet, expected_bullet)

    def test_format_markdown_missing_empty_line_before_list_already_spaced(
        self,
    ) -> None:
        text = (
            "To configure your build, complete the following steps:\n\n"
            "1.  Select your target device.\n"
            "2.  Build the package.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_before_list_after_heading(
        self,
    ) -> None:
        text = "### Steps\n1. First step.\n2. Second step.\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = "### Steps\n\n1.  First step.\n2.  Second step.\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_missing_empty_line_before_list_preserves_tight_lists(
        self,
    ) -> None:
        text = (
            "Introductory text:\n\n"
            "1.  Step 1\n"
            "    * Sub-step a\n"
            "    * Sub-step b\n"
            "2.  Step 2\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_around_code_block(
        self,
    ) -> None:
        text = (
            "Paragraph before code block.\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n"
            "Paragraph after code block.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before code block", issues)
        self.assertIn("missing empty line after code block", issues)
        expected = (
            "Paragraph before code block.\n\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n\n"
            "Paragraph after code block.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_block_already_spaced(self) -> None:
        text = (
            "Paragraph before code block.\n\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n\n"
            "Paragraph after code block.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_after_list(self) -> None:
        # Headings and link reference definitions directly after a list.
        text = (
            "- Item with [a link][ref]\n"
            "[ref]: https://fuchsia.dev\n"
            "\n"
            "- Another item\n"
            "## Next section\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line after list", issues)
        expected = (
            "- Item with [a link][ref]\n"
            "\n"
            "[ref]: https://fuchsia.dev\n"
            "\n"
            "- Another item\n"
            "\n"
            "## Next section\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_lazy_continuation_keeps_list(self) -> None:
        # A non-indented line directly after a list item is a lazy
        # continuation, so no empty line is inserted before the next item.
        text = "Intro:\n\n- First item\nlazy continuation\n- Second item\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertNotIn("missing empty line before list", issues)
        expected = "Intro:\n\n- First item lazy continuation\n- Second item\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_list_after_new_paragraph(self) -> None:
        # A non-indented paragraph after an empty line ends the list, so a
        # following list needs a new empty line.
        text = "- First list\n\nA new paragraph:\n- Second list\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = "- First list\n\nA new paragraph:\n\n- Second list\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_nested_list_after_item_text(self) -> None:
        # A nested list following continuation text inside a list item.
        text = (
            "1.  Parent item:\n"
            "\n"
            "    Continuation paragraph:\n"
            "    - Child A\n"
            "    - Child B\n"
            "2.  Next parent\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = (
            "1.  Parent item:\n"
            "\n"
            "    Continuation paragraph:\n"
            "\n"
            "    - Child A\n"
            "    - Child B\n"
            "2.  Next parent\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_block_inside_list_item(self) -> None:
        text = (
            "1.  Run the command:\n"
            "    ```bash\n"
            "    fx build\n"
            "    ```\n"
            "    Then continue with the next step.\n"
            "2.  Next step\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before code block", issues)
        self.assertIn("missing empty line after code block", issues)
        # The next item follows a continuation paragraph rather than a list
        # item paragraph, so it is also separated by an empty line.
        self.assertIn("missing empty line before list", issues)
        expected = (
            "1.  Run the command:\n"
            "\n"
            "    ```bash\n"
            "    fx build\n"
            "    ```\n"
            "\n"
            "    Then continue with the next step.\n"
            "\n"
            "2.  Next step\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_whitespace_only_line_counts_as_empty(
        self,
    ) -> None:
        text = "Intro paragraph.\n  \n- Item\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertNotIn("missing empty line before list", issues)
        self.assertNotIn("\n\n\n", formatted)

    def test_format_markdown_spacing_rules_are_idempotent(self) -> None:
        text = (
            "Intro:\n"
            "- Item A\n"
            "    ```bash\n"
            "    echo a\n"
            "    ```\n"
            "    More text.\n"
            "- Item B\n"
            "### Heading\n"
            "Text before code:\n"
            "```\n"
            "code\n"
            "```\n"
            "Text after code.\n"
        )
        once, _ = skill_linter.format_markdown_with_issues(text)
        twice, issues = skill_linter.format_markdown_with_issues(once)
        self.assertEqual(once, twice)
        self.assertEqual(issues, [])

    def test_format_markdown_link_reference_definitions_not_merged(
        self,
    ) -> None:
        text = (
            "Paragraph before links.\n\n"
            "[ref-a]: https://example.com/a\n"
            "[ref-b]: https://example.com/b\n"
            "[ref-c]: https://example.com/very/long/url/that/exceeds/the/specified/maximum/width/and/should/not/be/flagged/as/long/line\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(
            text, width=40
        )
        self.assertEqual(issues, [])
        self.assertIn("[ref-a]: https://example.com/a\n", formatted)
        self.assertIn("[ref-b]: https://example.com/b\n", formatted)
        self.assertIn(
            "[ref-c]: https://example.com/very/long/url/that/exceeds/the/specified/maximum/width/and/should/not/be/flagged/as/long/line\n",
            formatted,
        )
        # Ensure definitions are not concatenated on the same line
        self.assertNotIn("[ref-a]: https://example.com/a [ref-b]:", formatted)

    def test_cli_modes_and_directory_traversal(self) -> None:
        temp_dir = tempfile.mkdtemp()
        try:
            # Create two skill subdirectories: clean-skill and dirty-skill
            clean_dir = os.path.join(temp_dir, "clean-skill")
            dirty_dir = os.path.join(temp_dir, "dirty-skill")
            os.makedirs(clean_dir)
            os.makedirs(dirty_dir)

            clean_skill = os.path.join(clean_dir, "SKILL.md")
            with open(clean_skill, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: clean-skill\n"
                    "description: A completely valid skill.\n"
                    "---\n\n"
                    "# Overview\n\n"
                    "Valid skill content.\n"
                )

            # dirty-skill has BOTH a formatting issue (single trailing space + consecutive empty lines)
            # AND a description error that cannot be auto-fixed (lone "<")
            dirty_skill = os.path.join(dirty_dir, "SKILL.md")
            with open(dirty_skill, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: dirty-skill\n"
                    "description: Compare a < b values.\n"
                    "---\n\n"
                    "# Overview \n\n\n"
                    "Run on the target.\n"
                )

            # 1. Directory traversal in check-only mode
            code, findings = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code, 1)

            # 2. JSON mode (--suggest-fix-in-json) on parent directory
            code_json, findings_json = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(code_json, 0)
            self.assertEqual(len(findings_json), 1)
            finding = findings_json[0]
            self.assertEqual(finding["level"], "error")
            self.assertIn("angle brackets", finding["message"])
            self.assertIn("replacements", finding)
            # Replacement should have fixed the triple newline & trailing space
            self.assertNotIn("# Overview \n\n\n", finding["replacements"][0])

            # 3. --suggest-fix (stdout mode)
            captured = io.StringIO()
            with mock.patch("sys.stdout", captured):
                code_stdout, _ = skill_linter.do_lint(
                    root_path=dirty_skill,
                    fixit=True,
                    stdout_mode=True,
                    suggest_fix_in_json_mode=False,
                )
            self.assertEqual(code_stdout, 1)
            self.assertIn(
                "# Overview\n\nRun on the target.\n",
                captured.getvalue(),
            )

            # 4. --fixit mode writes formatting fixes to disk while still returning exit code 1
            code_fixit, _ = skill_linter.do_lint(
                root_path=dirty_skill,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code_fixit, 1)
            with open(dirty_skill, "r", encoding="utf-8") as f:
                updated_disk = f.read()
            self.assertNotIn("# Overview \n\n\n", updated_disk)
        finally:
            shutil.rmtree(temp_dir)

    def test_generate_frontmatter_lines_preserves_nested_dicts_and_lists(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: test-skill\n"
                    "description: A valid skill.\n"
                    "metadata:\n"
                    "  hardware:\n"
                    "    board: x64\n"
                    "  targets:\n"
                    "    - name: emu\n"
                    "      arch: arm64\n"
                    "---\n\n"
                    "# Overview \n\n\n"
                    "Body.\n"
                )
            code, _ = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code, 0)
            with open(skill_path, "r", encoding="utf-8") as f:
                updated = f.read()
            self.assertNotIn("{'hardware':", updated)
            parsed = skill_linter._get_skill_metadata(updated)
            self.assertIsNotNone(parsed)
            assert parsed is not None
            meta, _ = parsed
            self.assertEqual(
                meta["metadata"],
                {
                    "hardware": {"board": "x64"},
                    "targets": [{"name": "emu", "arch": "arm64"}],
                },
            )

    def test_format_code_hint_gated_on_autofixes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            content = (
                "---\n"
                "name: perfectly-formatted\n"
                "description: Compare a < b values.\n"
                "---\n\n"
                "# Title\n\n"
                "Well formatted body.\n"
            )
            with open(skill_path, "w") as f:
                f.write(content)

            # The lone angle bracket in the description is an error that has
            # no autofix, so the format-code hint must not be shown.

            # CLI mode
            logger = logging.getLogger()
            with self.assertLogs(logger, level="INFO") as log_context:
                exit_code, findings = skill_linter.lint_single_skill(
                    skill_file=skill_path,
                    fixit=False,
                    stdout_mode=False,
                    suggest_fix_in_json_mode=False,
                )
            self.assertEqual(exit_code, 1)
            log_output = "\n".join(log_context.output)
            self.assertNotIn(
                "You can run fx format-code to fix these issues.", log_output
            )

            # JSON mode
            exit_code, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(len(findings), 1)
            self.assertIn("angle brackets", findings[0]["message"])
            self.assertNotIn(
                "You can run fx format-code to fix these issues.",
                findings[0]["message"],
            )


if __name__ == "__main__":
    unittest.main()
