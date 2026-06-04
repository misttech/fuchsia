# `spmi-hwreg` - Rust SPMI register access library

`spmi-hwreg` provides highly ergonomic, type-safe, and asynchronous access to SPMI registers on Fuchsia, matching the MMIO `hwreg` paradigm.

## Targets

This library provides two build targets to support different FIDL backends:

- **`spmi-hwreg`**: Uses the legacy Fuchsia FIDL bindings (`fidl_fuchsia_hardware_spmi`).
- **`spmi-hwreg-next`**: Uses the next-generation, zero-copy FIDL bindings (`fidl_next_fuchsia_hardware_spmi`).

Both targets share the same core register definition macros and logic in `common.rs`, ensuring API compatibility and minimizing code duplication.

## Features

### Individual register definitions (`spmi_register!`)
Define single registers with bitfield extraction, masking, and helper setter functions:

```rust
spmi_register! {
    test_reg, u16, 0x10, RW, LE, {
        pub field1, set_field1: 7;
        pub field2, set_field2: 15, 8;
        pub const SUCCESS: u8 = 0x82;
    }
}
```

### Endianness support
Support both little-endian (`LE`) and big-endian (`BE`) registers natively:

```rust
spmi_register! {
    test_be_reg, u16, 0xEF, RW, BE, {
        pub flag, set_flag: 4;
        pub field, set_field: 3, 0;
    }
}
```

> [!NOTE]
> For 1-byte width registers (`u8`), specifying the endianness is completely optional as byte-order is irrelevant. The macro exposes a specialized arm for `u8` to omit it entirely.

### Register block access (`spmi_register_block!`)
Consolidate multiple registers into a single type-safe block tied to the SPMI FIDL client:

```rust
spmi_register_block! {
    pub struct TestRegs {
        pub test => test_reg,
    }
}

let regs = TestRegs::new(proxy);
let mut val = regs.test().read().await?;
let is_set = val.field1();
val = val.set_field2(0x5);
regs.test().write(val).await?;
```

Alternatively, initialize a new value from scratch using `Default` (which defaults to `0`):

```rust
let val = test_reg::Value::default()
    .set_field1(true)
    .set_field2(0x5);
regs.test().write(val).await?;
```
### Multi-register contiguous access (`spmi_read_contiguous!`, `spmi_write_contiguous!`)
Perform atomic multi-register contiguous reads and writes type-safely in exactly one async FIDL call:

```rust
// Read both 'general' and 'status' registers in one call:
let (mut general_val, status_val) = spmi_read_contiguous!(
    &regs,
    my_reg,
    status_be_reg
).await?;

general_val = general_val.set_field1(true);

spmi_write_contiguous!(
    &regs,
    my_reg => general_val,
    status_be_reg => status_val
).await?;
```

> [!IMPORTANT]
> These macros perform **compile-time contiguity validation** using `const` assertions. If you attempt to group non-contiguous registers, the code will fail to compile with a clear error message, preventing accidental wrong-address accesses at runtime.

### Typestate access modes (`ReadOnly`, `WriteOnly`, `ReadWrite`)
Use typestate marker traits to restrict access modes at compile-time:

- **ReadOnly**: Only exposes the `read()` method.
- **WriteOnly**: Only exposes the `write()` method.
- **ReadWrite**: Exposes both `read()` and `write()` methods.

### Enum field support
Define named states for bitfields using Rust enums either out-of-line or inline:

#### Out-of-line enum
```rust
#[repr(u16)]
pub enum PowerMode {
    Normal = 0,
    Hibernate = 1,
    Unknown = 0xFFFF,
}

impl PowerMode {
    pub const fn from_val(val: u16) -> Self {
        match val {
            0 => PowerMode::Normal,
            1 => PowerMode::Hibernate,
            _ => PowerMode::Unknown,
        }
    }
}

spmi_register! {
    mode_reg, u16, 0x0A, RW, LE, {
        pub enum PowerMode, mode, set_mode: 3, 2;
    }
}
```

#### Inline enum
```rust
spmi_register! {
    mode_reg, u16, 0x0A, RW, LE, {
        pub enum PowerMode {
            Normal = 0,
            Hibernate = 1,
        }, mode, set_mode: 3, 2;
    }
}

let mut val = regs.mode().read().await?;
val = val.set_mode(mode_reg::PowerMode::Hibernate);
```

## Testing

Run unit tests with:
```posix-terminal
fx test //src/devices/spmi/lib/hwreg-spmi/rust:tests
```
