---
name: search-driver-docs
description: Find Fuchsia driver documentation.
---

# Search Driver Docs

## When to use this skill

Use this skill when you need to answer questions regarding Fuchsia Driver
Framework concepts, architectures, tutorials, or development guides (DFv1 and
DFv2).

## Process

To navigate and inspect published Fuchsia driver documentation, follow this
multi-stage search strategy:

### 1. Keyword-Based Searching (Direct Grep)

Perform a `grep` based search (keyword based search) directly in the
`/docs/concepts/drivers` and `/docs/development/drivers` directories within the
Fuchsia codebase. When performing this keyword based search, only look up `.md`
files (for example, by filtering for `*.md`).

Try different keywords or search queries up to 7 times if initial searches fail
to yield relevant results.

### 2. Semantic Searching (Fallback)

> **Important:**
> Do NOT open, read, or inspect `driver-docs-index.yaml` for verification,
> cross-referencing, or double-checking if your keyword-based grep searches
> successfully locate relevant documentation. Consulting this index file is
> strictly restricted to cases where all 7 keyword grep attempts fail.

Only if the keyword based search fails after 7 tries, consult the pre-compiled
documentation metadata index stored within this skill directory at:
[driver-docs-index.yaml](assets/driver-docs-index.yaml)

Read the `"description"` fields in `driver-docs-index.yaml` to perform a
semantic search, using your innate contextual comprehension to find the most
conceptually relevant documentation.

### 3. Open and Read the Docs

Once you have isolated matching file paths (either via direct grep or the
index), read the documentation (markdown files) and verify that the contents are
relevant to the user's request.

Note: All paths inside the index are relative to the Fuchsia root directory of
the host machine.

### 4. Return Matched Documents or Report Failure

If relevant documents are found, return them to the caller (e.g., the main
agent), including the page title, description, and file path in YAML frontmatter
format.

If both keyword based search (after 7 tries) and semantic search fail, tell the
caller that no relevant documents were found in the Fuchsia codebase.

## Example Use Cases & Prompts

Here are common scenarios demonstrating how an agent uses this skill to resolve
developer requests via Keyword or Semantic search:

### 1. Exposing Diagnostics in Legacy Code

* **Scenario:** Implement Fuchsia `Inspect` metrics in an un-migrated legacy
  driver.
* **Keyword Search:** Use `grep` to search for `"Inspect"` in
  `/docs/concepts/drivers` and `/docs/development/drivers`.
* **Semantic Search:** If grep fails after 7 tries, review descriptions in
  `driver-docs-index.yaml` for concepts related to *"exposing driver manager
  data for diagnostic query tools in legacy models."*

### 2. Driving Protocol Migrations

* **Scenario:** Convert an old Banjo driver backend over to FIDL.
* **Keyword Search:** Use `grep` to search for `"Banjo"` or `"FIDL"` in
  `/docs/concepts/drivers` and `/docs/development/drivers`.
* **Semantic Search:** If grep fails after 7 tries, review descriptions in
  `driver-docs-index.yaml` for concepts related to *"instructions or examples on
  transferring inter-driver communication interfaces to modernized protocols."*

### 3. Hardware Bring-Up Rules

* **Scenario:** Need procedures for General Purpose Input/Output (GPIO) pin
  initialization.
* **Keyword Search:** Use `grep` to search for `"GPIO"` in
  `/docs/concepts/drivers` and `/docs/development/drivers`.
* **Semantic Search:** If grep fails after 7 tries, review descriptions in
  `driver-docs-index.yaml` for concepts related to *"statically configuring pin
  multiplexing or drive strengths during board initialization."*

### 4. Legacy Driver Binding Adjustments

* **Scenario:** A legacy driver fails to bind to its hardware, requiring an
  update to its old-style bind rules without accidentally applying modern DFv2
  paradigms.
* **Keyword Search:** Use `grep` to search for `"driver binding"` in
  `/docs/concepts/drivers` and `/docs/development/drivers`.
* **Semantic Search:** If grep fails after 7 tries, review descriptions in
  `driver-docs-index.yaml` for concepts related to *"how old matching programs
  determine which devices coordinate with which software hooks upon boot-up."*
