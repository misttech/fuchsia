# Part 1: Basic interface configuration

We need to ensure the host is correctly enumerating the connected Fuchsia CDC device, and that it
is configured correctly.

## Does the host see the device?

The first thing to determine is if the host’s USB stack is aware of the Fuchsia device. If not,
there’s no network interface to configure. Google’s public USB vendor identifier (VID) is `0x18d1`,
which is used by all Fuchsia USB devices (CDC or otherwise).

Fuchsia devices have a product identifier (PID) in the range of` 0xa000-0xafff`. Use the `lsusb`
command to determine if the host sees any USB-attached Fuchsia device(s), which reports a VID:PID
tuple for any attached device.

Look for an entry in the output mentioning Google Inc. CDC Ethernet. E.g.

You may see other results corresponding to other Google products whose PID is outside the range
of Fuchsia PIDs.


### Caveat: PID=0xa025

Some product configurations do not expose a CDC interface in the enumerated Fuchsia USB device.
An `adb`-only configuration will have a PID of `0xa025`. If you see a Fuchsia device whose PID is
`0xa025`, it intentionally contains no CDC interface, and thus there’s no corresponding network
interface to configure.

## Is the CDC interface named correctly?

Fuchsia CDC network interfaces should be named similar to `zx-XXXXXXXXXXXX`,
with the actual interface name representing the interface’s MAC address.

You must have a setup that will detect and name the Fuchsia CDC interfaces properly. Currently the
fuchsia source tree contains a Puppet script that can be run for machines that support it
[here](https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/scripts/puppet/networkmanager-cfg.pp).
This setup requires that you are using `NetworkManager`, are using Puppet, and are running a
Debian-like Linux distribution. If _all_ of this applies to you, you can run the above script using
the following command:

```posix-terminal
curl -s 'https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/scripts/puppet/networkmanager-cfg.pp?format=TEXT' | base64 -d | sudo puppet apply
```

You can the interfaces exists using:

```posix-terminal
ip -6 --oneline addr | grep 'zx-'
```

This looks for IPv6 interfaces named with a prefix of "zx-", and should render results similar to:

```none {:.devsite-disable-click-to-copy}
bash$ ip -6 --oneline addr | grep 'zx-'
7: zx-c863147051da    inet6 fe80::ca63:14ff:fe70:51da/64 <snip>
```


## Was the interface assigned the Fuchsia CDC profile?

In a nutshell, the local NetworkManager service should configure the interface with a link-local
IPv6 address and not attempt DHCP lease acquisition. The configuration exists in the Fuchsia CDC
NetworkManager connection profile. Ensure the CDC interface is being applied the profile. Use the
`nmcli conn show` command to verify the correct profile was assigned.

```posix-terminal
nmcli conn show
```

In the Fuchsia-CDC profile row, you should see the CDC interface. The UUID is an unimportant opaque
identifier.

```none {:.devsite-disable-click-to-copy}
bash$ nmcli conn show
NAME         UUID                                  TYPE      DEVICE
...
Fuchsia CDC  25b512f5-70e0-4fd3-9bc2-34432a01bc80  ethernet  zx-c863147051da
...
```

The Fuchsia CDC profile configures the host-side interface with an IPv6 link-local address, which
are addresses are on the `fe80::/10` network. Verify with the ip command.

```none {:.devsite-disable-click-to-copy}
bash$ ip -6 --oneline addr | grep 'zx-'
7: zx-c863147051da    inet6 fe80::ca63:14ff:fe70:51da/64 <snip>
```

Here, the address `fe80::ca63:14ff:fe70:51da` is the link-local address of the host-side network
interface. Notably, it is not the address of the connected Fuchsia CDC device.

[Next: Part 2: Device discovery](./part-2-device-discovery.md)
