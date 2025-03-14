# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

__all__ = ["IDENTIFIERS"]

from common import Deny, Identifier

# These are keywords and identifiers used in languages we support and in
# generated bindings. This list is maintained by hand and should be expanded
# to cover as many cases as we can think of.
#
# Each identifier has lower_camel_case name, a tag (used to maintain source
# stability when adding identifiers) and optionally a deny list.
#
# The deny list members specify a list of styles, uses and bindings to deny.
# The bindings list is used to decide whether to include certain identifiers
# in the generated fidl files. If a bindings list is included then a
# @bindings_denylist attribute will be used, if not then the identifier will
# be entirely omitted from the affected contexts.

# Deny rule to apply to Rust keywords, see https://fxbug.dev/42138375
RUST_KEYWORD = [
    Deny(
        bindings=["rust"],
        uses=[
            "method.names",
            "method.event.arguments",
            "method.request.arguments",
            "method.response.arguments",
            "service.member.names",
            "table.fields",
            "struct.names",
        ],
    )
]

# Deny rule to apply to FIDL primitives.
FIDL_PRIMITIVE = [
    Deny(
        styles=["lower"],
        uses=[
            "service.member.types",  # FIDL compiler disallows primitives here.
        ],
    )
]

IDENTIFIERS = [
    Identifier("abstract", RUST_KEYWORD),
    Identifier("alignas"),
    Identifier("alignof"),
    Identifier("and"),
    Identifier("and_eq"),
    Identifier("as", RUST_KEYWORD),
    Identifier("asm"),
    Identifier("assert"),
    Identifier("async", RUST_KEYWORD),
    Identifier("atomic_cancel"),
    Identifier("atomic_commit"),
    Identifier("atomic_noexcept"),
    Identifier("auto"),
    Identifier("await", RUST_KEYWORD),
    Identifier("become", RUST_KEYWORD),
    Identifier("bitand"),
    Identifier("bitor"),
    Identifier("bool", FIDL_PRIMITIVE),
    # TODO(https://fxbug.dev/42157590): this can be re-enabled once builtin shadowing works.
    # Identifier('box', RUST_KEYWORD),
    Identifier("break", RUST_KEYWORD),
    Identifier("byte", FIDL_PRIMITIVE),
    Identifier("bytes", FIDL_PRIMITIVE),
    Identifier("case"),
    Identifier("catch"),
    Identifier("chan"),
    Identifier("char"),
    Identifier("char16_t"),
    Identifier("char32_t"),
    Identifier("class"),
    Identifier("clone"),
    Identifier("co_await"),
    Identifier("co_return"),
    Identifier("co_yield"),
    Identifier("coding_traits"),
    Identifier("compl"),
    Identifier("concept"),
    Identifier("const", RUST_KEYWORD),
    Identifier("const_cast"),
    Identifier("constexpr"),
    Identifier("continue", RUST_KEYWORD),
    # TODO(https://fxbug.dev/42145610): Fix in Rust.
    Identifier("control_handle", [Deny(bindings=["rust"])]),
    Identifier("controller"),
    Identifier("covariant"),
    Identifier("crate", RUST_KEYWORD),
    Identifier("decltype"),
    Identifier("decodable"),
    Identifier(
        "decoder"
    ),  # TODO(https://fxbug.dev/42161195) [Deny(uses=['union.names'], styles=['lower'])]),
    Identifier("default"),
    Identifier("defer"),
    Identifier("deferred"),
    Identifier("delete"),
    Identifier("do", RUST_KEYWORD),
    Identifier("double"),
    Identifier("dynamic"),
    Identifier("dynamic_cast"),
    Identifier("else", RUST_KEYWORD),
    Identifier("encodable"),
    Identifier("encoder"),
    Identifier("ensure_values_instantiated"),
    Identifier("enum", RUST_KEYWORD),
    Identifier("empty"),
    Identifier("err"),
    Identifier("explicit"),
    Identifier("export"),
    Identifier("extends"),
    Identifier("extern", RUST_KEYWORD),
    Identifier("external"),
    Identifier("factory"),
    Identifier("fallthrough"),
    # TODO(https://fxbug.dev/42157590)
    # Identifier('false', RUST_KEYWORD),
    Identifier("fidl"),
    Identifier("fidl_type"),
    Identifier("final", RUST_KEYWORD),
    Identifier("finally"),
    Identifier("float"),
    Identifier("fn", RUST_KEYWORD),
    Identifier("for", RUST_KEYWORD),
    Identifier("frame"),
    Identifier("friend"),
    Identifier("func"),
    Identifier("future"),
    Identifier("futures"),
    Identifier("get"),
    Identifier("go"),
    Identifier("goto"),
    Identifier("handles"),
    Identifier("has_invalid_tag"),
    Identifier("hash_code"),
    Identifier("header"),
    Identifier("if", RUST_KEYWORD),
    Identifier("impl", RUST_KEYWORD),
    Identifier("implements"),
    Identifier("import"),
    Identifier("in", RUST_KEYWORD),
    Identifier("index"),
    Identifier("inline"),
    Identifier("int"),
    Identifier("int16", FIDL_PRIMITIVE),
    Identifier("int32", FIDL_PRIMITIVE),
    Identifier("int64", FIDL_PRIMITIVE),
    Identifier("int8", FIDL_PRIMITIVE),
    Identifier("interface"),
    Identifier("internal_tag"),
    Identifier("is"),
    Identifier("let", RUST_KEYWORD),
    Identifier("lhs"),
    Identifier("library"),
    Identifier("list"),
    Identifier("long"),
    Identifier("loop", RUST_KEYWORD),
    Identifier("macro", RUST_KEYWORD),
    Identifier("map"),
    Identifier("match", RUST_KEYWORD),
    Identifier("mixin"),
    Identifier("mod", RUST_KEYWORD),
    Identifier("module"),
    Identifier("move", RUST_KEYWORD),
    Identifier("mut", RUST_KEYWORD),
    Identifier("mutable"),
    Identifier("namespace"),
    Identifier("never"),
    Identifier("new", [Deny(bindings=["rust"], uses=["method.names"])]),
    Identifier("no_such_method"),
    Identifier("noexcept"),
    Identifier("none"),
    Identifier("not"),
    Identifier("not_eq"),
    Identifier("null"),
    Identifier("nullptr"),
    Identifier("num"),
    Identifier("object"),
    Identifier("offset", [Deny(bindings=["rust"])]),
    Identifier("offsetof"),
    Identifier("ok"),
    Identifier("on_open"),
    Identifier("operator"),
    Identifier("option"),
    Identifier("or"),
    Identifier("or_eq"),
    Identifier("override", RUST_KEYWORD),
    Identifier("package"),
    Identifier("part"),
    Identifier("priv", RUST_KEYWORD),
    Identifier("private"),
    Identifier("proc"),
    Identifier("protected"),
    Identifier("proxy"),
    Identifier("pub", RUST_KEYWORD),
    Identifier("public"),
    Identifier("pure"),
    Identifier("range"),
    Identifier("ref", RUST_KEYWORD),
    Identifier("register"),
    Identifier("reinterpret_cast"),
    Identifier("requires"),
    Identifier("result"),
    # TODO(https://fxbug.dev/42145610): Fix in Rust.
    Identifier("responder", [Deny(bindings=["rust"])]),
    Identifier("rethrow"),
    Identifier("return", RUST_KEYWORD),
    Identifier("rhs"),
    Identifier("rune"),
    Identifier("runtime_type"),
    Identifier("select"),
    Identifier(
        "self",
        RUST_KEYWORD
        + [
            Deny(
                bindings=["rust"],
                styles=["upper"],
                uses=["event.names", "enums"],
            )
        ],
    ),
    Identifier("send"),
    Identifier("set"),
    Identifier("set_controller"),
    Identifier("short"),
    Identifier("signed"),
    Identifier("sizeof"),
    Identifier("some"),
    Identifier("static", RUST_KEYWORD),
    Identifier("static_assert"),
    Identifier("static_cast"),
    Identifier("stream"),
    Identifier(
        "string",
        FIDL_PRIMITIVE
        + [
            # TODO(https://fxbug.dev/42145610): Need to escape "String" in Rust.
            Deny(
                bindings=["rust"],
                styles=["upper", "camel"],
                uses=["using"],
            )
        ],
    ),
    Identifier("struct", RUST_KEYWORD),
    Identifier("stub"),
    Identifier("stderr"),
    Identifier("stdin"),
    Identifier("stdout"),
    Identifier("super", RUST_KEYWORD),
    Identifier("switch"),
    Identifier("synchronized"),
    Identifier("template"),
    Identifier("this"),
    Identifier("thread_local"),
    Identifier("throw"),
    Identifier("to_string"),
    Identifier("trait", RUST_KEYWORD),
    # TODO(https://fxbug.dev/42157590)
    # Identifier('true', RUST_KEYWORD),
    Identifier("try", RUST_KEYWORD),
    Identifier("type", RUST_KEYWORD),
    Identifier("typedef"),
    Identifier("typeid"),
    Identifier("typename"),
    Identifier("typeof", RUST_KEYWORD),
    Identifier("uint16", FIDL_PRIMITIVE),
    # We use uint32 as a type in some tests which makes it conflict.
    # See also: https://fxbug.dev/42113840 https://fxbug.dev/42160762)
    Identifier(
        "uint32",
        [
            Deny(
                styles=["lower"],
                uses=[
                    "constants",
                    "service.member.types",  # FIDL compiler disallows primitives here.
                    "struct.types",
                    "table.names",  # TODO(https://fxbug.dev/42161195) 'union.names'
                ],
            )
        ],
    ),
    Identifier("uint64", FIDL_PRIMITIVE),
    Identifier("uint8", FIDL_PRIMITIVE),
    Identifier("union"),
    Identifier("unknown"),
    Identifier("unknown_bytes"),
    # TODO(https://fxbug.dev/42138681):  Remedy identifier clashes.
    Identifier("unknown_data", [Deny(bindings=["dart", "rust"])]),
    Identifier("unsafe", RUST_KEYWORD),
    Identifier("unsigned"),
    Identifier("unsized", RUST_KEYWORD),
    Identifier("use", RUST_KEYWORD),
    Identifier("using"),
    Identifier("value"),
    Identifier("value_of"),
    Identifier("value_union"),
    Identifier("values_map"),
    Identifier("var"),
    Identifier("vec"),
    Identifier("virtual", RUST_KEYWORD),
    Identifier("void"),
    Identifier("volatile"),
    Identifier("wchar_t"),
    Identifier("where", RUST_KEYWORD),
    Identifier("which"),
    Identifier("while", RUST_KEYWORD),
    Identifier("with"),
    Identifier("xor"),
    Identifier("xor_eq"),
    Identifier("xunion"),
    Identifier("yield", RUST_KEYWORD),
    Identifier("zx"),
]
