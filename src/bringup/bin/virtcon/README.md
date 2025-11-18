# Virtcon

Virtcon is the system terminal. It is a critical part of bringup and
provides graphical output with minimal hardware requrements.

## Design

Virtcon is written in Rust and powered by Carnelian. Carnelian enables
advanced vector graphics and truetype text rendering while maintaining
minimal hardware requirements.

## Goals

* Minimal resource usage.
* Maximize code reuse with Terminal app.
* Boot animation for startup and shutdown.
* Runtime product configuration.
* Flicker free single framebuffer mode.

## Usage

### System reboot

Devices with keyboards may issue a system reboot request by pressing the
Ctrl + Alt + Delete key combination.

Alternatively, devices with a power button may reboot the system by pressing
the power button 3 times in a row. Button press interval must be no more than
2 seconds.

## Roadmap

1. Boot animation chime support.
2. Silent boot system for runtime suppression of chime.

## Testing

Configure

    fx set core.x64 --with //src/bringup/bin/virtcon:tests

Then test

    fx test virtual_console_tests
