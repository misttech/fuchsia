# display-tweak - display driver debugging tool

`display-tweak` is a tool intended to help debug display drivers and hardware,
by tuning low-level parameters. The tool includes multiple commands, all of
which should have very straightforward mappings to FIDL calls into the display
driver system.

More complex testing code should be hosted in the `display-tool` sibling tool.

## Service discovery

`display-tweak` connects to `fuchsia.hardware.display.Service` (for `panel`)
and `fuchsia.hardware.backlight.Service` (for `backlight`). It looks for each
service in `/svc` first, then in `/ns/svc`.

The `/ns/svc` fallback lets the tool run inside another component's namespace
with `ffx component explore`. With the default explore layout, `/svc` holds the
explore shell's own capabilities and the explored component's namespace is
mounted under `/ns`. For example, to turn the panel off using the services
routed to `bootstrap/console-launcher`:

```posix-terminal
ffx component explore bootstrap/console-launcher \
    --tools fuchsia-pkg://fuchsia.com/display-tweak \
    -c "display-tweak panel --power off"
```

The explored component must have the service routed to it.
