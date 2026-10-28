---
name: fuchsia-skill-naming
description: >
  Names, places, formats, and audits Fuchsia AI agent skills. Enforces kebab-
  case names, action-verb patterns, folder paths, and fx, ffx, and FIDL style
  rules. Use when creating, renaming, reviewing, or auditing Fuchsia skills in
  fuchsia.git. Don't use for non-Fuchsia skill naming or general C++ and Rust
  code style outside of skill files.
---

# Fuchsia skill naming and style rules

Use these rules to name, place, format, and audit AI agent skills for Fuchsia.
Good skill names help engineers and AI agents find the right tool fast.

## Quick-check rubric

Check every new or changed Fuchsia skill against these five rules.

- **1. Case and symbols**
  - **Do this:** Use `kebab-case` with small letters, numbers, and hyphens
    (`[a-z0-9-]`).
  - **Avoid this:** Do not use `snake_case`, `camelCase`, spaces, or capital
    letters in the YAML `name`.
- **2. Length**
  - **Do this:** Use 2 to 4 words. Keep the name under 64 chars.
  - **Avoid this:** Do not use 1-word names or long strings. Drop vague words
    like `helper`, `utils`, or `tools`.
- **3. Word order**
  - **Do this:** Start with an action verb (`debug-`, `write-`, `trace-`), a CLI
    tool name (`fx-`, `ffx-`), or `fuchsia-` for global skills.
  - **Avoid this:** Do not use filler words like `skill`, `guide`, `playbook`,
    `how-to`, or `checklist`.
- **4. Folder match**
  - **Do this:** Match the folder name to the YAML `name` in `kebab-case`.
  - **Avoid this:** Do not give the folder and the YAML `name` different words
    or case styles.
- **5. Public content**
  - **Do this:** Keep `fuchsia.git` skills free of non-public board names, chip
    names, and private links.
  - **Avoid this:** Do not put proprietary code names or private URLs in
    `fuchsia.git`.

## Naming patterns by skill type

Structure skill names so humans and agents can spot the scope, action, and code
language right away:

- **General tasks (`verb-noun`):** Use specific, action-oriented phrases.
    - **Good:** `edit-fuchsia-doc`, `verify-snapshot`, `revert-cl`,
      `triage-recent-regression`
    - **Avoid:** `fidl_helper`, `utils`, `testing`, `ota-playbook`
- **CLI tools (`fx-tool` or `ffx-tool`):** Prefix with the tool name so related
  commands group together in the shared global list (`//.agents/skills/`).
    - **Good:** `fx-test`, `fx-test-remote`, `fx-worktree`, `fx-repro`,
      `ffx-inspect`, `ffx-log`
    - **Avoid:** `run_fx_test`, `how-to-use-ffx-profile`, `fxtest`
- **Subsystem workflows (`subsystem-action`):** Prefix with the subsystem name
  to prevent name clashes in the global catalog.
    - **Good:** `netstack3-unit-tests`, `wlan-run-e2e-test`,
      `devicetree-visitor`, `starnix-app-launch-triage`,
      `cpp-to-rust-dispatcher`
    - **Avoid:** `unit-tests`, `e2e-helper`, `app-launch-guide`
- **Multi-language suites (`subsystem-action-language`):** Use when a subsystem
  has separate skills for implementation, testing, or debugging across C++ and
  Rust.
    - **Good:** `driver-interrupt-impl-cpp`, `driver-interrupt-testing-rust`
    - **Avoid:** `cpp_interrupt_driver`, `driver-interrupt-guide`
- **Migrations (`migrate-old-to-new`):** Name both the source and target
  technologies.
    - **Good:** `migrate-banjo-to-fidl`, `migrate-fidl-to-bazel`,
      `migrate-dfv1-to-dfv2`
    - **Avoid:** `banjo-migration`, `dfv2-upgrade-skill`
- **Fuchsia-wide standards (`fuchsia-topic`):** Use `fuchsia-*` to separate
  Fuchsia workflows from general non-Fuchsia tools.
    - **Good:** `fuchsia-skill-naming`, `fuchsia-cpp-guidelines`,
      `fuchsia-gerrit-cli`, `fuchsia-commit-message-auditor`, `fuchsia-debugger`
    - **Avoid:** `naming_guide`, `cpp_rules_checklist`

## Folder and file paths

Place the skill in the `fuchsia.git` folder that fits its scope.

1.  **Global `fuchsia.git` skills:** Place repo-wide skills in
    `//.agents/skills/<skill-name>/SKILL.md` only after testing thoroughly.
    Always use the plural `.agents` folder. Never use `.agent/skills/`.
2.  **Subteam `fuchsia.git` skills:** Place domain skills in
    `//src/<area>/skills/<skill-name>/SKILL.md`,
    `//src/<area>/.agents/skills/<skill-name>/SKILL.md`, or
    `//docs/skills/<skill-name>/SKILL.md`.
3.  **Nested subsystem suites:** You can group subsystem folders by topic, task,
    and code language. Use `kebab-case` for every folder in the path. One good
    example is
    `//src/devices/skills/driver-interrupt/implementation/cpp/SKILL.md`.

## Style rules for fx, ffx, and Fuchsia code

Follow these style rules inside every Fuchsia `SKILL.md` file.

### Skill tools and linting

- Run `fx manage-skills --add=<skill-name>` to add a new Fuchsia skill.
- Run `fx format-code` before you upload a change. This command runs
  `skill_linter` on `SKILL.md` files.
- Keep lines in Fuchsia repo skills at or under 80 chars.
- Do not use raw `<` or `>` brackets in YAML text or prose. The `skill_linter`
  tool strips HTML tags. Use backticks around tags like `<skill-name>`.

### Code blocks and commands

- Use `posix-terminal` instead of `bash` or `sh` for shell code blocks in
  Fuchsia docs.
- Leave out the `$` prompt in `posix-terminal` blocks. This helps users copy
  commands fast.
- Write full `fx` and `ffx` commands. Good examples are `fx test
  //src/devices/bin/driver_manager` and `ffx component show`.

### FIDL and code names

- Write FIDL names as `library/Protocol.Method`, such as
  `fuchsia.hardware.gpio/Gpio.GetInterrupt`.
- Use `UpperCamelCase` for FIDL types and protocols.
- Use `lower_snake_case` for FIDL methods and fields.
- Use root paths like `/sdk/lib/driver/component/cpp` when linking to files in
  `fuchsia.git`. Never use local machine paths.

### Headings and prose

- Use sentence case for all headings (`## Naming patterns by skill type`).
- Do not put backticks or emojis in headings.
- Write in active voice and present tense. Keep sentences short and clear.

## How to audit skills

When you review a skill or run a team audit, follow these steps.

1.  **Read the header and path:** Check the file path, parent folder name, YAML
    `name`, and YAML `description`.
2.  **Test each rule:**
    - Check that the YAML `name` uses only `[a-z0-9-]`.
    - Count the words (2 to 4 words) and chars (max 64).
    - Check that the name starts with an action verb, `fx-`, `ffx-`, a subsystem
      prefix, or `fuchsia-` for global skills.
    - Flag filler words (`skill`, `guide`, `playbook`, `how-to`, `checklist`).
    - Check that the folder name matches the YAML `name` in `kebab-case`. For
      nested subsystem skills, check that every folder uses `kebab-case` and
      matches the YAML `name`.
    - Scan `fuchsia.git` skills for non-public board names or private links.
3.  **Suggest a fix:** For each broken rule, name the rule and give a clean
    `kebab-case` name and target path.
