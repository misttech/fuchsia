# bitrs

`bitrs` ("bitters") is a no-std crate for ergonomically specifying layouts of
bitfields over integral types. While the aim is to be general-purpose, the
imagined user is a systems programmer uncomfortably hunched over an
architectural manual or hardware spec, looking to transcribe register layouts
into Rust with minimal fuss.

TODO(https://fxbug.dev/525077555): Implement and document bitrs.

## Why another crate for bitfields?
There are already a handful out there, so why this one too? It is the author's
opinion that none of those at the time of writing this offer _all_ of the above
features (e.g., around reserved semantics or boilerplate-free, custom field
representations) or the author's desired ergonomics around register modeling.
For example, some constrain field specification by bit width instead of by an
explicit bit range, which is not how registers are commonly described in
official references (plus, the author surely can't trust himself to do mental
math like that).
