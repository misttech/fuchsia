# mise on Fuchsia

This mise config is an alternative to modifying your global shell config to
be aware of Fuchsia. Instead you can add mise to your global shell config and
it will automatically set up Fuchsia development tools when your shell's current
directory is inside a Fuchsia checkout.

See https://mise.jdx.dev/about.html for more information about mise.

## Setup

See https://mise.jdx.dev/getting-started.html.

Once you can use `fx` without [setting up Fuchsia environment variables][fdev]
then you're good to go.

## IDE Agent Integration

If you want mise to also update environment variables for shells launched by
coding agents in e.g. Antigravity, then you can still convince the
non-interactive editor terminals to activate mise if you set `BASH_ENV` to a
script that activates mise.

For example:

```sh
mkdir -p ~/.config/mise
echo 'eval "$($HOME/.local/bin/mise activate bash)"' > ~/.config/mise/bash_env
```

Add the following to your `~/.bashrc` or `~/.profile` *before* either bails
early for non-interactive shells:

```sh
export BASH_ENV="$HOME/.config/mise/bash_env"
```

After you restart your development machine the editor's agent should be able
to run `fx`, `ffx`, etc. using mise the same way your interactive shells do.

[fdev]: https://fuchsia.dev/fuchsia-src/get-started/get_fuchsia_source?hl=en#set-up-environment-variables
[add shims]: https://mise.jdx.dev/ide-integration.html#adding-shims-to-path-default-shell