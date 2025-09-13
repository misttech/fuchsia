# Part 3: The SSH daemon

Once the target’s address is known, SSH connectivity can be verified. You can attempt to directly
connect to the target using

```posix-terminal
ssh -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
```

This will connect and attempt to authenticate using the SSH private key located at
`~/.ssh/fuchsia_ed25519`. If connection fails, pass the `-v` flag to ssh for more verbose
diagnostics:

```posix-terminal
ssh -v -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
```

A lot of output will be produced. Before looking at failures, it helps to understand what a
successful connection involves. Here are the major things you want to look for:

```none {:.devsite-disable-click-to-copy}
bash$ ssh -v -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
...
debug1: Connecting to fe80::ca63:14ff:fe70:51db%zx-c863147051da [fe80::ca63:14ff:fe70:51db%zx-c863147051da] port 22.
debug1: using TCP window size of 4194304 / 4194304
debug1: fd 3 clearing O_NONBLOCK
debug1: Connection established.
...
debug1: Offering public key: /home/user/.ssh/fuchsia_ed25519 ED25519 SHA256:<snip>
debug1: Server accepts key: /home/user/.ssh/fuchsia_ed25519 ED25519 SHA256:<snip>
Authenticated to fe80::ca63:14ff:fe70:51db%zx-c863147051da ([fe80::ca63:14ff:fe70:51db%zx-c863147051da]:22) using "publickey"
...
$
```

You can see the client first establish a connection, and then offers an accepted key for
authentication. If the key is rejected, the session will end with:

```none {:.devsite-disable-click-to-copy}
debug1: Next authentication method: keyboard-interactive
debug1: Authentications that can continue: publickey,keyboard-interactive
debug1: No more authentication methods to try.
user@fe80::ca63:14ff:fe70:51db%zx-c863147051da: Permission denied (publickey,keyboard-interactive).
```

This indicates that the server is not configured with the public-half of the corresponding key.
This almost always means either the Fuchsia target was not provisioned keys, or there is a key
mismatch between the localhost and Fuchsia target.
