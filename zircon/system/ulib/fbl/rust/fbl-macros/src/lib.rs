// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use proc_macro::TokenStream;
use quote::quote;
use syn::{Fields, ItemStruct, parse_macro_input};

/// Attribute macro to make a struct reference counted.
///
/// Adds a `ref_count: fbl::RefCounted` field and a `_guard: RefCountedGuard` as the first and
/// second fields. Implements `fbl::HasRefCount` and `fbl::Recyclable` for the struct.
#[proc_macro_attribute]
pub fn ref_counted(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as ItemStruct);
    let name = &input.ident;

    // Check for #[repr(C)]
    let has_repr_c = input.attrs.iter().any(|attr| {
        if !attr.path().is_ident("repr") {
            return false;
        }
        if let Ok(ident) = attr.parse_args::<syn::Ident>() {
            return ident == "C";
        }
        if let Ok(list) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) {
            return list.iter().any(|meta| {
                if let syn::Meta::Path(path) = meta {
                    return path.is_ident("C");
                }
                false
            });
        }
        false
    });

    if !has_repr_c {
        panic!("Structs using #[ref_counted] must also be marked with #[repr(C)]");
    }

    if let Fields::Named(ref mut fields) = input.fields {
        // Add ref_count field as the first field.
        fields.named.insert(
            0,
            syn::parse_quote! {
                ref_count: ::fbl::RefCounted
            },
        );
        // Add _guard field to prevent manual allocation.
        fields.named.push(syn::parse_quote! {
            __fbl_ref_counted_guard: ()
        });
    } else {
        panic!("ref_counted attribute only supports structs with named fields");
    }

    let expanded = quote! {
        #input

        impl ::fbl::HasRefCount for #name {
            fn ref_count(&self) -> &::fbl::RefCounted {
                &self.ref_count
            }
        }

        impl ::fbl::Recyclable for #name {
            unsafe fn recycle(ptr: ::core::ptr::NonNull<Self>) {
                // SAFETY: ptr was allocated by `try_make_ref_counted`, which uses `Box::try_new`.
                unsafe {
                    let _ = ::kalloc::Box::from_non_null(ptr);
                }
            }
        }

        // Compile-time validation that ref_count is at offset 0
        ::zr::static_assert!(core::mem::offset_of!(#name, ref_count) == 0);
    };

    TokenStream::from(expanded)
}
