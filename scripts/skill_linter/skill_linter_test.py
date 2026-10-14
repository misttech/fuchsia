#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import json
import logging
import os
import re
import shutil
import subprocess
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

    def test_load_blocked_words_policy(self) -> None:
        # 1. Test JSON structured format
        json_content = """{
            "version": 1,
            "default_scope": "public_only",
            "blocked_words": [
                {
                    "term": "secret-board",
                    "reason": "Unannounced board codename.",
                    "suggestion": "Use generic board x64.",
                    "scope": "public_only",
                    "severity": "error"
                },
                "simple-term"
            ]
        }"""
        with tempfile.NamedTemporaryFile(
            "w", suffix=".json", delete=False
        ) as f:
            f.write(json_content)
            temp_json = f.name
        try:
            rules = skill_linter.load_blocked_words_policy(temp_json)
            self.assertEqual(len(rules), 2)
            self.assertEqual(rules[0].term, "secret-board")
            self.assertEqual(rules[0].reason, "Unannounced board codename.")
            self.assertEqual(rules[0].suggestion, "Use generic board x64.")
            self.assertEqual(rules[0].scope, "public_only")
            self.assertEqual(rules[0].severity, "error")
            self.assertEqual(rules[1].term, "simple-term")
            self.assertEqual(rules[1].scope, "public_only")
        finally:
            os.remove(temp_json)

        # 2. Test YAML simple list
        yaml_content = "- forbidden-one\n- forbidden-two\n"
        with tempfile.NamedTemporaryFile(
            "w", suffix=".yaml", delete=False
        ) as f:
            f.write(yaml_content)
            temp_yaml = f.name
        try:
            rules = skill_linter.load_blocked_words_policy(temp_yaml)
            self.assertEqual(len(rules), 2)
            self.assertEqual(rules[0].term, "forbidden-one")
            self.assertEqual(rules[1].term, "forbidden-two")
        finally:
            os.remove(temp_yaml)

        # 3. Test nonexistent file
        self.assertEqual(
            skill_linter.load_blocked_words_policy("/no/such/file.json"), []
        )

    def _compile_rule(
        self,
        term: str,
        pattern: str | None = None,
        reason: str = "Prohibited internal or unannounced term.",
        suggestion: str | None = None,
        scope: str = "public_only",
        severity: str = "error",
        case_sensitive: bool = False,
        check_code_blocks: bool = True,
        allowed_paths: list[str] | None = None,
        exemptions: list[str] | None = None,
    ) -> skill_linter.BlockedWordRule:
        rule = skill_linter._compile_blocked_word_rule(
            term=term,
            pattern=pattern,
            reason=reason,
            suggestion=suggestion,
            scope=scope,
            severity=severity,
            case_sensitive=case_sensitive,
            check_code_blocks=check_code_blocks,
            allowed_paths=allowed_paths,
            exemptions=exemptions,
        )
        self.assertIsNotNone(rule)
        assert rule is not None
        return rule

    def test_blocked_words_word_boundaries(self) -> None:
        rule = self._compile_rule(term="badterm")
        # Substrings inside words should NOT match
        self.assertFalse(bool(rule.pattern.search("mybadterm")))
        self.assertFalse(bool(rule.pattern.search("badterminal")))
        self.assertFalse(bool(rule.pattern.search("mybadtermcorp")))

        # Delimiters should match
        self.assertTrue(bool(rule.pattern.search("badterm")))
        self.assertTrue(bool(rule.pattern.search("flash badterm image")))
        self.assertTrue(bool(rule.pattern.search("badterm-device")))
        self.assertTrue(bool(rule.pattern.search("flash-badterm")))
        self.assertTrue(bool(rule.pattern.search("badterm_board")))
        self.assertTrue(bool(rule.pattern.search("core.badterm")))
        self.assertTrue(bool(rule.pattern.search("badterm.")))
        self.assertTrue(bool(rule.pattern.search("BADTERM")))

    def test_blocked_words_in_frontmatter(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            reason="Unannounced board",
            suggestion="Use x64",
        )
        # Test violation in name
        meta = {
            "name": "flash-secret-board",
            "description": "A valid description",
        }
        content = (
            "---\n"
            "name: flash-secret-board\n"
            "description: A valid description\n"
            "---\n"
            "\n"
            "Body."
        )
        errors, warnings = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/my-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn(
            'Prohibited term "secret-board" found in frontmatter field "name"',
            errors[0],
        )
        self.assertIn("Reason: Unannounced board", errors[0])
        self.assertIn("Suggestion: Use x64", errors[0])

        # Test violation in description
        meta = {
            "name": "flash-device",
            "description": "Flashes a secret-board device.",
        }
        content = (
            "---\n"
            "name: flash-device\n"
            "description: Flashes a secret-board device.\n"
            "---\n"
            "\n"
            "Body."
        )
        errors, warnings = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/my-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn(
            'Prohibited term "secret-board" found in frontmatter field'
            ' "description"',
            errors[0],
        )

    def test_blocked_words_in_markdown_body_and_lines(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            reason="Unannounced board",
        )
        meta = {"name": "flash-device", "description": "Flashes device."}
        content = (
            "---\n"
            "name: flash-device\n"
            "description: Flashes device.\n"
            "---\n"
            "\n"
            "# Flashing guide\n"
            "\n"
            "Run fastboot on secret-board to flash.\n"
        )
        errors, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/my-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn(
            'Prohibited term "secret-board" found in markdown body (line 8)',
            errors[0],
        )
        self.assertIn("Run fastboot on secret-board to flash.", errors[0])

    def test_blocked_words_in_code_blocks(self) -> None:
        rule_check_code = self._compile_rule(
            term="secret-board", check_code_blocks=True
        )
        rule_ignore_code = self._compile_rule(
            term="secret-board", check_code_blocks=False
        )
        meta = {"name": "flash-device", "description": "Flashes device."}
        content = (
            "---\n"
            "name: flash-device\n"
            "description: Flashes device.\n"
            "---\n"
            "\n"
            "```bash\n"
            "fx set core.secret-board\n"
            "```\n"
        )
        # Should flag when check_code_blocks is True
        errors, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/my-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule_check_code],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("markdown body (line 7)", errors[0])

        # Should NOT flag when check_code_blocks is False
        errors_ignored, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/my-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule_ignore_code],
        )
        self.assertEqual(len(errors_ignored), 0)

    def test_blocked_words_scoping_public_vs_vendor(self) -> None:
        rule = self._compile_rule(term="secret-board", scope="public_only")
        meta = {"name": "flash-device", "description": "Flashes secret-board."}
        content = (
            "---\n"
            "name: flash-device\n"
            "description: Flashes secret-board.\n"
            "---\n"
            "\n"
            "Body."
        )

        # Public path: triggers error
        errors, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/public-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors), 1)

        # Vendor path: exempt from public_only rule
        errors_vendor, _ = skill_linter._validate_blocked_words(
            skill_file="vendor/google/docs/skills/internal-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors_vendor), 0)

        # Rule with scope: "all": triggers in vendor as well
        rule_all = self._compile_rule(term="secret-board", scope="all")
        errors_all, _ = skill_linter._validate_blocked_words(
            skill_file="vendor/google/docs/skills/internal-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule_all],
        )
        self.assertEqual(len(errors_all), 1)

    def test_blocked_words_allowed_paths(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            scope="all",
            allowed_paths=["docs/skills/exempt-skill/**", "special-skill/**"],
        )
        meta = {"name": "flash-device", "description": "Flashes secret-board."}
        content = (
            "---\n"
            "name: flash-device\n"
            "description: Flashes secret-board.\n"
            "---\n"
            "\n"
            "Body."
        )

        # Allowed path: skipped
        errors_allowed, _ = skill_linter._validate_blocked_words(
            skill_file="docs/skills/exempt-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors_allowed), 0)

        # Non-allowed path: flagged
        errors_flagged, _ = skill_linter._validate_blocked_words(
            skill_file="docs/skills/other-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        self.assertEqual(len(errors_flagged), 1)

    def test_lint_single_skill_with_blocked_words(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            reason="Unannounced hardware",
            suggestion="Use emulator",
        )
        skill_content = (
            "---\n"
            "name: test-skill\n"
            "description: A test skill.\n"
            "---\n"
            "\n"
            "# Instructions\n"
            "\n"
            "Do not use secret-board here.\n"
        )
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write(skill_content)
            temp_path = f.name
        try:
            # Check-only mode returns 1 when blocked word is found
            code, _ = skill_linter.lint_single_skill(
                skill_file=temp_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code, 1)

            # JSON mode returns 0 with an error level finding
            code_json, json_findings = skill_linter.lint_single_skill(
                skill_file=temp_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code_json, 0)
            self.assertEqual(len(json_findings), 1)
            self.assertEqual(json_findings[0]["level"], "error")
            self.assertIn("secret-board", json_findings[0]["message"])
            self.assertIn("Unannounced hardware", json_findings[0]["message"])
            self.assertIn("Use emulator", json_findings[0]["message"])
        finally:
            os.remove(temp_path)

    def test_blocked_words_regex_patterns(self) -> None:
        rule_lookbehind = self._compile_rule(
            term="device",
            pattern=r"(_|\b)(?<!mock )devices?(_|\b)",
        )
        meta = {"name": "test-skill", "description": "A test skill."}

        # "mock device" should be permitted
        errors_mock, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/test/SKILL.md",
            meta=meta,
            raw_content=(
                "---\n"
                "name: test-skill\n"
                "description: A test skill.\n"
                "---\n"
                "\n"
                "Connect to a mock device for testing.\n"
            ),
            rules=[rule_lookbehind],
        )
        self.assertEqual(len(errors_mock), 0)

        # "device" standalone should be flagged
        errors_device, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/test/SKILL.md",
            meta=meta,
            raw_content=(
                "---\n"
                "name: test-skill\n"
                "description: A test skill.\n"
                "---\n"
                "\n"
                "Targeting device board.\n"
            ),
            rules=[rule_lookbehind],
        )
        self.assertEqual(len(errors_device), 1)

    def test_blocked_words_exemptions(self) -> None:
        rule_mock = self._compile_rule(
            term="mockkeyword",
            pattern=r"(_|\b)mockkeywords?(_|\b)[A-Za-z_]*",
            exemptions=[
                "MOCKKEYWORD_ALLOWED",
                "MOCKKEYWORD_TCP",
                "MOCKKEYWORD_IP",
            ],
        )
        meta = {"name": "test-skill", "description": "A test skill."}

        # Code using MOCKKEYWORD_ALLOWED should be permitted
        errors_exempt, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/test/SKILL.md",
            meta=meta,
            raw_content=(
                "---\n"
                "name: test-skill\n"
                "description: A test skill.\n"
                "---\n"
                "\n"
                "setsockopt(fd, MOCKKEYWORD_ALLOWED, 1);\n"
            ),
            rules=[rule_mock],
        )
        self.assertEqual(len(errors_exempt), 0)

        # Code using mockkeyword_unallowed should be flagged
        errors_flagged, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/test/SKILL.md",
            meta=meta,
            raw_content=(
                "---\n"
                "name: test-skill\n"
                "description: A test skill.\n"
                "---\n"
                "\n"
                "Using mockkeyword_unallowed driver.\n"
            ),
            rules=[rule_mock],
        )
        self.assertEqual(len(errors_flagged), 1)
        self.assertIn("markdown body (line 6)", errors_flagged[0])

    def test_parse_gcl_policy(self) -> None:
        gcl_content = """
        final fuchsia_banned_words_with_exemptions =
            [{
               blocked_keyword_pattern = '(_|\\b)mockkeywords?(_|\\b)[A-Za-z_]*'
               allowed_keyword_pattern = [
                 'MOCKKEYWORD_ALLOWED',
                 'MOCKKEYWORD_TCP',
               ]
             }]

        final fuchsia_only_common_banned_words =
            fuchsia_banned_words_with_exemptions +
            map(
                lambda w: {
                  blocked_keyword_pattern = '(_|\\b)' + w + '(_|\\b)'
                },
                [
                  '(?<!mock )devices?',
                  'sampleboards?',
                  'plainword',
                ])
        """
        with tempfile.NamedTemporaryFile("w", suffix=".gcl", delete=False) as f:
            f.write(gcl_content)
            temp_gcl = f.name
        try:
            rules = skill_linter._parse_gcl_policy(temp_gcl)
            self.assertEqual(len(rules), 4)
            terms = {r.term for r in rules}
            self.assertIn("mockkeywords", terms)
            self.assertIn("(?<!mock )devices?", terms)
            self.assertIn("sampleboards?", terms)
            self.assertIn("plainword", terms)

            mock_rule = next(r for r in rules if r.term == "mockkeywords")
            self.assertEqual(
                mock_rule.exemptions, ["MOCKKEYWORD_ALLOWED", "MOCKKEYWORD_TCP"]
            )
        finally:
            os.remove(temp_gcl)

    def _discover_repo_skill_files(self) -> list[str]:
        """Discovers real SKILL.md files tracked in the Fuchsia repository."""
        root = skill_linter.find_fuchsia_root()
        if not root or not os.path.isdir(root):
            return []
        try:
            out = subprocess.check_output(
                ["git", "-C", root, "ls-files", "*SKILL.md"],
                text=True,
                stderr=subprocess.DEVNULL,
            )
            paths = [
                os.path.join(root, rel.strip())
                for rel in out.splitlines()
                if rel.strip()
            ]
            return [p for p in paths if os.path.isfile(p)]
        except (subprocess.CalledProcessError, FileNotFoundError):
            found = []
            for sub in (".agents/skills", "docs/skills", "zircon/skills"):
                base = os.path.join(root, sub)
                if not os.path.isdir(base):
                    continue
                for dirpath, _, filenames in os.walk(base):
                    if "SKILL.md" in filenames:
                        found.append(os.path.join(dirpath, "SKILL.md"))
            return found

    def _synthesize_sample_from_rule(
        self, rule: skill_linter.BlockedWordRule
    ) -> str | None:
        """Synthesizes a matching test token dynamically from a compiled rule."""
        term = rule.term
        candidate = term
        candidate = re.sub(r"\(\?<!.*?\)", "", candidate)
        candidate = candidate.replace("(-|_|\\s)", "-")
        candidate = candidate.replace("(es)?", "es")
        candidate = candidate.replace("?", "")
        candidate = candidate.strip()
        if not candidate:
            return None
        if rule.exemptions:
            candidate = f"{candidate}_unallowed_test_token"
        if rule.pattern.search(candidate):
            if not (
                rule.exemptions
                and skill_linter.is_match_exempt(candidate, rule.exemptions)
            ):
                return candidate
        return None

    def test_all_real_fuchsia_skills_have_no_blocked_words(self) -> None:
        skill_files = self._discover_repo_skill_files()
        if not skill_files:
            self.skipTest("Fuchsia repository SKILL.md files not accessible.")

        self.assertGreaterEqual(
            len(skill_files),
            10,
            "Expected to discover real SKILL.md files in fuchsia.git",
        )

        policy_path = skill_linter.find_blocked_words_policy_path()
        live_rules = (
            skill_linter.load_blocked_words_policy(policy_path)
            if policy_path
            else []
        )
        synthetic_rules = [
            self._compile_rule(
                term="unannounced-synthetic-board",
                reason="Synthetic test rule",
            )
        ]
        combined_rules = live_rules + synthetic_rules

        root = skill_linter.find_fuchsia_root() or ""
        for full_path in skill_files:
            with open(full_path, "r", encoding="utf-8") as f:
                raw_content = f.read()
            parsed = skill_linter._get_skill_metadata(raw_content)
            self.assertIsNotNone(
                parsed, f"Failed to parse frontmatter in {full_path}"
            )
            assert parsed is not None
            meta, _ = parsed
            rel_path = os.path.relpath(full_path, root) if root else full_path
            errors, warnings = skill_linter._validate_blocked_words(
                skill_file=rel_path,
                meta=meta,
                raw_content=raw_content,
                rules=combined_rules,
            )
            self.assertEqual(
                errors,
                [],
                f"Unexpected blocked word errors in {rel_path}: {errors}",
            )
            self.assertEqual(
                warnings,
                [],
                f"Unexpected blocked word warnings in {rel_path}: {warnings}",
            )

    def test_real_skills_mutated_with_blocked_words(self) -> None:
        skill_files = self._discover_repo_skill_files()
        if not skill_files:
            self.skipTest("Fuchsia repository SKILL.md files not accessible.")

        # Pick up to 5 representative skills across different directories
        selected_skills = skill_files[:5]
        synthetic_rule = self._compile_rule(
            term="secret-board-codename",
            reason="Internal board codename.",
            suggestion="Use generic x64 board.",
        )
        active_rules = [synthetic_rule]
        test_tokens = [("secret-board-codename", "secret-board-codename")]

        # If the live GCL policy is mounted, also dynamically test rules from it
        if os.path.isfile(skill_linter.DEFAULT_GOOGLE3_HEAD_GCL_PATH):
            live_rules = skill_linter.load_blocked_words_policy(
                skill_linter.DEFAULT_GOOGLE3_HEAD_GCL_PATH
            )
            active_rules.extend(live_rules)
            # Pick a few rules (including exemptions rule, regex rule, and plain rule)
            for r in live_rules[:4]:
                sample = self._synthesize_sample_from_rule(r)
                if sample:
                    test_tokens.append((r.term, sample))

        for full_path in selected_skills:
            with open(full_path, "r", encoding="utf-8") as f:
                original_content = f.read()
            parsed = skill_linter._get_skill_metadata(original_content)
            self.assertIsNotNone(parsed)
            assert parsed is not None
            orig_meta, orig_body = parsed

            for expected_term, injected_token in test_tokens:
                # 1. Inject into frontmatter name
                slug_token = re.sub(
                    r"[^a-z0-9-]", "-", injected_token.lower()
                ).strip("-")
                if expected_term == "secret-board-codename":
                    meta_name = dict(orig_meta)
                    meta_name["name"] = f"skill-{slug_token}"[:64]
                    errs, _ = skill_linter._validate_blocked_words(
                        skill_file=".agents/skills/test/SKILL.md",
                        meta=meta_name,
                        raw_content=original_content,
                        rules=active_rules,
                    )
                    self.assertTrue(
                        any(
                            'frontmatter field "name"' in e
                            and (expected_term in e or injected_token in e)
                            for e in errs
                        ),
                        f"Expected name violation for {expected_term}, got {errs}",
                    )

                # 2. Inject into frontmatter description
                meta_desc = dict(orig_meta)
                meta_desc[
                    "description"
                ] = f"{orig_meta.get('description', '')} Targets {injected_token} hardware."
                errs_desc, _ = skill_linter._validate_blocked_words(
                    skill_file=".agents/skills/test/SKILL.md",
                    meta=meta_desc,
                    raw_content=original_content,
                    rules=active_rules,
                )
                self.assertTrue(
                    any(
                        'frontmatter field "description"' in e
                        and (expected_term in e or injected_token in e)
                        for e in errs_desc
                    ),
                    f"Expected description violation for {expected_term}, got {errs_desc}",
                )

                # 3. Inject into markdown body paragraph and code block
                mutated_content = (
                    original_content.rstrip("\n")
                    + f"\n\nRun tests on {injected_token} target:\n\n"
                    + f"```bash\nfx set core.{injected_token}\n```\n"
                )
                errs_body, _ = skill_linter._validate_blocked_words(
                    skill_file=".agents/skills/test/SKILL.md",
                    meta=orig_meta,
                    raw_content=mutated_content,
                    rules=active_rules,
                )
                body_matches = [
                    e
                    for e in errs_body
                    if "markdown body" in e
                    and (expected_term in e or injected_token in e)
                ]
                self.assertGreaterEqual(
                    len(body_matches),
                    2,
                    f"Expected at least 2 body violations for {expected_term}, got {errs_body}",
                )

    def test_blocked_words_in_all_frontmatter_structures(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            reason="Unannounced board",
        )

        # 1. Multiline folded scalar description (description: >)
        content_multiline = (
            "---\n"
            "name: valid-skill\n"
            "description: >\n"
            "  This is a multiline folded scalar description that spans\n"
            "  multiple lines and mentions secret-board on the second line.\n"
            "---\n\n"
            "Body text.\n"
        )
        parsed = skill_linter._get_skill_metadata(content_multiline)
        self.assertIsNotNone(parsed)
        assert parsed is not None
        meta, _ = parsed
        errs, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/valid-skill/SKILL.md",
            meta=meta,
            raw_content=content_multiline,
            rules=[rule],
        )
        self.assertEqual(len(errs), 1)
        self.assertIn('frontmatter field "description"', errs[0])

        # 2. YAML list field (e.g., tags or allowed-tools)
        content_list = (
            "---\n"
            "name: valid-skill\n"
            "description: A valid description.\n"
            "tags:\n"
            "  - public-tag\n"
            "  - secret-board-tag\n"
            "---\n\n"
            "Body text.\n"
        )
        parsed_list = skill_linter._get_skill_metadata(content_list)
        self.assertIsNotNone(parsed_list)
        assert parsed_list is not None
        meta_list, _ = parsed_list
        errs_list, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/valid-skill/SKILL.md",
            meta=meta_list,
            raw_content=content_list,
            rules=[rule],
        )
        self.assertEqual(len(errs_list), 1)
        self.assertIn('frontmatter field "tags"', errs_list[0])

        # 3. Nested YAML dict field
        content_dict = (
            "---\n"
            "name: valid-skill\n"
            "description: A valid description.\n"
            "metadata:\n"
            "  hardware:\n"
            "    board: secret-board\n"
            "---\n\n"
            "Body text.\n"
        )
        parsed_dict = skill_linter._get_skill_metadata(content_dict)
        self.assertIsNotNone(parsed_dict)
        assert parsed_dict is not None
        meta_dict, _ = parsed_dict
        errs_dict, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/valid-skill/SKILL.md",
            meta=meta_dict,
            raw_content=content_dict,
            rules=[rule],
        )
        self.assertEqual(len(errs_dict), 1)
        self.assertIn(
            'frontmatter field "metadata.hardware.board"', errs_dict[0]
        )

        # 4. Custom frontmatter key name containing blocked word
        content_key = (
            "---\n"
            "name: valid-skill\n"
            "description: A valid description.\n"
            "secret-board-config: enabled\n"
            "---\n\n"
            "Body text.\n"
        )
        parsed_key = skill_linter._get_skill_metadata(content_key)
        self.assertIsNotNone(parsed_key)
        assert parsed_key is not None
        meta_key, _ = parsed_key
        errs_key, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/valid-skill/SKILL.md",
            meta=meta_key,
            raw_content=content_key,
            rules=[rule],
        )
        self.assertEqual(len(errs_key), 1)
        self.assertIn('frontmatter field "secret-board-config"', errs_key[0])

    def test_blocked_words_in_all_markdown_body_structures(self) -> None:
        rule = self._compile_rule(
            term="secret-board",
            reason="Unannounced board",
        )
        content = (
            "---\n"
            "name: comprehensive-skill\n"
            "description: Tests every markdown structure.\n"
            "---\n"
            "\n"
            "# Header 1 with secret-board\n"  # line 6
            "\n"
            "## Subheader with secret-board\n"  # line 8
            "\n"
            "- Bullet item mentioning secret-board in a list\n"  # line 10
            "1. Numbered item mentioning secret-board in a list\n"  # line 11
            "\n"
            "> [!NOTE]\n"
            "> Blockquote callout referencing secret-board target.\n"  # line 14
            "\n"
            "| Target | Board Name |\n"
            "| --- | --- |\n"
            "| Primary | secret-board |\n"  # line 18
            "\n"
            "Inline code span `fx set core.secret-board` in prose.\n"  # line 20
            "\n"
            "---\n"  # horizontal rule on line 22
            "\n"
            "See [Guide](https://example.com/secret-board/docs) after HR.\n"  # line 24
            "\n"
            "[ref-link]: https://internal.example.com/secret-board\n"  # line 26
        )
        parsed = skill_linter._get_skill_metadata(content)
        self.assertIsNotNone(parsed)
        assert parsed is not None
        meta, _ = parsed

        # With max_occurrences_to_report = 5, test subsets and full scan
        errs, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/comprehensive-skill/SKILL.md",
            meta=meta,
            raw_content=content,
            rules=[rule],
        )
        # First 5 occurrences (lines 6, 8, 10, 11, 14) + summary of 4 more (lines 18, 20, 24, 26)
        self.assertEqual(len(errs), 6)
        self.assertIn("markdown body (line 6)", errs[0])
        self.assertIn("markdown body (line 8)", errs[1])
        self.assertIn("markdown body (line 10)", errs[2])
        self.assertIn("markdown body (line 11)", errs[3])
        self.assertIn("markdown body (line 14)", errs[4])
        self.assertIn("also appeared 4 more times in the file", errs[5])

        # Verify lines after horizontal rule (---) and inside tables/links are accurately line-numbered
        content_after_hr = (
            "---\n"
            "name: hr-skill\n"
            "description: Tests horizontal rule handling.\n"
            "---\n"
            "\n"
            "Paragraph before horizontal rule.\n"
            "\n"
            "---\n"
            "\n"
            "| Header |\n"
            "| --- |\n"
            "| secret-board |\n"  # line 12
            "\n"
            "Link after HR: [docs](https://example.com/secret-board)\n"  # line 14
        )
        parsed_hr = skill_linter._get_skill_metadata(content_after_hr)
        assert parsed_hr is not None
        errs_hr, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/hr-skill/SKILL.md",
            meta=parsed_hr[0],
            raw_content=content_after_hr,
            rules=[rule],
        )
        self.assertEqual(len(errs_hr), 2)
        self.assertIn("markdown body (line 12)", errs_hr[0])
        self.assertIn("markdown body (line 14)", errs_hr[1])

    def test_blocked_words_occurrence_cap_and_multiple_matches(self) -> None:
        rule_exemptions = self._compile_rule(
            term="mockkeyword",
            pattern=r"(_|\b)mockkeywords?(_|\b)[A-Za-z_]*",
            exemptions=["MOCKKEYWORD_ALLOWED", "MOCKKEYWORD_X"],
        )
        rule_other = self._compile_rule(
            term="secret-board",
            severity="warning",
        )

        # Same line has BOTH an exempt token AND a non-exempt token, PLUS a second rule match
        content = (
            "---\n"
            "name: multi-match-skill\n"
            "description: Uses MOCKKEYWORD_ALLOWED and mockkeyword_bad.\n"
            "---\n"
            "\n"
            "Call setsockopt(MOCKKEYWORD_ALLOWED) on mockkeyword_unallowed and secret-board.\n"
        )
        parsed = skill_linter._get_skill_metadata(content)
        assert parsed is not None
        errors, warnings = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/multi-match/SKILL.md",
            meta=parsed[0],
            raw_content=content,
            rules=[rule_exemptions, rule_other],
        )
        # Frontmatter description should be flagged (because mockkeyword_bad is after MOCKKEYWORD_ALLOWED)
        # Markdown body line 6 should be flagged for mockkeyword_unallowed (error) and secret-board (warning)
        self.assertEqual(len(errors), 2)
        self.assertIn('frontmatter field "description"', errors[0])
        self.assertIn("markdown body (line 6)", errors[1])
        self.assertEqual(len(warnings), 1)
        self.assertIn('Prohibited term "secret-board"', warnings[0])
        self.assertIn("markdown body (line 6)", warnings[0])

    def test_blocked_words_boundary_and_delimiter_matrix(self) -> None:
        rule_ci = self._compile_rule(term="badterm", case_sensitive=False)
        rule_cs = self._compile_rule(term="BadTerm", case_sensitive=True)

        should_match = [
            "badterm",
            "BADTERM",
            "BadTerm",
            "prefix-badterm",
            "badterm-suffix",
            "a-badterm-b",
            "prefix_badterm",
            "badterm_suffix",
            "_badterm_",
            "core.badterm",
            "badterm.arm64",
            "/path/to/badterm/file",
            '"badterm"',
            "'badterm'",
            "(badterm)",
            "[badterm]",
            "`badterm`",
            "badterm, next",
            "badterm: value",
            "badterm; next",
        ]
        for sample in should_match:
            self.assertIsNotNone(
                rule_ci.pattern.search(sample),
                f"Expected case-insensitive rule to match {sample!r}",
            )

        should_not_match = [
            "mybadterm",
            "badterminal",
            "mybadtermcorp",
            "0badterm",
            "badterm9",
            "123badterm456",
        ]
        for sample in should_not_match:
            self.assertIsNone(
                rule_ci.pattern.search(sample),
                f"Expected rule NOT to match alphanumeric substring {sample!r}",
            )

        # Case-sensitive check
        self.assertIsNotNone(rule_cs.pattern.search("Use BadTerm here"))
        self.assertIsNone(rule_cs.pattern.search("Use badterm here"))
        self.assertIsNone(rule_cs.pattern.search("Use BADTERM here"))

    def test_blocked_words_gcl_complex_regexes_and_exemptions(self) -> None:
        # 1. Multi-delimiter character class & optional plural (e.g., samplebrand(-|_|\s)mockgadgets?)
        rule_multi = self._compile_rule(
            term=r"samplebrand(-|_|\s)mockgadgets?",
            pattern=r"(_|\b)samplebrand(-|_|\s)mockgadgets?(_|\b)",
        )
        for valid_hit in (
            "samplebrand-mockgadget",
            "samplebrand_mockgadget",
            "samplebrand mockgadget",
            "samplebrand-mockgadgets",
            "SAMPLEBRAND_MOCKGADGETS",
        ):
            self.assertIsNotNone(
                rule_multi.pattern.search(valid_hit),
                f"Expected pattern to match {valid_hit!r}",
            )
        for non_hit in (
            "samplebrandmockgadget",
            "mysamplebrand-mockgadget",
            "samplebrand-other",
        ):
            self.assertIsNone(
                rule_multi.pattern.search(non_hit),
                f"Expected pattern NOT to match {non_hit!r}",
            )

        # 2. Exemption prefix regression test (verifying short exemptions like MOCKKEYWORD_X
        # allow MOCKKEYWORD_X25 where [A-Za-z_]* matches MOCKKEYWORD_X, but DO NOT allow
        # longer identifiers like mockkeyword_xyz or MOCKKEYWORD_X_INTERNAL)
        rule_ex = self._compile_rule(
            term="mockkeywords",
            pattern=r"(_|\b)mockkeywords?(_|\b)[A-Za-z_]*",
            exemptions=[
                "MOCKKEYWORD_SOCKET",
                "MOCKKEYWORD_IPV",
                "MOCKKEYWORD_X",
            ],
        )
        meta = {"name": "net-skill", "description": "Socket options skill."}

        # Allowed constants (including digit-suffixed constants like MOCKKEYWORD_IPV6 and MOCKKEYWORD_X25)
        allowed_content = (
            "---\n"
            "name: net-skill\n"
            "description: Socket options skill.\n"
            "---\n\n"
            "Use MOCKKEYWORD_SOCKET, MOCKKEYWORD_IPV6, and MOCKKEYWORD_X25.\n"
        )
        errs_allowed, _ = skill_linter._validate_blocked_words(
            skill_file=".agents/skills/net-skill/SKILL.md",
            meta=meta,
            raw_content=allowed_content,
            rules=[rule_ex],
        )
        self.assertEqual(errs_allowed, [])

        # Unallowed identifiers that start with MOCKKEYWORD_X or mockkeyword_
        for unallowed_token in (
            "mockkeyword_xyz",
            "MOCKKEYWORD_X_INTERNAL",
            "mockkeyword_custom",
            "mockkeywords",
        ):
            bad_content = (
                "---\n"
                "name: net-skill\n"
                "description: Socket options skill.\n"
                "---\n\n"
                f"Configure {unallowed_token} in driver.\n"
            )
            errs_bad, _ = skill_linter._validate_blocked_words(
                skill_file=".agents/skills/net-skill/SKILL.md",
                meta=meta,
                raw_content=bad_content,
                rules=[rule_ex],
            )
            self.assertEqual(
                len(errs_bad),
                1,
                f"Expected {unallowed_token!r} to be flagged (not exempted by prefix), got {errs_bad}",
            )

    def test_parse_gcl_policy_with_peer_android_gcl(self) -> None:
        temp_dir = tempfile.mkdtemp()
        try:
            fuchsia_dir = os.path.join(temp_dir, "fuchsia")
            android_dir = os.path.join(temp_dir, "android")
            os.makedirs(fuchsia_dir)
            os.makedirs(android_dir)

            fuchsia_gcl = os.path.join(fuchsia_dir, "common.gcl")
            android_gcl = os.path.join(android_dir, "common.gcl")

            with open(fuchsia_gcl, "w", encoding="utf-8") as f:
                f.write(
                    """
                    final fuchsia_banned_words_with_exemptions =
                        [{
                           blocked_keyword_pattern = '(_|\\b)mockkeywords?(_|\\b)[A-Za-z_]*'
                           allowed_keyword_pattern = [
                             'MOCKKEYWORD_ALLOWED', // inline comment
                           ]
                         }]

                    final fuchsia_only_common_banned_words =
                        fuchsia_banned_words_with_exemptions +
                        map(
                            lambda w: {
                              blocked_keyword_pattern = '(_|\\b)' + w + '(_|\\b)'
                            },
                            [
                              // Comment line to ignore
                              'fuchsiaboard(es)?', // plural comment
                              'plainfuchsiaterm',
                            ])
                    """
                )

            with open(android_gcl, "w", encoding="utf-8") as f:
                f.write(
                    """
                    android_shared_common_banned_words = [
                      '\\\\bsharedandroidterm\\\\b', // trailing comment
                      '\\\\bsecondsharedterm\\\\b',
                    ]
                    """
                )

            rules = skill_linter._parse_gcl_policy(fuchsia_gcl)
            terms = {r.term for r in rules}
            self.assertEqual(
                terms,
                {
                    "mockkeywords",
                    "fuchsiaboard(es)?",
                    "plainfuchsiaterm",
                    "sharedandroidterm",
                    "secondsharedterm",
                },
            )
        finally:
            shutil.rmtree(temp_dir)

    def test_find_blocked_words_policy_path_precedence(self) -> None:
        with tempfile.NamedTemporaryFile(
            "w", suffix=".json", delete=False
        ) as f:
            f.write('{"blocked_words": ["custom-term"]}')
            temp_policy = f.name
        try:
            # 1. Explicit custom_path takes highest priority
            self.assertEqual(
                skill_linter.find_blocked_words_policy_path(temp_policy),
                os.path.abspath(temp_policy),
            )

            # 2. Non-existent custom_path returns None
            self.assertIsNone(
                skill_linter.find_blocked_words_policy_path(
                    "/nonexistent/policy/path.json"
                )
            )

            # 3. Environment variable FUCHSIA_SKILL_LINTER_BLOCKED_WORDS
            with mock.patch.dict(
                os.environ, {"FUCHSIA_SKILL_LINTER_BLOCKED_WORDS": temp_policy}
            ):
                self.assertEqual(
                    skill_linter.find_blocked_words_policy_path(None),
                    os.path.abspath(temp_policy),
                )
        finally:
            os.remove(temp_policy)

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
            # AND a blocked word ("secret-board")
            dirty_skill = os.path.join(dirty_dir, "SKILL.md")
            with open(dirty_skill, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: dirty-skill\n"
                    "description: A skill with formatting and blocked word issues.\n"
                    "---\n\n"
                    "# Overview \n\n\n"
                    "Run on secret-board target.\n"
                )

            rule = self._compile_rule(
                term="secret-board",
                reason="Unannounced board",
                suggestion="Use generic board",
            )

            # 1. Directory traversal in check-only mode
            code, findings = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code, 1)

            # 2. JSON mode (--suggest-fix-in-json) on parent directory
            code_json, findings_json = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code_json, 0)
            self.assertEqual(len(findings_json), 1)
            finding = findings_json[0]
            self.assertEqual(finding["level"], "error")
            self.assertIn("secret-board", finding["message"])
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
                    blocked_word_rules=[rule],
                )
            self.assertEqual(code_stdout, 1)
            self.assertIn(
                "# Overview\n\nRun on secret-board target.\n",
                captured.getvalue(),
            )

            # 4. --fixit mode writes formatting fixes to disk while still returning exit code 1
            code_fixit, _ = skill_linter.do_lint(
                root_path=dirty_skill,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code_fixit, 1)
            with open(dirty_skill, "r", encoding="utf-8") as f:
                updated_disk = f.read()
            self.assertNotIn("# Overview \n\n\n", updated_disk)

            # 5. Without blocked_word_rules (--no-blocked-words), dirty_skill now passes with exit code 0
            code_no_bw, _ = skill_linter.do_lint(
                root_path=dirty_skill,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
                blocked_word_rules=[],
            )
            self.assertEqual(code_no_bw, 0)
        finally:
            shutil.rmtree(temp_dir)

    def test_compile_blocked_word_rule_invalid_regex(self) -> None:
        # Invalid custom pattern should return None instead of raising re.error
        rule = skill_linter._compile_blocked_word_rule(
            term="test", pattern="(?<unclosed_regex"
        )
        self.assertIsNone(rule)

    def test_load_blocked_words_policy_edge_cases(self) -> None:
        yaml_content = """
- term: good-term
- term: bad-regex-term
  pattern: '(?<unclosed_regex'
- term: null
  pattern: valid-pattern
- term: null
"""
        with tempfile.NamedTemporaryFile(
            "w", suffix=".yaml", delete=False
        ) as f:
            f.write(yaml_content)
            temp_yaml = f.name
        try:
            rules = skill_linter.load_blocked_words_policy(temp_yaml)
            terms = [r.term for r in rules]
            self.assertIn("good-term", terms)
            self.assertIn("valid-pattern", terms)
            self.assertNotIn("None", terms)
            self.assertNotIn("bad-regex-term", terms)
            self.assertEqual(len(rules), 2)
        finally:
            os.remove(temp_yaml)

    def test_main_explicit_missing_blocked_words_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\nname: valid-skill\ndescription: valid\n---\n\nBody.\n"
                )

            # 1. Normal mode: sys.exit(1) on nonexistent policy
            test_argv = [
                "skill_linter.py",
                "--blocked-words-file",
                os.path.join(temp_dir, "nonexistent.json"),
                skill_path,
            ]
            with mock.patch("sys.argv", test_argv):
                with self.assertRaises(SystemExit) as cm:
                    skill_linter.main()
                self.assertEqual(cm.exception.code, 1)

            # 2. JSON mode: sys.exit(0) and output JSON error finding
            test_argv_json = [
                "skill_linter.py",
                "--blocked-words-file",
                os.path.join(temp_dir, "nonexistent.json"),
                "--suggest-fix-in-json",
                skill_path,
            ]
            captured_stdout = io.StringIO()
            with mock.patch("sys.argv", test_argv_json), mock.patch(
                "sys.stdout", captured_stdout
            ):
                with self.assertRaises(SystemExit) as cm:
                    skill_linter.main()
                self.assertEqual(cm.exception.code, 0)
                findings = json.loads(captured_stdout.getvalue())
                self.assertEqual(len(findings), 1)
                self.assertEqual(findings[0]["level"], "error")
                self.assertIn("not found", findings[0]["message"])

    def test_blocked_word_in_xml_tag_not_bypassed_by_autofix(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: test-skill\n"
                    "description: Deploy to <secret-board> device.\n"
                    "---\n\n"
                    "# Overview\n\n"
                    "Body text.\n"
                )
            rule = self._compile_rule(term="secret-board")
            code, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
                blocked_word_rules=[rule],
            )
            self.assertEqual(code, 0)
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["level"], "error")
            self.assertIn(
                'Prohibited term "secret-board"', findings[0]["message"]
            )

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

    def test_load_blocked_words_policy_null_and_string_fields(self) -> None:
        yaml_content = """
default_scope: null
blocked_words:
  - term: null-fields-term
    reason: null
    scope: null
    severity: null
    allowed_paths: null
    exemptions: null
  - term: string-list-term
    allowed_paths: "vendor/*"
    exemptions: "string-list-term-ok"
  - term: ""
    pattern: "   "
"""
        with tempfile.NamedTemporaryFile(
            "w", suffix=".yaml", delete=False
        ) as f:
            f.write(yaml_content)
            temp_yaml = f.name
        try:
            rules = skill_linter.load_blocked_words_policy(temp_yaml)
            self.assertEqual(len(rules), 2)
            r0, r1 = rules[0], rules[1]
            self.assertEqual(r0.term, "null-fields-term")
            self.assertEqual(r0.scope, "public_only")
            self.assertEqual(r0.severity, "error")
            self.assertEqual(r0.allowed_paths, [])
            self.assertEqual(r0.exemptions, [])
            self.assertNotEqual(r0.reason, "None")

            self.assertEqual(r1.term, "string-list-term")
            self.assertEqual(r1.allowed_paths, ["vendor/*"])
            self.assertEqual(r1.exemptions, ["string-list-term-ok"])
        finally:
            os.remove(temp_yaml)

    def test_is_path_allowed_segment_boundaries(self) -> None:
        self.assertTrue(
            skill_linter.is_path_allowed(
                "docs/skills/exempt/SKILL.md", ["docs/skills/exempt/**"]
            )
        )
        self.assertTrue(
            skill_linter.is_path_allowed(
                "docs/skills/exempt/SKILL.md", ["docs/skills/exempt"]
            )
        )
        self.assertFalse(
            skill_linter.is_path_allowed(
                "docs/skills/exempt-not-really/SKILL.md",
                ["docs/skills/exempt"],
            )
        )

    def test_parse_gcl_policy_multiple_exemption_blocks(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            gcl_path = os.path.join(temp_dir, "common.gcl")
            with open(gcl_path, "w", encoding="utf-8") as f:
                f.write(
                    """
                    final fuchsia_banned_words_with_exemptions = [
                      {
                        blocked_keyword_pattern = '(_|\\b)firstsecret(_|\\b)[A-Za-z_]*'
                        allowed_keyword_pattern = [
                          'FIRSTSECRET_ALLOWED',
                        ]
                      },
                      {
                        blocked_keyword_pattern = '(_|\\b)secondsecret(_|\\b)[A-Za-z_]*'
                        allowed_keyword_pattern = [
                          'SECONDSECRET_ALLOWED',
                        ]
                      }
                    ]
                    """
                )
            rules = skill_linter._parse_gcl_policy(gcl_path)
            self.assertEqual(
                {r.term for r in rules}, {"firstsecret", "secondsecret"}
            )

    def test_validate_blocked_words_nested_dict_reports_all_siblings(
        self,
    ) -> None:
        rule = self._compile_rule(term="secret-board")
        meta = {
            "name": "valid-name",
            "description": "Valid description",
            "metadata": {
                "primary": "Uses secret-board",
                "secondary": "Also uses secret-board",
            },
        }
        raw = "---\nname: valid-name\ndescription: Valid description\n---\n\nBody.\n"
        errors, _ = skill_linter._validate_blocked_words(
            "src/skills/test/SKILL.md", meta, raw, [rule]
        )
        self.assertEqual(len(errors), 2)
        self.assertTrue(any("metadata.primary" in e for e in errors))
        self.assertTrue(any("metadata.secondary" in e for e in errors))

    def test_android_shared_banned_words_regex_support(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fuchsia_dir = os.path.join(
                temp_dir,
                "devtools",
                "gerritcodereview",
                "banned_words",
                "fuchsia",
            )
            android_dir = os.path.join(
                temp_dir,
                "devtools",
                "gerritcodereview",
                "banned_words",
                "android",
            )
            os.makedirs(fuchsia_dir, exist_ok=True)
            os.makedirs(android_dir, exist_ok=True)

            fuchsia_gcl = os.path.join(fuchsia_dir, "common.gcl")
            with open(fuchsia_gcl, "w") as f:
                f.write("// empty fuchsia gcl\n")

            android_gcl = os.path.join(android_dir, "common.gcl")
            with open(android_gcl, "w") as f:
                f.write(
                    "final android_shared_common_banned_words = [\n"
                    "  '\\\\b(dummyalpha|dummybeta)\\\\b',\n"
                    "  '\\\\bplaindummy\\\\b',\n"
                    "]\n"
                )

            rules = skill_linter._parse_gcl_policy(fuchsia_gcl)
            rule_map = {r.term: r for r in rules}

            self.assertIn("(dummyalpha|dummybeta)", rule_map)
            regex_rule = rule_map["(dummyalpha|dummybeta)"]
            self.assertIsNotNone(
                regex_rule.pattern.search("test dummyalpha test")
            )
            self.assertIsNotNone(
                regex_rule.pattern.search("test dummybeta test")
            )

    def test_is_vendor_path_relative_resolution(self) -> None:
        fuchsia_root = skill_linter.find_fuchsia_root()
        if not fuchsia_root:
            self.skipTest("Fuchsia root not found")

        vendor_test_dir = os.path.join(
            fuchsia_root, "vendor", "google", "test_mock_dir"
        )
        os.makedirs(vendor_test_dir, exist_ok=True)

        original_cwd = os.getcwd()
        try:
            os.chdir(vendor_test_dir)
            self.assertTrue(skill_linter.is_vendor_path("SKILL.md"))
            self.assertTrue(skill_linter.is_vendor_path("."))
            self.assertTrue(skill_linter.is_vendor_path("./subdir/SKILL.md"))
        finally:
            os.chdir(original_cwd)
            try:
                os.rmdir(vendor_test_dir)
            except OSError:
                pass

    def test_blocked_words_reports_matched_token_not_regex_syntax(self) -> None:
        rule = skill_linter._compile_blocked_word_rule(
            term="(dummyalpha|dummybeta)",
            pattern=r"\b(dummyalpha|dummybeta)\b",
            reason="Forbidden term.",
        )
        self.assertIsNotNone(rule)
        assert rule is not None

        meta = {
            "name": "test-skill",
            "description": "This uses dummyalpha in description.",
        }
        raw_content = (
            "---\n"
            "name: test-skill\n"
            "description: This uses dummyalpha in description.\n"
            "---\n\n"
            "Here we reference dummybeta in body.\n"
        )
        errors, _ = skill_linter._validate_blocked_words(
            skill_file="docs/skills/test/SKILL.md",
            meta=meta,
            raw_content=raw_content,
            rules=[rule],
        )

        self.assertGreaterEqual(len(errors), 2)
        for err in errors:
            self.assertNotIn("(dummyalpha|dummybeta)", err)
        self.assertTrue(
            any('Prohibited term "dummyalpha"' in err for err in errors)
        )
        self.assertTrue(
            any('Prohibited term "dummybeta"' in err for err in errors)
        )

    def test_format_code_hint_gated_on_autofixes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            content = (
                "---\n"
                "name: perfectly-formatted\n"
                "description: A perfectly formatted description.\n"
                "---\n\n"
                "# Title\n\n"
                "Prohibited token forbidden-item here.\n"
            )
            with open(skill_path, "w") as f:
                f.write(content)

            rule = skill_linter._compile_blocked_word_rule(
                term="forbidden-item"
            )
            assert rule is not None

            # CLI mode
            logger = logging.getLogger()
            with self.assertLogs(logger, level="INFO") as log_context:
                exit_code, findings = skill_linter.lint_single_skill(
                    skill_file=skill_path,
                    fixit=False,
                    stdout_mode=False,
                    suggest_fix_in_json_mode=False,
                    blocked_word_rules=[rule],
                )
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
                blocked_word_rules=[rule],
            )
            self.assertEqual(len(findings), 1)
            self.assertNotIn(
                "You can run fx format-code to fix these issues.",
                findings[0]["message"],
            )


if __name__ == "__main__":
    unittest.main()
