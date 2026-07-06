#[allow(clippy::unreadable_literal)]
static PROPERTY_KEYS: phf::Set<&'static str> = ::phf::Set { map: ::phf::Map {
    key: 15467950696543387533,
    disps: &[
        (1, 0),
    ],
    entries: &[
        ("patternProperties", ()),
        ("properties", ()),
    ],
} };
#[allow(clippy::unreadable_literal)]
static NON_SCHEMA_KEYS: phf::Set<&'static str> = ::phf::Set { map: ::phf::Map {
    key: 12913932095322966823,
    disps: &[
        (0, 4),
        (7, 0),
        (2, 9),
    ],
    entries: &[
        ("dependencies", ()),
        ("dependentSchemas", ()),
        ("properties", ()),
        ("oneOf", ()),
        ("anyOf", ()),
        ("enum", ()),
        ("dependentRequired", ()),
        ("$defs", ()),
        ("allOf", ()),
        ("definitions", ()),
        ("patternProperties", ()),
        ("const", ()),
    ],
} };#[allow(clippy::unreadable_literal)]
static BOOLEAN_SCHEMA_ARRAY_KEYS: phf::Set<&'static str> = ::phf::Set { map: ::phf::Map {
    key: 15467950696543387533,
    disps: &[
        (3, 0),
    ],
    entries: &[
        ("anyOf", ()),
        ("allOf", ()),
        ("items", ()),
        ("oneOf", ()),
    ],
} };#[allow(clippy::unreadable_literal)]
static FINAL_KEYS: phf::Set<&'static str> = ::phf::Set { map: ::phf::Map {
    key: 10121458955350035957,
    disps: &[
        (1, 0),
    ],
    entries: &[
        ("required", ()),
        ("default", ()),
        ("enum", ()),
        ("type", ()),
    ],
} };#[allow(clippy::unreadable_literal)]
const ALLOW_NON_CONSUMED_KEYS: phf::Set<&'static str> = ::phf::Set { map: ::phf::Map {
    key: 10121458955350035957,
    disps: &[
        (1, 0),
        (9, 5),
    ],
    entries: &[
        ("$id", ()),
        ("definitions", ()),
        ("title", ()),
        ("$anchor", ()),
        ("default", ()),
        ("examples", ()),
        ("format", ()),
        ("$schema", ()),
        ("$defs", ()),
        ("description", ()),
    ],
} };