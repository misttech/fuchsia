// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derives `Parse` for a struct, parsing its fields sequentially.
///
/// Every field of the struct must also implement `Parse`.
///
/// # Example
///
/// ```rust
/// #[derive(Parse)]
/// struct Header {
///     magic: u32,
///     version: u32,
/// }
/// ```
#[proc_macro_derive(Parse)]
pub fn derive_parse(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let parse_impl = match input.data {
        // Matches structs. Unions and enums are not supported.
        Data::Struct(data) => match data.fields {
            // Matches structs with named fields, e.g. `struct Foo { id: u32 }`.
            Fields::Named(fields) => {
                let recurse = fields.named.iter().map(|f| {
                    let name = &f.ident;
                    quote! {
                        #name: crate::new_policy::traits::Parse::parse(cursor)?,
                    }
                });
                quote! {
                    Self {
                        #(#recurse)*
                    }
                }
            }
            // Matches tuple structs with unnamed fields, e.g. `struct Foo(u32)`.
            Fields::Unnamed(fields) => {
                let recurse = fields.unnamed.iter().map(|_| {
                    quote! {
                        crate::new_policy::traits::Parse::parse(cursor)?,
                    }
                });
                quote! {
                    Self (
                        #(#recurse)*
                    )
                }
            }
            // Matches unit structs, e.g. `struct Foo;`.
            Fields::Unit => {
                quote! { Self }
            }
        },
        _ => {
            return syn::Error::new_spanned(&name, "Only structs are supported by Parse derive")
                .to_compile_error()
                .into();
        }
    };

    let expanded = quote! {
        impl #impl_generics crate::new_policy::traits::Parse for #name #ty_generics #where_clause {
            fn parse(cursor: &mut crate::new_policy::parser::PolicyCursor<'_>) -> Result<Self, crate::new_policy::error::ParseError> {
                Ok(#parse_impl)
            }
        }
    };

    expanded.into()
}

/// Derives `Serialize` for a struct, serializing its fields sequentially.
///
/// Every field of the struct must also implement `Serialize`.
///
/// # Example
///
/// ```rust
/// #[derive(Serialize)]
/// struct Header {
///     magic: u32,
///     version: u32,
/// }
/// ```
#[proc_macro_derive(Serialize)]
pub fn derive_serialize(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let serialize_impl = match input.data {
        // Matches structs. Unions and enums are not supported.
        Data::Struct(data) => match data.fields {
            // Matches structs with named fields, e.g. `struct Foo { id: u32 }`.
            Fields::Named(fields) => {
                let recurse = fields.named.iter().map(|f| {
                    let name = &f.ident;
                    quote! {
                        crate::new_policy::traits::Serialize::serialize(&self.#name, writer)?;
                    }
                });
                quote! {
                    #(#recurse)*
                }
            }
            // Matches tuple structs with unnamed fields, e.g. `struct Foo(u32)`.
            Fields::Unnamed(fields) => {
                let recurse = fields.unnamed.iter().enumerate().map(|(i, _)| {
                    let index = syn::Index::from(i);
                    quote! {
                        crate::new_policy::traits::Serialize::serialize(&self.#index, writer)?;
                    }
                });
                quote! {
                    #(#recurse)*
                }
            }
            // Matches unit structs, e.g. `struct Foo;`.
            Fields::Unit => {
                quote! {}
            }
        },
        _ => {
            return syn::Error::new_spanned(
                &name,
                "Only structs are supported by Serialize derive",
            )
            .to_compile_error()
            .into();
        }
    };

    let expanded = quote! {
        impl #impl_generics crate::new_policy::traits::Serialize for #name #ty_generics #where_clause {
            fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), crate::new_policy::error::SerializeError> {
                #serialize_impl
                Ok(())
            }
        }
    };

    expanded.into()
}

/// Derives `Validate` for a struct, validating its fields sequentially.
///
/// Every field of the struct must also implement `Validate`.
///
/// # Example
///
/// ```rust
/// #[derive(Validate)]
/// struct Header {
///     magic: u32,
///     version: u32,
/// }
/// ```
#[proc_macro_derive(Validate)]
pub fn derive_validate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let validate_impl = match input.data {
        // Matches structs. Unions and enums are not supported.
        Data::Struct(data) => match data.fields {
            // Matches structs with named fields, e.g. `struct Foo { id: u32 }`.
            Fields::Named(fields) => {
                let recurse = fields.named.iter().map(|f| {
                    let name = &f.ident;
                    quote! {
                        crate::new_policy::traits::Validate::validate(&self.#name, policy)?;
                    }
                });
                quote! {
                    #(#recurse)*
                }
            }
            // Matches tuple structs with unnamed fields, e.g. `struct Foo(u32)`.
            Fields::Unnamed(fields) => {
                let recurse = fields.unnamed.iter().enumerate().map(|(i, _)| {
                    let index = syn::Index::from(i);
                    quote! {
                        crate::new_policy::traits::Validate::validate(&self.#index, policy)?;
                    }
                });
                quote! {
                    #(#recurse)*
                }
            }
            // Matches unit structs, e.g. `struct Foo;`.
            Fields::Unit => {
                quote! {}
            }
        },
        _ => {
            return syn::Error::new_spanned(&name, "Only structs are supported by Validate derive")
                .to_compile_error()
                .into();
        }
    };

    let expanded = quote! {
        impl #impl_generics crate::new_policy::traits::Validate for #name #ty_generics #where_clause {
            fn validate(&self, policy: &crate::new_policy::NewPolicy) -> Result<(), crate::new_policy::error::ValidateError> {
                #validate_impl
                Ok(())
            }
        }
    };

    expanded.into()
}
