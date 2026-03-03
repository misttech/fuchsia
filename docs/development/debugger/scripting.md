# Automating zxdb with scripts

Zxdb provides scripting support that lets you automate repetitive debugging
tasks, jump to a specific application state, and verify debugger behavior in an
automated way. Scripts are plain text files containing a sequence of zxdb
commands alongside their expected output.

## Running a script

To run a script from the command line when launching zxdb, use the `-S` or
`--script-file` option.

* Using `ffx debug`:

```posix-terminal
ffx debug connect -- --script-file=my_script.script
```

* Or using `zxdb` directly (note that this will only work with offline core
  files, like a minidump, so your script should start by loading a core file
  with the `opendump` command. See `help opendump` for details.):

```posix-terminal
zxdb --script-file=my_script.script
```

## Script syntax

Scripts are evaluated line-by-line and generally alternate between entering
commands and matching lines of output.

### Commands

Lines starting with `[zxdb]` are interpreted as commands to run. These commands
are executed the same as they would be in the interactive zxdb console.

```zxdb
[zxdb] attach my_component.cm
```

You can use abbreviations and any standard `zxdb` features:

```zxdb
[zxdb] t * f
```

Which is equivalent to:

```zxdb
[zxdb] thread * frame
```

### Output matching

If a line does not start with `[zxdb]` and is not a comment, it is treated as a
pattern to match against the output of the previously executed command.

Because many commands in zxdb are asynchronous, the debugger needs to know when
a command has actually completed its output before issuing the next one. Zxdb
will wait for the output to match the specified lines before proceeding to the
next command. This ensures your script stays synchronized with the debugger's
asynchronous events.

```zxdb
[zxdb] attach cobalt.cm
Attached Process
Done.
```

If you don't care about the output of a command, you don't need to specify any
matching lines. You can write the next `[zxdb]` command, and the previous
command will be allowed to finish immediately.

### Wildcards

If you only want to match parts of a line, or if a line contains unpredictable
data (like memory addresses, process IDs, or file paths), you can use the `??`
wildcard. It matches arbitrary sections of a line (up to the entire line).

```zxdb
[zxdb] frame
▶ 0 MyFunction() • my_file.cc:??
```

### Comments

Lines starting with `#` are comments and are ignored. Comments are useful for
documenting the script, but remember that UTF-8 characters are also supported.

```zxdb
# This is a comment explaining what the next command does.
[zxdb] pause
# Wait for the stop event
🛑
```

### Out-of-order output

By default, the script expects output lines to arrive in the exact order
specified. If the command produces output where the order is not guaranteed, you
can add `## allow-out-of-order-output` anywhere in the block of expected output.
This ensures all the specified lines match, regardless of the order they are
printed in. This directive must come immediately after the `zxdb` command line
and before any expected output that is to be matched for the matching algorithm
to correctly take into account all output.

```zxdb
[zxdb] thread
## allow-out-of-order-output
  1 State: Running
  2 State: Suspended
```

### Exiting the script

Once the script completes, you will automatically be dropped into the
interactive `[zxdb]` command line.

If you prefer zxdb to exit automatically at the end of the script instead, you
can append a `quit` command at the end of the file. However, ensure that the
previous command has matching output specified, otherwise `quit` might be
executed before the previous command has finished.

```zxdb
[zxdb] quit --force
```

## Examples

### Capture async backtrace

This script attaches to a component, pauses it, prints the async backtrace, and
leaves you in the interactive console to investigate further.

```zxdb
# Attach to the component
[zxdb] attach hwinfo.cm
Attached Process
Done.

# Pause execution
[zxdb] pause
🛑

# Output the tree of async tasks.
[zxdb] async-backtrace
```

### Print all threads' frames

This script attaches to a component, waits for it to stop, prints the
backtraces of all threads, and then drops into the interactive console.

```zxdb
[zxdb] attach cobalt.cm
Attached Process
Done.

[zxdb] pause
🛑

# Output the backtraces of all threads of the current process.
[zxdb] thread * frame
```
