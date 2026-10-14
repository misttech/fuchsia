# election

`election` provides `Election<K, V>`, which arbitrates a single setting across
multiple voters and elects the winning value (the result).

It is FIDL-agnostic and shared by components that combine requests from several
clients into one hardware setting, such as charger drivers (arbitrating
`Controller`, `Debug`, and `Vreg` votes) and `battery-manager` (arbitrating
votes from charge governors).

Linux charger drivers call the same object a votable (Qualcomm `pmic-voter`) or
an election (Google `gvotable`).

## Concepts

- **Voter (`K`)**: identifies who cast a vote, such as an enum of client types
  or a governor's name (`String`). Each voter has at most one vote.
- **Vote (`V`)**: the value a voter requests. A voter casts a vote with `cast`
  and withdraws it with `withdraw` (`set_vote` takes `Some` or `None`).
- **Result**: the winning vote across all active voters, or `None` if no votes
  are active. Callers apply their own default when it is `None`.
- **Comparator**: ranks active votes; the vote ranked `Ordering::Greater` wins.
  - `Election::min()`: the lowest vote wins (for example, current or voltage
    ceilings, where every voter's limit must be honored).
  - `Election::max()`: the highest vote wins.
  - `Election::new(cmp)`: a custom precedence, for value types without `Ord`
    (such as FIDL enums).
- **Override voter** (optional): `with_override_voter(k)` designates a voter
  whose active vote wins over all other voters regardless of the comparator (for
  example, a debug override).

## Usage

```rust
use election::Election;

let fcc_ua = Election::<&str, u32>::min().with_override_voter("debug");

fcc_ua.cast("msc", 2_000_000);
fcc_ua.cast("thermal", 1_000_000);
assert_eq!(fcc_ua.result(), Some(1_000_000));

// The override voter wins even under `min()`.
fcc_ua.cast("debug", 3_000_000);
assert_eq!(fcc_ua.result(), Some(3_000_000));

// Withdrawing a vote restores the election across the remaining voters.
fcc_ua.withdraw("debug");
assert_eq!(fcc_ua.result(), Some(1_000_000));
```

### Applying single-field changes

`try_cast` and `try_withdraw` (or `try_set_vote` with `Some` or `None`) update a
voter's vote and, only if the result changes, run an async `apply` closure (for
example, a register write). If `apply` fails, or the returned future is dropped
before `apply` completes, the voter's previous vote is restored so that the
election matches what was last applied and the request can be retried:

```rust
let changed = fcc_ua
    .try_cast("msc", 1_500_000, |result| async move {
        program_fcc(result.unwrap_or(DEFAULT_FCC_UA)).await
    })
    .await?;
```

They return `Ok(true)` if the result changed and was applied, or `Ok(false)` if
the result was unchanged (and `apply` did not run). A failed or cancelled
`apply` may still have partially taken effect.

### Multi-field transactions

When one request updates several `Election`s that must be applied together (for
example, a single driver call carrying every changed field), use the
lower-level APIs and roll back on failure:

1.  Snapshot each voter's previous votes with `vote` (or keep the value returned
    by `set_vote`).
2.  Cast or withdraw the new votes with `set_vote`.
3.  Read each `result` and apply them together.
4.  On failure, restore the previous votes with `set_vote`.

`Election` holds its internal lock only for short map operations, never across
`.await` points, so callers must serialize multi-field transactions themselves
(for example, behind an async mutex).

### Inspect

`record_inspect(parent, name)` records a node for the election:

```
<name>:
  result: <winning vote, omitted when no votes are active>
  overridden: <whether the override voter has an active vote>
  votes:
    <voter>: <vote>
```

Values implement `InspectValue` (provided for `u32`, `u64`, `i64`, and `bool`).
For value types defined in other crates, which cannot implement `InspectValue`,
use `record_inspect_with` and pass a closure that records each value:

```rust
mode.record_inspect_with(node, "operating_mode", |mode, node, key| {
    node.record_string(key, format!("{mode:?}"));
});
```

Voter keys must implement `AsRef<str>` to be recorded.

## Testing

```
fx test election-tests
```
