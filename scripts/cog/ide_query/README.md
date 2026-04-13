# `fx ide-query`

`fx ide-query` is a tool (currently in development) designed to provide IDE-related metadata and query functionality within a Fuchsia checkout.

## Usage

Once fully implemented, the tool can be invoked via the `fx` command:

```bash
fx ide-query [args]
```

To force a rebuild of the tool from source (useful during development), use the `--dev` flag:

```bash
fx ide-query --dev [args]
```

## Implementation Details

The tool is implemented in Go and lives in `//scripts/cog/ide_query`. It uses a self-bootstrapping mechanism to ensure it remains fast and decoupled from the main Fuchsia build graph for developer workflows.

For more information on the bootstrapping design, see [docs/BOOTSTRAP_DESIGN.md](./docs/BOOTSTRAP_DESIGN.md).
