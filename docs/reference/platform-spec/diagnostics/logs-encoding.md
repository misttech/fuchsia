# Encoding structured records

Fuchsia uses a wire format for structured logs inspired by its [trace format]. This transmission
format allows consumption and propagation of arbitrary data structures as log records.

## Validation

The size of a log record is capped at 32kb.

Writers that send oversize or invalid records to the diagnostics service will have their streams
closed and an error recorded by the diagnostics service.

## Primitives

Integers have little endian encoding. Signed integers are twos-complement.

Timestamps are signed 64-bit integers measured in nanoseconds, as recorded by
[`zx_clock_get_monotonic`].

Strings are denoted by a 16-bit "string ref". If the most significant bit (MSB) is 0, the string is
empty. If the MSB is 1, the remaining bits of the ref indicate the length of the subsequent UTF-8
byte stream. All string refs with an MSB of 0 other than the empty string are reserved for future
extensions. Strings are padded with zeroes until 8-byte aligned.

Records consist of a number of 8-byte words and are 8-byte aligned.

### Header

The required metadata for a log record are a record type, the overall length of the record, and a
timestamp. These are encoded in the first 16 bytes of a record:

Note: For reference on how to read the bit field diagrams, see
[Bitfield Diagram reference][bitfield-diagram].

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| T | SizeWords | Reserved                            | Severity|
|---------------------------------------------------------------|
| TimestampNanos                                                |
'---------------------------------------------------------------'

T (type)       = {0,3}    must be 9
SizeWords      = {4,15}   includes header word
Reserved       = {16,55}  must be 0
Severity       = {56,63}  severity of the log record
TimestampNanos = {64,127}
```

Currently all records are expected to have type=9. This was chosen to mirror the [trace format] but
may require a change before these records can be processed by tracing tools.
Values for severity are defined in
[/sdk/fidl/fuchsia.diagnostics/severity.fidl](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.diagnostics/severity.fidl)

## Arguments

Log record data is conveyed via a list of typed key-value pairs. The keys are always non-empty
strings, which supports different types of arguments, and the values can have several types.

### Argument header

Each argument has an 8-byte header, followed by the argument name, followed by the argument's value.
The name is padded with zeroes to 8-byte alignment before the argument's content is written.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| T | SizeWords | NameRef     | Varies                          |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'

T (type)  = {0,3}           see table below
SizeWords = {4,15}          includes header word
NameRef   = {16,31}         string ref for the argument name
Varies    = {32,63}         varies by argument type, must be 0 if unused
NameLen   = 64*NameRef
Name      = {64,64+NameLen} name of the argument, padded to 8-byte alignment
```

The first 4 bits of the argument header determine which type the argument has:

T (type) | name
---------|--------------------------
`3`      | signed 64-bit integers
`4`      | unsigned 64-bit integers
`5`      | double-precision floats
`6`      | UTF-8 strings
`9`      | booleans

### Signed 64-bit integer arguments

Signed integers are appended after the argument name is terminated.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| 3 | SizeWords | NameRef     | Reserved                        |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'
| Value                                                         |
'---------------------------------------------------------------'

T (type)  = {0,3}                  must be 3
SizeWords = {4,15}                 includes header word
NameRef   = {16,31}                string ref for the argument name
Reserved  = {32,63}                must be 0
NameEnd   = 64+(64*NameRef)
Name      = {64,NameEnd}           name of the argument, padded to 8-byte alignment
Value     = {NameEnd+1,SizeWords*64}
```

### Unsigned 64-bit integer arguments

Unsigned integers are appended after the argument name is terminated.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| 4 | SizeWords | NameRef     | Reserved                        |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'
| Value                                                         |
'---------------------------------------------------------------'

T (type)  = {0,3}                  must be 4
SizeWords = {4,15}                 includes header word
NameRef   = {16,31}                string ref for the argument name
Reserved  = {32,63}                must be 0
NameEnd   = 64+(64*NameRef)
Name      = {64,NameEnd}           name of the argument, padded to 8-byte alignment
Value     = {NameEnd+1,SizeWords*64}
```

### 64-bit floating point arguments

Floats are appended after the argument name is terminated.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| 5 | SizeWords | NameRef     | Reserved                        |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'
| Value                                                         |
'---------------------------------------------------------------'

T (type)  = {0,3}                  must be 5
SizeWords = {4,15}                 includes header word
NameRef   = {16,31}                string ref for the argument name
Reserved  = {32,63}                must be 0
NameEnd   = 64+(64*NameRef)
Name      = {64,NameEnd}           name of the argument, padded to 8-byte alignment
Value     = {NameEnd+1,SizeWords*64}
```

### String arguments

Strings are encoded in UTF-8, padded with zeroes until 8-byte aligned, and appended after the
argument name.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| 6 | SizeWords | NameRef     | ValueRef      | Reserved        |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'
| Value (1+ words)                                              |
'---------------------------------------------------------------'

T (type)  = {0,3}                  must be 6
SizeWords = {4,15}                 includes header word
NameRef   = {16,31}                string ref for the argument name
ValueRef  = {32,47}                string ref for the argument value
Reserved  = {48,63}                must be 0
NameEnd   = 64+(64*NameRef)
Name      = {64,NameEnd}           name of the argument, padded to 8-byte alignment
Value     = {NameEnd+1,SizeWords*64}
```
---

### Boolean arguments

Booleans are appended after the `NameRef` field in the argument header.

```
.---------------------------------------------------------------.
|         |1|1|1|1|1|2|2|2|2|2|3|3|3|3|3|4|4|4|4|4|5|5|5|5|5|6|6|
|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|4|6|8|0|2|
|---+-----------+-----------------------------------------------|
| 9 | SizeWords | NameRef     |B| Reserved                      |
|---------------------------------------------------------------|
| Name (1+ words)                                               |
'---------------------------------------------------------------'

T (type)      = {0,3}                  must be 9
SizeWords     = {4,15}                 includes header word
NameRef       = {16,31}                string ref for the argument name
B (BoolValue) = {32}                   boolean value
Reserved      = {33,63}                must be 0
NameEnd       = 64+(64*NameRef)
Name          = {64,NameEnd}           name of the argument, padded to 8-byte alignment
```
---

# Encoding "legacy" format messages

Components that call [`LogSink.Connect`] are expected to pass a socket in "datagram" mode (as
opposed to "streaming") and to write the ["legacy" wire format] into it. This uses little endian
integers and a mix of length-prefixed and null-terminated UTF-8 strings.

At the moment, this is only used by the [Go logger] implementation.

[bitfield-diagram]: /docs/reference/platform-spec/diagnostics/bitfield-diagram.md
[Go logger]: /src/lib/syslog/go/logger.go
[`zx_clock_get_monotonic`]: /reference/syscalls/clock_get_monotonic.md
[`LogSink.Connect`]: https://fuchsia.dev/reference/fidl/fuchsia.logger#Connect
["legacy" wire format]: /zircon/system/ulib/syslog/include/lib/syslog/wire_format.h
[trace format]: /docs/reference/tracing/trace-format.md
