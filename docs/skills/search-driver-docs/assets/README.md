# Generating the driver documentation index file

This page explains how the `driver-docs-index.yaml` file is generated
and updated using a local **Gemma 4** model, which is executed directly
from a terminal using the `litert-lm` command.

## Overview

The `driver-docs-index.yaml` file serves as a metadata catalog that
maps driver documentation paths to titles, descriptions, and keywords.

To automate the generation and maintenance of this index file,
without relying on external Gemini 3 models via API, you can run
a local instance of the **Gemma 4 E4B** model.

The index file generation pipeline includes the following steps:

- **Local model host**: Download and run Gemma 4 E4B locally using
  the `litert-lm` tool.

- **Function-calling script**: Use the in-tree Python script
  (`extract_metadata.py`, located at
  `docs/skills/search-driver-docs/scripts/extract_metadata.py`)
  that exposes a `read_file` tool. This allows the local Gemma model
  to read Fuchsia documentation files directly from the host machine's
  local Fuchsia checkout setup and extract keywords and descriptions.

  > **Note:**
  > For security, the `read_file` tool strictly validates and restricts
  > file access to paths inside the Fuchsia checkout setup.
  > Any attempts to access files outside the repository boundaries
  > will be rejected.

- **Automation script**: Run the in-tree Python automation script
  (`generate_index.py`, located at
  `docs/skills/search-driver-docs/scripts/generate_index.py`)
  to scan target directories, query the local Gemma model, parse the
  output, and deterministically compile and sort the index.

## Step 1: Gemma model setup and download

To download and prepare the Gemma 4 E4B model on your host machine,
complete the following steps:

1. Install the `uv` and `litert-lm` tools on the host machine,
   for example:

   ```bash
   pipx install uv
   ```

   ```bash
   uv tool install litert-lm
   ```

2. Import the Gemma 4 E4B model from the Hugging Face repository:

   ```bash
   litert-lm import --from-huggingface-repo=litert-community/gemma-4-E4B-it-litert-lm gemma-4-E4B-it.litertlm gemma4-e4b
   ```

   This command downloads the model weights to your local
   `~/.litert-lm/models` directory.

3. Confirm that the Gemma 4 E4B model is available locally:

   ```bash
   litert-lm list
   ```

   This command prints output similar to the following:

   ```text
   Listing models in: $HOME/.litert-lm/models
   ID                          SIZE            MODIFIED
   gemma4-e4b                  2.4 GB          2026-05-08 23:26:00
   ```

4. Verify that the local model and function-calling script
   are configured correctly by running a test extraction
   (from the $FUCHSIA_DIR directory):

   ```bash
   litert-lm run gemma4-e4b \
     --preset=docs/skills/search-driver-docs/scripts/extract_metadata.py \
     --prompt="Process the file at docs/concepts/drivers/driver_framework.md"
   ```

   When this command is executed successfully, it prints
   the extracted single-sentence description and keywords
   for the `driver_framework.md` file.

## Step 2: Generate the index file using the automation script

Once the local model is prepared, you can automatically generate
and update the `driver-docs-index.yaml` file by executing the
Python automation script:

1. Ensure that the `FUCHSIA_DIR` environment variable is set
   to your Fuchsia checkout root directory (for example,
   `$HOME/fuchsia`):

   ```bash
   export FUCHSIA_DIR=/path/to/your/fuchsia/checkout
   ```

2. Run the `generate_index.py` script:

   ```bash
   python3 docs/skills/search-driver-docs/scripts/generate_index.py
   ```

### Key features of the generate_index.py script

The `generate_index.py` script will:

- Automatically find all driver documentation `.md` files under
  the target directories: `docs/development/drivers/` and
  `docs/concepts/drivers/`.

- Extract document titles from the first heading of each file.

- Run the local Gemma model via the `litert-lm` tool using the
  preset configuration.

- Parse keywords and descriptions from the model's output.

## Error handling by the generate_index.py script

- **Retry failures (resilience)**: Automatically retries metadata
  extraction up to 3 times per document if the model returns
  an invalid format or empty description, making the script highly
  tolerant of minor model generation hiccups.

- **Continue on error**: If a file fails all 3 attempts, the
  script logs the error, tracks the failure, and seamlessly
  continues processing the rest of the batch.

- **Deterministic incremental save**: Progress is saved to
  `docs/skills/search-driver-docs/assets/driver-docs-index.yaml`
  immediately after each successfully indexed file (sorted
  alphabetically by path) so that no progress is lost if interrupted.

### Post-run summary report from the generate_index.py script

At the end of execution, the `generate_index.py` script prints
a final summary report to console. If any driver documents failed
to index after all 3 retry attempts, they will be explicitly
listed at the bottom of the report to inform you which documents
require manual attention or formatting fixes.

Example output in case of a document failure:

```text
==================================================
             INDEX GENERATION REPORT
==================================================
Total files processed: 9
Successfully indexed:  8
Failed to index:        1

The following files failed to yield valid metadata after 3 retries:
  - docs/concepts/drivers/some_faulty_doc.md
==================================================
```

