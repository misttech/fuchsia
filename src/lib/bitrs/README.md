# bitrs

`bitrs` ("bitters") is a no-std crate for ergonomically specifying layouts of
bitfields over integral types. While the aim is to be general-purpose, the
imagined user is a systems programmer uncomfortably hunched over an
architectural manual or hardware spec, looking to transcribe register layouts
into Rust with minimal fuss.

The heavy lifting is done by the `layout!` procedural macro. Care is taken to
generate readable and efficient code.

## Features

- Generation of a simple, extensible wrapper type around the given integral base
  type;
- Simple, const-friendly builder pattern for constructing layout instances;
- Automatic implementations of the basic, convenient traits one would expect out
  of a thin integral wrapper type:
  - `Copy`, `Clone`
  - `Eq`, `PartialEq`
  - `Default`
  - `From` implementations between layout and base types
  - `Debug`, `Binary`, `LowerHex`, `UpperHex`, `Octal`
- Specification of default and "reserved-as" values, with `new()` respecting
  reserved-as values and `default()` respecting both;
- Iteration over individual bitfield metadata and values;
- Associated constants around masks and shifts for use in inline assembly.

## Example

```rust
use bitrs::layout;

layout!({
    pub struct Example(u32);
    {
        let foo @ 21..14;
        let bar @ 13..10 = 0b11;
        let baz @ 9;
        let frob @ 8..6;
        let __ @ 5..2 = 1;
        let __ @ 1..0;
    }
});

fn main() {
    let example = *Example::default()
        .set_frob(0x7);
    assert_eq!(example.foo(), 0);
    assert_eq!(example.bar(), 0b11);
    assert_eq!(example.baz(), false);
    assert_eq!(example.frob(), 0x7);
    assert_eq!(*example & 0b1100, 0b0100);

    // Will print: `Example { foo: 0x0, bar: 0x3, baz: false, frob: 0x7 }`
    println!("{example}");

    // Or iterate over all fields and and print them individually.
    for (metadata, value) in example {
      println!("{}: {:x}", metadata.name, value);
    }
}
```

## Syntax

To keep the DSL intuitive and formattable, we co-opt a few familiar Rust syntax
elements:

<blockquote>
    <em>Layout</em>:
    <br>
    &nbsp;&nbsp;
        <code>{</code>
            <em>LayoutType</em>
            <code>{</code>
                <em>Bitfield</em>
                <sup>*</sup>
            <code>}</code>
        <code>}</code>
    <br>
    <br>
    <em>LayoutType</em>:
    <br>
    &nbsp;&nbsp;
        <em>
            <a href="https://doc.rust-lang.org/reference/attributes.html">OuterAttribute </a>
        </em>
        <sup>*</sup>
    <br>
    &nbsp;&nbsp;
        <em>
            <a href="https://doc.rust-lang.org/reference/visibility-and-privacy.html">Visibility </a>
        </em>
        <sup>?</sup>
        <br>
    &nbsp;&nbsp;
        <code>struct</code>
        <a href="https://doc.rust-lang.org/reference/identifiers.html">IDENTIFIER </a>
        <code>(</code>
            <em>UnsignedBaseType</em>
        <code>)</code>
        <code>;</code>
    <br>
    <br>
    <em>Bitfield</em>:
    <br>
    &nbsp;&nbsp;&nbsp;&nbsp;
        <em>NamedBitfield</em>
        &nbsp;|&nbsp;
        <em>ReservedBitfield</em>
    <br>
    <br>
    <em>NamedBitfield</em>:
    <br>
    &nbsp;&nbsp;
        <code>#[unshifted]</code>
        <sup>?</sup>
    <br>
    &nbsp;&nbsp;
        <code>let</code>
        <a href="https://doc.rust-lang.org/reference/identifiers.html">IDENTIFIER </a>
        <code>@</code>
        <em>BitRange</em>
        (
            <code>=</code>
            <em>
                <a href="https://doc.rust-lang.org/reference/expressions.html">Expression </a>
            </em>
        )
        <sup>?</sup>
        <code>;</code>
    <br>
    <br>
    <em>ReservedBitfield</em>:
    <br>
    &nbsp;&nbsp;
        <code>let __ @</code>
        <em>BitRange</em>
        (
            <code>=</code>
            <em>
                <a href="https://doc.rust-lang.org/reference/expressions.html">Expression </a>
            </em>
        )
        <sup>?</sup>
        <code>;</code>
    <br>
    <br>
    <em>BitRange</em>:
    <br>
    &nbsp;&nbsp;&nbsp;&nbsp;
        <a href="https://doc.rust-lang.org/reference/tokens.html#integer-literals">INTEGER_LITERAL </a>
    <br>
    &nbsp;&nbsp;|&nbsp;
        <a href="https://doc.rust-lang.org/reference/tokens.html#integer-literals">INTEGER_LITERAL </a>
        <code>..</code>
        <a href="https://doc.rust-lang.org/reference/tokens.html#integer-literals">INTEGER_LITERAL </a>
    <br>
    <br>
    <em>UnsignedBaseType</em>:<br>
    &nbsp;&nbsp;
            <code>u8</code> |
            <code>u16</code> |
            <code>u32</code> |
            <code>u64</code> |
            <code>u128</code>
    <br>
    <br>
</blockquote>

