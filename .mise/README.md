# mise on Fuchsia

This mise config is an alternative to modifying your global shell config to
be aware of Fuchsia. Instead you can add mise to your global shell config and
it will automatically set up Fuchsia development tools when your shell's current
directory is inside a Fuchsia checkout.

See https://mise.jdx.dev/about.html for more information about mise.

## Setup

See https://mise.jdx.dev/getting-started.html.

Once you can use `fx` & `ffx` without [setting Fuchsia environment variables][fdev]
then you're good to go.

## Antigravity integration

If you've opened Fuchsia using `fuchsia.code-workspace`, the agent's terminal
should also be able to run `fx`/`ffx`/etc commands without additional
modifications.

[fdev]: https://fuchsia.dev/fuchsia-src/get-started/get_fuchsia_source?hl=en#set-up-environment-variables
