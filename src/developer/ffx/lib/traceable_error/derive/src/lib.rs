// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Procedural Macro Implementation for `TraceableError` (`traceable_error_derive`)
//!
//! This crate provides the procedural macro `#[derive(TraceableError)]`, which automates
//! the implementation of the `TraceableError` trait for error enums across the Fuchsia
//! codebase.
//!
//! ## Attribute Handling
//!
//! The macro actively inspects enum variants and fields for specific attributes to dictate
//! layer extraction and causal chaining:
//! - `#[source]` / `#[from]`: Identifies the underlying causal error field. If present, the macro extracts `chain_codes()` from this field and prepends the current layer code.
//! - `#[trace(opaque)]`: Can be applied to a variant or field to establish an opaque tracing boundary. When encountered, causal tracing stops at this layer, treating underlying sources as internal implementation details.
//!
//! ## Examples
//!
//! ### Base Case (No Source)
//! ```rust
//! use traceable_error::TraceableError;
//!
//! #[derive(TraceableError)]
//! pub enum SimpleError {
//!     InvalidInput, // Assigned Variant ID 0x00
//!     Timeout,      // Assigned Variant ID 0x01
//! }
//! ```
//!
//! ### Causal Chain (Transparent Tracing via `#[source]`)
//! ```rust
//! use traceable_error::TraceableError;
//! # struct FidlError;
//! # impl TraceableError for FidlError {
//! #     fn layer_code(&self) -> u32 { 0x11223300 }
//! #     fn chain_codes(&self) -> Vec<u32> { vec![0x11223300] }
//! # }
//!
//! #[derive(TraceableError)]
//! pub enum ComponentError {
//!     // The macro prepends ComponentError's layer code to FidlError's causal chain
//!     FidlFailure(#[source] FidlError),
//! }
//! ```
//!
//! ### Causal Chain (Trait Automation via `#[from]`)
//! ```rust
//! use traceable_error::TraceableError;
//! # struct DiscoveryError;
//! # impl TraceableError for DiscoveryError {
//! #     fn layer_code(&self) -> u32 { 0x99887700 }
//! #     fn chain_codes(&self) -> Vec<u32> { vec![0x99887700] }
//! # }
//!
//! #[derive(TraceableError)]
//! pub enum TargetResolveError {
//!     // Identical tracing behavior to #[source], but supports `?` conversion when paired with `thiserror`
//!     DiscoveryFailure(#[from] DiscoveryError),
//! }
//! ```
//!
//! ### Opaque Tracing Boundary
//! ```rust
//! use traceable_error::TraceableError;
//! # struct InternalDaoError;
//! # impl TraceableError for InternalDaoError {
//! #     fn layer_code(&self) -> u32 { 0x44556600 }
//! #     fn chain_codes(&self) -> Vec<u32> { vec![0x44556600] }
//! # }
//!
//! #[derive(TraceableError)]
//! pub enum ServiceError {
//!     // Tracing stops at ServiceError, hiding InternalDaoError from distributed diagnostics
//!     #[trace(opaque)]
//!     StorageCorrupted(#[source] InternalDaoError),
//! }
//! ```

#![allow(unused_crate_dependencies)]

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, parse_macro_input};

