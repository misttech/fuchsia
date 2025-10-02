# Troubleshoot network connectivity: Device discovery

Once the network interface is properly configured, you can begin probing for
the presence of the Fuchsia target device.

Note: The Fuchsia emulator (both AEMU and QEMU) uses different discovery mechanisms, which are out
of scope in this documentation.

The examples in this page use [`avahi`][avahi] to inspect mDNS records. The `avahi` toolchain talks
to a local daemon, which may or may not be running. If not running, `avahi` commands will generally
fail with a message saying `Daemon not running`.

When needed, you can start a daemon as a foreground process. In a terminal, run the following
command:

```posix-terminal
sudo avahi-daemon
```

The daemon will cache mDNS queries as they are encountered, and it can be safely killed when
finished.

## Multicast DNS resolution {:#multicast-dns-resolution}

Device discovery uses multicast DNS (mDNS) to determine the IP address of the Fuchsia target
device. Fuchsia target devices broadcast their IP information to mDNS with a service type of
`_fuchsia._udp`.

You can use `avahi` to query this information with the following command:

```posix-terminal
avahi-browse --resolve _fuchsia._udp
```

This command will appear to hang. It first dumps any records in the `avahi` cache, and then sits
and listens for mDNS broadcasts. With the `--resolve` flag, `avahi` subsequently tries to resolve
any records encountered. Resolution yields the IPv6 address of the Fuchsia target device, for
example:

```none {:.devsite-disable-click-to-copy}
$ avahi-browse --resolve _fuchsia._udp
+ zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
= zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
   hostname = [fuchsia-c863-1470-51da.local]
   address = [fe80::ca63:14ff:fe70:51db]
   port = [5353]
   txt = []
```

Here, you can see a host record was found containing the hostname `fuchsia-c863-1470-51da` and
subsequently resolved to `fe80::ca63:14ff:fe70:51db`, which is the address of the connected Fuchsia
target device.

## mDNS packet inspection {:#mdns-packet-inspection}

If no records are found, you can look to see if the target device is replying to mDNS queries. mDNS
operates on port 5353, which can be inspected with `tcpdump`. When running this command, restrict it
to your target interface and filter the results by the IPv6 protocol and mDNS port, for example:

```posix-terminal
sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
```

This command prints output similar to the following, which shows an mDNS query and response cycle:

```none {:.devsite-disable-click-to-copy}
$ sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
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

## mDNS firewall and netfilter rules {:#mdns-firewall-and-netfilter-rules}

In some cases, there may be a need to check the kernel’s [`netfilter`][netfilter] rules to discover
a device via mDNS.

### ufw rules

If you have a system with `ufw` installed, at some point during your checkout and code setup, you
will have needed to run:

```posix-terminal
fx setup-ufw
```

Running this command will have set up several UFW rules for netfilter. To check your `ufw` ruleset
you can run:

```posix-terminal
sudo ufw status
```

You should have something like the following:

```none {:.devsite-disable-click-to-copy}
Status: active

To                         Action      From
--                         ------      ----
Anywhere (v6)              ALLOW       fe80::/10 5353/udp         # Fuchsia MDNS
33331:33340/udp            ALLOW       fe80::/10                  # Fuchsia Netboot Protocol
8083/tcp                   ALLOW       fe80::/10                  # Fuchsia Package Server
Anywhere (v6)              ALLOW       fe80::/10 33340/udp        # Fuchsia Netboot TFTP Source Port
33331:33340/udp            ALLOW       fc00::/7                   # Fuchsia Netboot Prot
ocol
8083/tcp                   ALLOW       fc00::/7                   # Fuchsia Package Server
Anywhere (v6)              ALLOW       fc00::/7 33340/udp         # Fuchsia Netboot TFTP Source Port
Anywhere (v6)              ALLOW       fc00::/7 5353/udp          # Fuchsia MDNS
```

If this isn't the case (i.e. the output is empty) you'll need to run `fx setup-ufw`.

### netfilter rules

There may also be additional netfilter restrictions in place.

#### firewalld

If you see something like the following when running the `nft list tables` command, you may need to
adjust your rules:

```none {:.devsite-disable-click-to-copy}
$ sudo nft list tables
table ip filter
table ip nat
table inet firewalld
```

Most setups will likely have the first two rules.

To remove the `firewalld` rule, you can run the following commands:

```posix-terminal
sudo systemctl stop firewalld
sudo systemctl disable firewalld
sudo nft delete table inet firewalld
```

mDNS traffic occurs on port 5353 over the `224.0.0.251` and `fe02::fb` multicast addresses
(IPv4 and IPv6 respectively). It may be worth inspecting the `filter` table, looking for any policy
applied to port 5353 or these addresses, for example:

```none {:.devsite-disable-click-to-copy}
$ sudo nft list table ip filter
table ip filter {
    ...
}
```

### Further netfilter checks

If you've tried the above and the following is true:

* You do not have `ufw` installed on your machine.
* You do not have `firewalld` running on your machine.
* You are not able to resolve the device's address with `ffx`.
* You _are_ able to resolve the device's address with `avahi` per
  [the above section](#multicast-dns-resolution).

You likely still have some firewall rule blocking unicast mDNS messages. You may need to add
exceptions `iptables` manually in order to allow mDNS for link-local IPv6 messages:

```posix-terminal
sudo ip6tables -A INPUT -s fe80::/10 -d ::/0 -p udp --sport 5353 -j ACCEPT
sudo ip6tables -A INPUT -s fc00::/10 -d ::/0 -p udp --sport 5353 -j ACCEPT
```

If this does not work, you may want to observe what kinds of firewall rules might be affecting IPv6
traffic address this accordingly:

```posix-terminal
sudo iptables -L -n
```

# Multicast ping {:#multicast-ping}

Another option to discover the target’s IPv6 address is to ping the local multicast `ff02::1` device
address.

```posix-terminal
ping6 ff02::1%zx-c863147051da
```

This is a predetermined address specified by IANA. For more information, see ch 2.7.1 of
[RFC-4291][rfc-4291].

Note the local network interface was designated using the `scope-id` component of an IPv6 address,
a requirement of pinging a multicast address. The `-I` flag could have been used to similar effect.
Ping responses come from the Fuchsia device and include their IP address information, for example:

```none {:.devsite-disable-click-to-copy}
$ ping6 ff02::1%zx-c863147051da
PING ff02::1%zx-c863147051da (ff02::1%zx-c863147051da) 56 data bytes
64 bytes from fe80::ca63:14ff:fe70:51da%zx-c863147051da: icmp_seq=1 ttl=64 time=0.088 ms
64 bytes from fe80::ca63:14ff:fe70:51db%zx-c863147051da: icmp_seq=1 ttl=64 time=0.958 ms
```

Be aware that the localhost may itself reply to the multicast ping. Here, `fe80::ca63:14ff:fe70:51da` is
the address of the host-side interface (as determined above), and `fe80::ca63:14ff:fe70:51db` is
the address of the Fuchsia target device.

A quick rule of thumb is that the device responding in far less than a millisecond is generally the
localhost responding, and anything approaching one or more milliseconds to respond is likely the
Fuchsia target device.

<!-- Reference links -->

[avahi]: https://avahi.org/
[netfilter]: https://www.netfilter.org/
[rfc-4291]: https://datatracker.ietf.org/doc/html/rfc4291
