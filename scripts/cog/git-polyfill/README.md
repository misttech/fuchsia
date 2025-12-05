# Git Polyfill

This directory contains a custom `git` wrapper designed to provide Git-like functionality in environments where a standard `.git` directory is missing.

## Usage

When `fx` starts up, it checks for the existence of a `.git` directory in the Fuchsia root. If the `.git` directory is **missing**, `fx` automatically prepends this directory (`scripts/cog/git-polyfill`) to the `PATH`.

This ensures that any subsequent calls to `git` within the `fx` environment are handled by this wrapper.

## Supported Commands

The following `git` subcommands are currently implemented or intercepted:

*   **`rev-parse`**: Supports `HEAD` to retrieve the current commit hash (e.g., from Cog metadata).
*   **`status`**: Currently a placeholder (prints "not implemented yet").

Any command not explicitly registered in `git.py` will fail with a "not yet implemented" message.

## Working Directory & Environment

The `git` wrapper script changes the current working directory to the location of the script (`scripts/cog/git-polyfill`) before executing `git.py`. This is necessary for `hermetic-env` to function correctly.

**Developers implementing new subcommands must be aware of this:**

*   `os.getcwd()` will return the `git-polyfill` directory, NOT the user's current directory.
*   The user's original working directory is passed via the `--invoker-cwd` argument and is available in `top_level_args.invoker_cwd`.
*   If your command needs to operate on files relative to where the user ran the command, you MUST use `top_level_args.invoker_cwd`.

## Adding New Subcommands

To add support for a new `git` subcommand:

1.  Open `scripts/cog/git-polyfill/git.py`.
2.  Define a new class that inherits from `GitSubCommand`.
3.  Implement the `execute` method.
4.  Decorate the class with `@register_command("your-command-name")`.

### Example

```python
@register_command("my-command")
class MyCommand(GitSubCommand):
    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--foo", help="Example argument")

    def execute(
        self, top_level_args: argparse.Namespace, args: argparse.Namespace
    ) -> int:
        print(f"Executing my-command with foo={args.foo}")
        return 0
```

### Fallback to Real Git

If you need to invoke the actual system `git` binary (e.g., to run a command that works even without a `.git` dir, or to query a remote), you can access the path to the real git binary via `top_level_args.real_git`.

The wrapper script (`git`) automatically finds the next `git` in the `PATH` and passes it to the Python script.