/// Derives the `TraceableError` trait for an enum.
///
/// This procedural macro automatically calculates a stable 32-bit layer identifier for each variant
/// by shifting the 24-bit FNV-1a crate hash and combining it with an 8-bit variant index.
/// It generates `layer_code()` and `chain_codes()` match arms based on `#[source]`, `#[from]`,
/// and `#[trace(opaque)]` attributes.
///
/// ### Compilation-Time Constraints & Safety Panics
///
/// - **Enum Only**: This macro is strictly restricted to `enum` declarations. Attempting to derive
///   `TraceableError` on a `struct` or `union` will result in a compilation-time panic.
/// - **Variant Count**: Since the variant ID field occupies exactly 8 bits (low bits) of the 32-bit
///   layer code, the enum is capped at a maximum of **256 variants** (0x00 to 0xFF). Enums with
///   257 or more variants will trigger a compilation failure.
///
/// # Examples
///
/// ```rust
/// use traceable_error::TraceableError;
///
/// #[derive(TraceableError)]
/// pub enum NetworkError {
///     Disconnected,
///     HandshakeFailed(#[source] anyhow::Error),
///     SocketFailure(#[from] std::io::Error),
///     #[trace(opaque)]
///     InternalConfigCorrupted(#[source] anyhow::Error),
/// }
/// ```
#[proc_macro_derive(TraceableError, attributes(trace, source, from))]
pub fn derive_traceable_error(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let Data::Enum(data_enum) = &input.data else {
        panic!("TraceableError can only be derived on enums");
    };

    let mut layer_match_arms = Vec::new();
    let mut chain_match_arms = Vec::new();

    for (variant_idx, variant) in data_enum.variants.iter().enumerate() {
        assert!(variant_idx < 256, "TraceableError supports a maximum of 256 variants per crate");
        let variant_ident = &variant.ident;

        // 1. Find the chained field (if any)
        let chained_field_idx = variant.fields.iter().position(|f| {
            f.attrs
                .iter()
                .any(|attr| attr.path().is_ident("from") || attr.path().is_ident("source"))
        });

        let chained_field = chained_field_idx.map(|idx| variant.fields.iter().nth(idx).unwrap());

        // 2. Check for the #[trace(opaque)] boundary
        let is_opaque =
            variant.attrs.iter().chain(chained_field.map(|f| &f.attrs).into_iter().flatten()).any(
                |attr| {
                    if attr.path().is_ident("trace") {
                        let meta: Result<syn::Meta, _> = attr.parse_args();
                        if let Ok(syn::Meta::Path(path)) = meta {
                            return path.is_ident("opaque");
                        }
                    }
                    false
                },
            );

        let is_chained = chained_field.is_some() && !is_opaque;

        // 3. Generate the correct destructuring pattern to extract `inner`
        let destructure = match &variant.fields {
            syn::Fields::Named(_) => {
                if let Some(field) = chained_field {
                    let field_name = &field.ident;
                    quote! { { #field_name: inner, .. } }
                } else {
                    quote! { { .. } }
                }
            }
            syn::Fields::Unnamed(fields) => {
                if let Some(idx) = chained_field_idx {
                    let pats = (0..fields.unnamed.len()).map(|i| {
                        if i == idx {
                            quote! { inner }
                        } else {
                            quote! { _ }
                        }
                    });
                    quote! { ( #(#pats),* ) }
                } else {
                    quote! { ( .. ) }
                }
            }
            syn::Fields::Unit => quote! {},
        };

        // --- Generate layer_code() arm ---
        let name_str = name.to_string();
        let variant_str = variant_ident.to_string();
        layer_match_arms.push(quote! {
            Self::#variant_ident #destructure => {
                const CRATE_NAME: &str = match option_env!("CARGO_PKG_NAME") {
                    Some(name) => name,
                    None => "unknown",
                };
                format!("{}::{}::{}", CRATE_NAME, #name_str, #variant_str)
            }
        });

        // --- Generate chain_codes() arm ---
        if is_chained {
            chain_match_arms.push(quote! {
                Self::#variant_ident #destructure => {
                    let mut trace = inner.chain_codes();
                    trace.insert(0, self.layer_code());
                    trace
                }
            });
        } else {
            // Base case or Opaque boundary: Trace stops here
            chain_match_arms.push(quote! {
                Self::#variant_ident #destructure => vec![self.layer_code()],
            });
        }
    }

    let expanded = quote! {
        impl ::traceable_error::TraceableError for #name {
            fn layer_code(&self) -> String {
                match self {
                    #(#layer_match_arms)*
                }
            }

            fn chain_codes(&self) -> Vec<String> {
                match self {
                    #(#chain_match_arms)*
                }
            }
        }
    };

    TokenStream::from(expanded)
}
