# Network connectivity triage

This is intended to help diagnose CDC connectivity issues between your local Linux host and a directly-connected Fuchsia device.
Other non-CDC mechanisms are out of scope for the moment.

The mechanisms behind `ffx` are explored in depth.

This guide is split into three parts, where each part represents a common category of related issues. The three parts are:

* [Part 1: Basic interface configuration](./network-connectivity-triage/part-1-basic-interface-configuration.md)
* [Part 2: Device discovery](./network-connectivity-triage/part-2-device-discovery.md)
* [Part 3: The SSH daemon](./network-connectivity-triage/part-3-the-ssh-daemon.md)

When working with folks to diagnose connectivity issues, most often we find that it’s only one of
the three categories that is impeding connection. These concepts are explored using existing command
line tooling without going through any `fx`/`ffx` workflows.

Lastly, many of the command arguments and example output found below are specific to a testing
machine. While the same ideas apply, the actual interface naming, IP addressing, etc, will be
different on your machine. Take care to substitute the requisite identifiers in the commands shown
below with the actual values on your system.
