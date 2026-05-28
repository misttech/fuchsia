---
name: fuchsia-readme-generator
description: >
  Guidelines and workflow for generating or updating README.md files for
  Fuchsia, with general software practices and Fuchsia-specific agent record-
  keeping.
---

# Fuchsia README Generator Skill

This skill guides you in generating a new `README.md` or updating an existing
one for a software project within the Fuchsia codebase, with special
considerations and record-keeping conventions for AI agents.

## When to use this skill

Use this skill ONLY when:
1.  A developer explicitly asks you to write or update a `README.md` file.
2.  You are refactoring a project and want to ensure its primary `README.md` is
    updated to reflect architectural or usage changes.

Do NOT use this skill for other types of markdown documentation (such as design
documents, tutorials, API specs, or user guides), which may have different
styling and structure rules.

## Process

### 1. Context Gathering (No Guessing)

Gather context about the codebase/directory before drafting the README.
* **General Software Projects:** Identify the programming language (primarily
  C++ and Rust. Note that Python and Bash are frequently used for writing
  developer tools and scripts, but are not the main languages used for writing
  Fuchsia system components), main dependencies, build system (e.g., GN, Bazel,
  Cargo, CMake, pip), entry points, and test frameworks.
* **Fuchsia Projects:**
    * Identify the project path (e.g., `//src/connectivity/...`).
      * **WARNING:** Do not include any information specific to files,
        repositories, or documentation under the `//vendor` directory. The
        `//vendor` directory may contain proprietary or private information that
        must remain confidential and should never be leaked into general or
        public-facing documentation.
    * Analyze `BUILD.gn` or `BUILD.bazel` for target names, test targets, and
      FIDL dependencies.
    * Analyze source code to identify main entry points (e.g., main.rs, main.cc)
      and determine if the project exposes system diagnostics via the Inspect
      API or writes structured logs (look for inspect or logging libraries in
      the source).
      * *Why this matters:* In Fuchsia, diagnostics and logging are the primary
        telemetry interfaces for debugging and observability. Documenting these
        interfaces in the README is critical for developers (and AI agents) to
        know how to inspect system health and troubleshoot failures using tools
        like `ffx inspect` and `ffx log`.

### 2. Structuring a Good README

A good README should be structured logically, ensuring both humans and AI agents
can quickly understand how to use, update, and maintain the codebase.

#### A. Standard Structure (For All Projects)

1.  **Title & High-Level Summary:**
    * Clear H1 project title.
    * Concise explanation of what the software does, the problem it solves, and
      key features.
2.  **Getting Started (Optional):**
    * *Note:* Not all README.md files require a "Getting Started" section (e.g.,
      simple utility libraries or static configs). Adjust the structure
      accordingly.
    * **Prerequisites:** Software, runtimes, or environment configurations
      required.
    * **Installation/Setup:** Step-by-step commands to install and configure.
    * **Quick Start:** A minimal working example or basic usage command.
3.  **Usage Guide (Optional):**
    * *Note:* Simple libraries or configuration-only repositories might not
      require a usage guide. Omit if redundant.
    * Detailed code snippets or CLI command examples.
    * Configuration options (environment variables, config files).
4.  **Running Tests:**
    * Exact commands to run unit, integration, or end-to-end tests.
5.  **Contributing & License:**
    * How to contribute and the licensing terms.

#### B. Fuchsia-Specific Record-Keeping

For projects in the Fuchsia codebase, the README serves as a critical system of
record for developers and AI agents. Ensure you include:

1.  **Fuchsia Build Configuration:**
    * Provide the exact `fx set` command to include the project and its tests.
    * *Example:* `fx set workstation_eng.x64 --with
      //src/my/project,//src/my/project:tests`
2.  **Topology & Lifecycle (if applicable):**
    * Explain where the component sits in the topology (realms, monikers) and
      how it is started.
3.  **FIDL Boundaries (if applicable):**
    * If the project exposes or interacts with FIDL APIs, explicitly list and
      link the FIDL protocols implemented or consumed. This is vital for agents
      tracking API dependencies.
4.  **Diagnostics & Debugging (if applicable):**
    * Document the exact commands to watch logs: `ffx log --set-severity
      core/my-project#DEBUG`
    * Document the exact commands to inspect state: `ffx inspect show
      core/my-project`
5.  **AI Agent Record-Keeping (Developer Notes):**
    * **Crucial Context:** If there are specific quirks, known issues, or
      architectural decisions (e.g., why a certain sync pattern was used),
      document them clearly. This prevents future AI agents from re-introducing
      bugs or using incorrect APIs.
    * *Example:* "Note: Do not use async tasks here due to X constraint."
6.  **Tracked Design Patterns & Best Practices:**
    * **Feedback and Review Comments:** Document any specific design patterns,
      best practices, or style conventions mentioned by developers in code
      review feedback or comments. This helps preserve institutional knowledge
      for future development and automated agent tasks.

### 3. Drafting the Content

> **IMPORTANT: Markdown Line Wrapping (80-Character Limit)**
> You MUST manually wrap all body/prose text in the `README.md` file to a strict
> **80-character limit per line**. Do NOT write long single-line paragraphs. Most
> formatters (like `fx format-code`) do NOT wrap prose lines automatically in the
> Fuchsia codebase, so this formatting must be performed by the agent itself.
> * *Exclusions:* Do not wrap code blocks (text inside triple backticks), markdown
>   tables, headers (`#`), or blockquotes (`>`), as wrapping these will break
>   their syntactic layout.

* **Actionable Commands:** Ensure all commands are up-to-date (e.g., using `ffx`
  instead of legacy tools in Fuchsia).
* **Links:** Use relative links for in-tree files to maintain documentation
  integrity.
* **Preserve Existing Content (Minimal-Disruption Restructuring):** When editing
  or updating an existing `README.md` file, you are encouraged to adjust the
  **structure** to match the recommended layout guidelines (e.g., grouping
  build/run under "Getting Started", separating "Running Tests", and creating a
  "Diagnostics & Observability" section). However, you must make these
  structural changes in a way that **preserves the original text content** as
  much as possible. Move entire paragraphs, lists, and code blocks into their
  new headers rather than rewriting them. Only edit or refine the original text
  where necessary to correct outdated instructions, migrate legacy commands, or
  bridge new structural sections.

### 4. Verification (Optional & Non-Blocking)

Fuchsia builds and test executions can be extremely time-consuming. To ensure a
responsive developer experience:
1.  **Do Not Block on Verification:** Deliver the initial draft of the
    `README.md` immediately using static analysis. Never block draft delivery on
    running a build or a test.
2.  **Run Asynchronously:** If verification of the documented commands (e.g.,
    running `fx test`) is practical, launch it as a background task *after*
    presenting the draft, or run it asynchronously while the user is reviewing.
3.  **State Verification Status:** Clearly inform the developer whether the
    documented commands have been verified in the workspace or if they are
    generated statically from build definitions (e.g., "Note: The `fx test`
    commands are derived from `BUILD.gn` and have not been run locally").
