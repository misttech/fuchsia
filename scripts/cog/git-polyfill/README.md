# Git Polyfill

This directory contains a custom `git` wrapper designed to provide Git-like functionality in environments where a standard `.git` directory is missing.

## Usage

When `fx` starts up, it checks for the existence of a `.git` directory in the Fuchsia root. If the `.git` directory is **missing**, `fx` automatically prepends this directory (`scripts/cog/git-polyfill`) to the `PATH`.

This ensures that any subsequent calls to `git` within the `fx` environment are handled by this wrapper.

## Supported Commands

The following `git` subcommands are currently implemented or intercepted:

*   **`rev-parse`**: Supports `HEAD` to retrieve the current commit hash (e.g., from Cog metadata).
*   **`status`**: Currently a placeholder (prints "not implemented yet").
*   **`ls-files`**: Polyfilled to support listing files in the workspace. This only works on some directories at the moment.

Any command not explicitly registered in `git.py` will fail with a "not yet implemented" message.

## Logging

You can enable logging for debugging purposes by setting the `FUCHSIA_LOG_GIT_POLYFILL_COMMANDS` environment variable to a file path.

```bash
export FUCHSIA_LOG_GIT_POLYFILL_COMMANDS="/tmp/git-polyfill.log"
```

This will cause the wrapper script to append logs to the specified file.

## Working Directory & Environment

The `git` wrapper script changes the current working directory to the location of the script (`scripts/cog/git-polyfill`) before executing `git.py`. This is necessary for `hermetic-env` to function correctly.

**Developers implementing new subcommands must be aware of this:**

*   `os.getcwd()` will return the `git-polyfill` directory, NOT the user's current directory.
*   The user's original working directory is passed via the `--invoker-cwd` argument and is available in `context.invoker_cwd`.
*   If your command needs to operate on files relative to where the user ran the command, you MUST use `context.invoker_cwd`.

## Adding New Subcommands

To add support for a new `git` subcommand:

1.  Open `scripts/cog/git-polyfill/git.py`.
2.  Define a new class that inherits from `GitSubCommand`.
3.  Implement the `execute` method.
4.  Decorate the class with `@register_command("your-command-name")`.

### API for Subcommands

The `git.py` script provides a `Context` object to each subcommand. This context holds the parsed arguments and other environment information.

#### `context.args`

The `context.args` attribute is an `ArgsCollection` dataclass that categorizes the command-line arguments:

*   **`context.args.polyfill_args`**: Arguments intended for the polyfill itself (e.g., `--real-git`, `--invoker-cwd`).
*   **`context.args.global_git_args`**: Global git arguments (e.g., `-C`, `--git-dir`) that appear before the subcommand. These are available as raw strings in this list.
*   **`context.args.command_name`**: The name of the git subcommand (e.g., `status`, `ls-files`).
*   **`context.args.remaining_args`**: Arguments specific to the subcommand (e.g., `--cached`, file paths). These are available as raw strings.

#### `context.global_git_options`

This attribute contains the *parsed* global git arguments (like `-C` or `--git-dir`) as an `argparse.Namespace`. Use this if you need to inspect the value of global flags.

#### `context.git_subcommand_args`

This attribute holds the *parsed* subcommand arguments as an `argparse.Namespace`. This is populated automatically before `execute` is called, based on the arguments defined in your `add_arguments` method.

### Example

```python
@register_command("my-command")
class MyCommand(GitSubCommand):
    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--foo", help="Example argument")

    def execute(self, context: Context) -> int:
        # Access parsed arguments
        args = context.git_subcommand_args
        context.print(f"Executing my-command with foo={args.foo}")

        # Access raw arguments if needed
        context.print(f"Raw remaining args: {context.args.remaining_args}")
        return 0
```

### Fallback to Real Git

If you need to invoke the actual system `git` binary (e.g., to run a command that works even without a `.git` dir, or to query a remote), you can access the path to the real git binary via `context.real_git`.

The wrapper script (`git`) automatically finds the next `git` in the `PATH` and passes it to the Python script.
