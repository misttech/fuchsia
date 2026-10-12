// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DataEnum, DataStruct, DeriveInput, Fields, Generics, Ident, parse_macro_input};

#[proc_macro_derive(TypeFingerprint, attributes(serde))]
pub fn derive_type_fingerprint(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let result = match &input.data {
        Data::Struct(data) => handle_struct(&input.ident, &input.generics, data),
        Data::Enum(data) => handle_enum(&input.ident, &input.generics, data),
        Data::Union(_) => Err(syn::Error::new_spanned(&input, "unions are not supported")),
    };
    match result {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Parses `#[serde(...)]` attributes, returning `(skip, with_module_path)`.
fn parse_serde_attributes(attrs: &[syn::Attribute]) -> syn::Result<(bool, Option<syn::Path>)> {
    let mut skip = false;
    let mut module_path = None;
    for attr in attrs {
        if attr.path().is_ident("serde") {
            let res = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("skip") {
                    skip = true;
                } else if meta.path.is_ident("with") {
                    let value = meta.value()?;
                    let s: syn::LitStr = value.parse()?;
                    let path = s.parse::<syn::Path>()?;
                    module_path = Some(path);
                } else {
                    // Fully consume other nested serde attributes from the input.
                    if meta.input.peek(syn::Token![=]) {
                        let _value = meta.value()?;
                        let _: syn::Expr = _value.parse()?;
                    } else if meta.input.peek(syn::token::Paren) {
                        let content;
                        syn::parenthesized!(content in meta.input);
                        let _: TokenStream = content.parse()?;
                    }
                }
                Ok(())
            });
            res?;
        }
    }
    Ok((skip, module_path))
}

/// Returns the fingerprint expression for a field, delegating to its `serde(with)` module if
/// annotated, or `None` if `#[serde(skip)]` is set.
fn get_fingerprint_expr(field: &syn::Field) -> syn::Result<Option<TokenStream>> {
    let field_type = &field.ty;
    let (skip, module_path) = parse_serde_attributes(&field.attrs)?;
    if skip {
        Ok(None)
    } else if let Some(module_path) = module_path {
        Ok(Some(quote! { #module_path::fingerprint::<#field_type>() }))
    } else {
        Ok(Some(quote! { <#field_type as TypeFingerprint>::fingerprint() }))
    }
}

fn handle_fields(fields: &syn::Fields) -> syn::Result<Vec<TokenStream>> {
    match &fields {
        Fields::Unit => Ok(vec![]),
        Fields::Named(fields) => fields
            .named
            .iter()
            .filter_map(|field| match get_fingerprint_expr(field) {
                Ok(Some(fingerprint_expr)) => {
                    if let Some(field_name) = &field.ident {
                        let field_name = field_name.to_string();
                        Some(Ok(quote! { #field_name + ":" + &(#fingerprint_expr) }))
                    } else {
                        Some(Ok(quote! { &(#fingerprint_expr) }))
                    }
                }
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
            .collect(),
        Fields::Unnamed(fields) => fields
            .unnamed
            .iter()
            .filter_map(|field| match get_fingerprint_expr(field) {
                Ok(Some(fingerprint_expr)) => Some(Ok(quote! { &(#fingerprint_expr) })),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
            .collect(),
    }
}

fn handle_struct(
    ident: &Ident,
    _generics: &Generics,
    data: &DataStruct,
) -> syn::Result<TokenStream> {
    let fields = handle_fields(&data.fields)?;
    let mut out = quote! { "" };
    for (i, field) in fields.into_iter().enumerate() {
        if i != 0 {
            out = quote! { #out + "," }
        }
        out = quote! { #out + #field }
    }
    Ok(quote! {
        impl TypeFingerprint for #ident {
            fn fingerprint() -> String {
                "struct {".to_owned() + #out + "}"
            }
        }
    })
}

fn handle_enum(ident: &Ident, _generics: &Generics, data: &DataEnum) -> syn::Result<TokenStream> {
    let mut variants = data
        .variants
        .iter()
        .map(|variant| {
            let (skip, _) = parse_serde_attributes(&variant.attrs)?;
            if skip {
                return Ok(None);
            }
            let name = variant.ident.to_string();
            let fields = handle_fields(&variant.fields)?;
            if fields.is_empty() {
                Ok(Some(quote! { #name }))
            } else {
                let mut out = quote! { #name + "(" };
                for (i, field) in fields.into_iter().enumerate() {
                    if i != 0 {
                        out = quote! { #out + "," }
                    }
                    out = quote! { #out + #field }
                }
                Ok(Some(quote! { #out + ")" }))
            }
        })
        .collect::<syn::Result<Vec<_>>>()?;

    // Trailing `#[serde(skip)]` variants do not affect the serialized variant indices of earlier
    // variants, so they can be omitted from the fingerprint. Non-trailing `#[serde(skip)]`
    // variants still occupy a variant index in serde, so they must remain in the fingerprint.
    while matches!(variants.last(), Some(None)) {
        variants.pop();
    }

    let mut out = quote! { "" };
    for (i, variant) in variants.into_iter().enumerate() {
        if i != 0 {
            out = quote! { #out + "," }
        }
        let variant = variant.unwrap_or_else(|| quote! { "<skipped>" });
        out = quote! { #out + #variant }
    }
    Ok(quote! {
        impl TypeFingerprint for #ident {
            fn fingerprint() -> String {
                    "enum {".to_owned() + #out + "}"
            }
        }
    })
}
