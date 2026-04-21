---
name: writing_driver_skills
description: Create and edit Fuchsia driver skills.
---

# Writing Driver Skills

## Follow Common Patterns

### 1. Use Task-First Directory Structure

This structure is **optional but recommended**. Organize skills by the
specific task and target language.
* **Pattern**: `action/language`
* **Example**:
  * `debugging/SKILL.md`
  * `implementation/cpp/SKILL.md`
  * `implementation/rust/SKILL.md`
  * `testing/cpp/SKILL.md`
  * `testing/rust/SKILL.md`

Avoid all-in-one monolith files. A common pattern is to split skills into
**implementation**, **testing**, and **debugging**, and further split by
language if the implementations differ significantly.

### 2. Put Dependencies First

Always include a `## Dependencies` section near the top of the file if
dependencies are required.
* **Pattern**: Provide explicit code blocks for both **GN** and **Bazel**.
* **SDK Targets**: Prefer using SDK targets (e.g., `@fuchsia_sdk//pkg/...`) over
  in-tree paths when available, to ensure the skill is portable for out-of-tree
  developers.
* **Example**: Include comments explaining what the dependency provides.

```gn
deps = [
  # Provides fdf::DriverBase and incoming()
  "//sdk/lib/driver/component/cpp",
]
```

### 3. Group Manifest and Code Snippets

When describing a feature that requires both component manifest (`.cml`) updates
and source code changes, group them together.
* **Pattern**: Show the `.cml` snippet immediately before or after the
  corresponding C++ or Rust code snippet.
* **Example**: Do not put all manifest changes in one section and all code
  changes in another.

### 4. Use If-Else Branching Style

When there are multiple ways to implement a feature (e.g., connecting to a
protocol vs a service), use bolded conditional headers to guide the user.
* **Pattern**:
  * `#### **If** the capability is exposed directly as a protocol:`
  * `#### **Otherwise** (If the capability is exposed within a Service):`

### 5. Follow Link Conventions

Always provide clickable links for file paths, API definitions, and skills.
* **Pattern**: Use links relative to the Fuchsia root (starting with `/`).
* **Example**: `[`fdf_metadata::GetMetadata()`](/sdk/lib/driver/metadata/cpp/metadata.h)`
* **Rule**: Do not use absolute paths containing the user's home directory.
* **Rule**: Always add clickable links when referencing specific libraries or
  data types to help users navigate to the definitions.
* **Rule**: Do not link to local files outside of the Fuchsia repository (e.g.,
  Knowledge Items in the app data directory).
* **Rule**: If linking to external source code viewers like
  `cs.opensource.google`, always link to a specific revision or commit hash to
  ensure the link remains valid even if the file changes or is moved.

### 6. Create Symmetrical Links

Ensure that related skills are cross-linked.
* **Rule**: The implementation guide should link to the testing guide.
* **Rule**: The testing guide should link to the implementation guide.
* **Rule**: Both should link to the relevant debugging guide if one exists.

### 7. Apply Spacing and Formatting

* **Heading Spacing**: Always leave a blank line after any heading (H1 to H4)
  before the content begins. Also ensure there is exactly one blank line after
  the content of a section and before the next heading.
* **FIDL Notation**: When referencing FIDL methods in text, use the slash and
  dot convention: `fuchsia.hardware.gpio/Gpio.GetInterrupt`.
* **Line Wrapping**: Wrap lines at column 80 for readability in text files,
  except for long URLs. Use best effort for line wrapping code.
* **Bullet Point Spacing**: Bullet points should be followed by only one space
  (e.g., `* Bullet`).
* **Trailing Whitespace**: Do not leave trailing whitespaces at the end of lines.
* **Code Block Tags**: Always specify the language for fenced code blocks (e.g.,
  `cpp`, `gn`, `cml`, `bazel`, `bash`) to ensure proper syntax highlighting.

### 8. Include Standard Sections

* **Pitfalls**: Include a `## Common Pitfalls` section to document known edge
  cases, common errors, or non-obvious requirements.
* **Further Reading**: Include a `## Further Reading` section if there are any
  related skills, documentation, or background material to link to.

### 9. Avoid Redundancy

* **No "When to Use" or Description Explainers**: Do not include introductory
  paragraphs or sections (like "When to use this skill" or "Description") that
  describe the skill or when to use it. The YAML frontmatter `description` is
  sufficient for both discovery and context. Start the file directly with the
  content (e.g., dependencies or steps) after the H1 title.

### 10. Write Skill Descriptions

* **Action-Oriented**: The `description` in the YAML frontmatter must start with
  a strong imperative verb (e.g., "Implement", "Test", "Debug", "Identify").
* **No Filler Words**: Avoid starting descriptions with passive phrases like
  "Guide to...", "How to...", or "This skill helps...". These words do not help
  with intent matching and add noise.
* **Conciseness**: Keep the description short, specific, and focused on the
  action the skill enables.

### 11. Use Imperative Headers

* **Imperative Mood**: When listing sequential steps or operations in a guide,
  use strong imperative verbs for the headings (e.g., "Update Headers" instead
  of "Headers"). This makes the guide read like an actionable checklist.

### 12. Follow Project Language Rules

* **Adhere to GEMINI.md**: Ensure all code snippets adhere to the Fuchsia-specific
  coding style and constraints defined in `GEMINI.md`.
