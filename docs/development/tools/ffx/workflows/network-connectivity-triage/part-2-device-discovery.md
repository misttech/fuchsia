# Part 2: Device discovery

If the network interface is set up and configured correctly, we can begin probing for the presence
of the target device. Note that the Fuchsia emulator (both aemu and qemu) uses different discovery
mechanisms than what is described here, and is out of scope.

This example uses [avahi](https://avahi.org/) to inspect mDNS records. The avahi toolchain talks to
a local daemon, which may or may not be running. If not running, avahi commands will generally fail
with a message of `Daemon not running`.

If needed, simply start a daemon as a foreground process. In a terminal, execute:

```posix-terminal
sudo avahi-daemon
```

The daemon will cache mDNS queries as they are encountered, and can be safely killed when finished.


## Multicast DNS resolution

Device discovery uses multicast DNS (mDNS) to determine the ip address of the fuchsia target.
Fuchsia targets broadcast their IP information to mDNS with a service type of `_fuchsia._udp`.
You can use avahi to query this information with the following command:

```posix-terminal
avahi-browse --resolve _fuchsia._udp
```

This command will appear to hang. It first dumps any records in the avahi cache, and then sits and
listens for mDNS broadcasts. With the `--resolve` flag, avahi subsequently tries to resolve any
records encountered. Resolution yields the IPv6 address of the Fuchsia target device.

```none {:.devsite-disable-click-to-copy}
bash$ avahi-browse --resolve _fuchsia._udp
+ zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
= zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
   hostname = [fuchsia-c863-1470-51da.local]
   address = [fe80::ca63:14ff:fe70:51db]
   port = [5353]
   txt = []
```

Here, you can see a host record was found containing the hostname `fuchsia-c863-1470-51da`, and was
subsequently resolved to `fe80::ca63:14ff:fe70:51db` - the address of the connected Fuchsia target.

## mDNS packet inspection

If no records are found, you can look to see if the target is replying to mDNS queries. mDNS
operates on port 5353, which can be inspected with `tcpdump`. Restrict the command to the requisite
interface and filter by the IPv6 protocol and mDNS port:

```posix-terminal
sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
```

You should see output similar to the following, which shows an mDNS query/response cycle.

```none {:.devsite-disable-click-to-copy}
bash$ sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
13:32:41.725007 IP 169.254.31.167.58994 > 224.0.0.251.5353:
    0 PTR (QU)? _fuchsia._udp.local. (37)
13:32:41.725057 IP 169.254.31.167.58994 > 224.0.0.251.5353:
    0 PTR (QU)? _fastboot._tcp.local. (38)
13:32:43.724131 IP6 fe80::ca63:14ff:fe70:51da.34810 > ff02::fb.5353:
    0 PTR (QU)? _fuchsia._udp.local. (37)
13:32:43.724183 IP6 fe80::ca63:14ff:fe70:51da.34810 > ff02::fb.5353:
    0 PTR (QU)? _fastboot._tcp.local. (38)
13:32:43.725635 IP6 fe80::ca63:14ff:fe70:51db.5353 > fe80::ca63:14ff:fe70:51da.34810:
    0*- [0q] 1/0/3 PTR fuchsia-c863-1470-51da._fuchsia._udp.local. (152)
```

## mDNS firewall/netfilter rules

In some cases there may be a need to check the kernel’s [netfilter](https://www.netfilter.org/)
rules to discover a device via mDNS. If you see something like the following when running
`nft list tables` you may need to adjust your rules.

```none {:.devsite-disable-click-to-copy}
bash$ sudo nft list tables
table ip filter
table ip nat
table inet firewalld
```

Most setups should just have the first two rules. To remove the `firewalld` rule, the following
commands can be executed:

```posix-terminal
sudo systemctl stop firewalld
sudo systemctl disable firewalld
sudo nft delete table inet firewalld
```

mDNS traffic occurs on port 5353 over the `224.0.0.251` and `fe02::fb` multicast addresses
(IPv4 and IPv6 respectively). It may be worth inspecting the `filter` table, looking for any policy
applied to port 5353 or these addresses.

```none {:.devsite-disable-click-to-copy}
bash$ sudo nft list table ip filter
table ip filter {
    ...
}
```

## Multicast ping

Another option to discover the target’s IPv6 address is to ping the local multicast `ff02::1` device
address. This is a predetermined address specified by IANA. For more information, see ch 2.7.1 of
[RFC-4291](https://datatracker.ietf.org/doc/html/rfc4291)

```posix-terminal
ping6 ff02::1%zx-c863147051da
```

Note the local network interface was designated using the scope-id component of an IPv6 address, a
requirement of pinging a multicast address. The `-I` flag could have been used to similar effect.
Ping responses come from the Fuchsia device, and include their IP address information.

```none {:.devsite-disable-click-to-copy}
bash$ ping6 ff02::1%zx-c863147051da
PING ff02::1%zx-c863147051da (ff02::1%zx-c863147051da) 56 data bytes
64 bytes from fe80::ca63:14ff:fe70:51da%zx-c863147051da: icmp_seq=1 ttl=64 time=0.088 ms
64 bytes from fe80::ca63:14ff:fe70:51db%zx-c863147051da: icmp_seq=1 ttl=64 time=0.958 ms
```

Be aware the localhost may itself reply to the multicast ping. Here, `fe80::ca63:14ff:fe70:51da` is
the address of the host-side interface (as determined above), and `fe80::ca63:14ff:fe70:51db` is
the address of the Fuchsia target device.

A quick rule of thumb is that the device responding in far less than a millisecond is generally the
localhost responding, and anything approaching one or more milliseconds to respond is likely the
Fuchsia target device.

[Next: Part 3: The SSH daemon](./part-3-the-ssh-daemon.md)