Despite the exclusive `..` token, both endpoints of a _BitRange_ are treated as
inclusive bit indices; see
[Named and reserved fields](#named-and-reserved-fields).

## Generated Code

Using the `Example` struct from the [Example](#example) section, here is how the
macro translates the definition:

### Layout type

The layout type is always a tuple struct wrapping an unsigned integral type,
giving a layout of bitfields over such an integer. The underlying integral type
is referred to as the "base type". In particular, `Example` defines a 32-bit
layout.

### Trait implementations

The basic, convenient traits one might expect of a thinly-wrapped integral type
are implemented for the layout type:

- `Copy`, `Clone`
- `Eq`, `PartialEq`
- `Default` (see [below](#new-and-default-default-and-reserved-as-values))
- `From` implementations between layout and base types
- `Debug`, `Binary`, `LowerHex`, `UpperHex`, `Octal`

`IntoIterator` is also implemented to [iterate](#iteration) over individual
field values and metadata.

### Layout type attributes

Though none are given on `Example`, all attributes annotating the layout type in
the macro are forwarded verbatim to the definition. However, any derivations
that conflict with the above implemented traits will result in a compilation
error.

### Visibility

To keep things simple and practical, only the visibility of the layout type may
be specified; it too is forwarded verbatim from the definition in the macro. All
methods are generated as public. The associated iterator type is also given the
same visibility as the layout type.

### Const-ness

All methods are const where possible. This is limited only by the current
unavailability of const traits, the exception being trait methods.

### Named and reserved fields

Bitfields are defined in the block following the layout type definition, and
each is defined with a let statement of one of the following forms:

- `let $name @ $bit (= $default)?;`
- `let $name @ $high..$low (= $default)?;`
- `let __ @ $bit (= $value)?;`
- `let __ @ $high..$low (= $value)?;`

Reserved fields are denoted by the identifier `__`, and yield no accessors. A
bare bit index `$bit` or range `$high..$low` (inclusive at both ends, despite
the exclusive-range token) indicates the bits covered by the field. Fields that
span a single bit are referred to as _width-1_ fields.

A width-1 field named `foo` will yield a getter and setter of that bit's content
of the forms

```rust
const pub fn foo(&self) -> bool;

const pub fn set_foo(&mut self, value: bool) -> &mut Self;
```

Otherwise, a named field `foo` yields a getter and setter over its range

```rust
const pub fn foo(&self) -> MinWidth<$high, $low>;

const pub fn set_foo(&mut self, value: MinWidth<$high, $low>) -> &mut Self;
```

where `MinWidth<$high, $low>` is the smallest unsigned integral type of bit size
at least `$high - $low + 1`.

Each getter and setter is annotated with an auto-generated doc string
referencing the corresponding bit range (i.e., `TypeName[hi:lo]` or
`TypeName[bit]`). Doc comments on field declarations are also forwarded to the
getter, and appear before the auto-generated line.

A named field may be annotated with `#[unshifted]`, which results in the getter
and setter operating on values in their original bit position within the base
type rather than shifting them down. For example, the setter and getter for a
field at \[19:16\] would expect and return values like `0x50000` instead of
`0x5`. `#[unshifted]` is incompatible with reserved fields.

If an expression is given on the right-hand side of a field declaration, this
indicates a _default_ value in the case of a named field or a _reserved-as_
value in the case of a reserved value. More on that
[below](#new-and-default-default-and-reserved-as-values).

Note that a reserved field with no reserved-as value has no semantic meaning and
is purely for documentation's sake.

### `new()` and `default()`; default and reserved-as values

Reserved-as field values reflect values that fields _must_ have, modeling
hardware requirements in the case of registers. Given that, `new()` will yield
an otherwise-zeroed base value with the reserved-as values set.

Default field values reflect desired defaults, possibly modeling reset values in
the case of registers. `default()` will yield an otherwise-zeroed base value
with _both_ the default and reserved-as values set.

### Associated constants

In C, one would accomplish bitfield manipulation through manual masking and
shifting, usually with an equivalent set of `FOO_MASK` and `FOO_SHIFT`
preprocessor variables. Even though we have more structure at our disposal in
this context, it can sometimes be convenient to have these raw masks and shifts
on hand. One example is when building up a register value in inline assembly.
Accordingly, the layout type will have the associated constants of
`FOO_MASK: $base` and `FOO_SHIFT: usize` for each named field `foo` of width >
1; for a width-1 field named `foo` only a `FOO_BIT: usize` constant will be
defined, representing the shift. Further, `RSVD1_MASK: $base` and
`RSVD0_MASK: $base` are defined, giving the mask of reserved-as bits that should
be set or unset, as well as `DEFAULT: $base` giving the default layout value.

### Iteration

The layout type admits iterators over field values and metadata. An iterator can
be accessed via `iter()`, and `IntoIterator` is implemented by the layout type
and references to it. Its item type is
`(&'static bitrs::FieldMetadata<$base>, $base)`. See `FieldMetadata` for more
info.

Iterators and iteration are both cheap, with the associated metadata being
defined as a static constant.

## Why another crate for bitfields?

There are already a handful out there, so why this one too? It is the author's
opinion that none of those at the time of writing this offer _all_ of the above
features (e.g., around reserved semantics) or the author's desired ergonomics
around register modeling. For example, some constrain field specification by bit
width instead of by an explicit bit range, which is not how registers are
commonly described in official references (plus, the author surely can't trust
himself to do mental math like that).
