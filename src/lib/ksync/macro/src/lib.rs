// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Fields, GenericParam, Ident, ItemStruct, Type, TypePath, Visibility, parse_macro_input};

struct MutexField {
    ident: Ident,
    class_ident: Ident,
    mutex_type: proc_macro2::TokenStream,
}

struct GuardedField {
    ident: Ident,
    vis: Visibility,
    ty: Type,
    mutex_ident: Ident,
}

#[proc_macro_attribute]
pub fn guarded(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut input_struct = parse_macro_input!(input as ItemStruct);

    let mut mutex_fields = Vec::new();
    let mut guarded_fields = Vec::new();
    let mut errors = Vec::new();

    if let Fields::Named(ref mut fields) = input_struct.fields {
        for field in fields.named.iter_mut() {
            let mut is_mutex = false;
            let mut guarded_by = None;
            let mut attrs_to_remove = Vec::new();
            let mut mutex_type = quote! { ::ksync::RawMutex };

            for (idx, attr) in field.attrs.iter().enumerate() {
                if attr.path().is_ident("mutex") {
                    is_mutex = true;
                    attrs_to_remove.push(idx);

                    if !matches!(attr.meta, syn::Meta::Path(_)) {
                        errors.push(syn::Error::new(
                            attr.meta.span(),
                            "#[mutex] attribute does not accept arguments. Use KMutex<LockType> to specify the lock type.",
                        ));
                    }
                } else if attr.path().is_ident("guarded_by") {
                    if let syn::Meta::List(meta_list) = &attr.meta {
                        if let Ok(ident) = meta_list.parse_args::<Ident>() {
                            guarded_by = Some(ident);
                        }
                    }
                    attrs_to_remove.push(idx);
                }
            }

            // Remove our helper attributes in reverse order.
            for idx in attrs_to_remove.into_iter().rev() {
                field.attrs.remove(idx);
            }

            if is_mutex {
                if !is_kmutex_type(&field.ty) {
                    errors.push(syn::Error::new(
                        field.ty.span(),
                        "Mutex field must be of type KMutex",
                    ));
                }
                match extract_lock_type(&field.ty) {
                    Ok(Some(ty)) => {
                        mutex_type = ty;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        errors.push(e);
                    }
                }
                // Automatically prepend the #[pin] attribute for pin-init layout
                field.attrs.push(syn::parse_quote!(#[pin]));
                mutex_fields.push((field.clone(), mutex_type));
            } else if let Some(mutex_ident) = guarded_by {
                guarded_fields.push(GuardedField {
                    ident: field.ident.clone().unwrap(),
                    vis: field.vis.clone(),
                    ty: field.ty.clone(),
                    mutex_ident,
                });
            }
        }
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* }.into();
    }

    // Automatically apply the #[::pin_init::pin_data] attribute to the parent struct
    // only if it doesn't already have one (e.g. #[pin_data(PinnedDrop)])
    let has_pin_data = input_struct
        .attrs
        .iter()
        .any(|attr| attr.path().segments.iter().any(|seg| seg.ident == "pin_data"));
    if !has_pin_data {
        input_struct.attrs.push(syn::parse_quote!(#[::pin_init::pin_data]));
    }

    let struct_ident = &input_struct.ident;
    let struct_vis = &input_struct.vis;

    let mut mutex_fields_processed = Vec::new();
    let mut generated_names = std::collections::HashSet::new();

    for (field, mutex_type) in mutex_fields {
        let field_ident = field.ident.clone().unwrap();
        let mu_camel = to_camel_case(&field_ident.to_string());

        // Static check for Guard name collision within the same struct
        let guard_name = format!("{}{}Guard", struct_ident, mu_camel);
        if !generated_names.insert(guard_name.clone()) {
            errors.push(syn::Error::new(
                field_ident.span(),
                format!(
                    "The mutex field '{}' generates the duplicate Guard name '{}'. Please use distinct field names.",
                    field_ident, guard_name
                )
            ));
        }

        let mut class_ident =
            format_ident!("{}{}", struct_ident, format_ident!("{}Class", mu_camel));
        // Span Hygiene: Set the ZST class name span to the specific mutex field span
        class_ident.set_span(field_ident.span());

        mutex_fields_processed.push(MutexField { ident: field_ident, class_ident, mutex_type });
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* }.into();
    }

    // Rewrite fields in the struct to be of type KMutex
    if let Fields::Named(ref mut fields) = input_struct.fields {
        for field in fields.named.iter_mut() {
            let field_ident = field.ident.as_ref().unwrap();

            if let Some(mutex_field) =
                mutex_fields_processed.iter().find(|m| m.ident == *field_ident)
            {
                let class_ident = &mutex_field.class_ident;
                let mutex_type = &mutex_field.mutex_type;
                if let Type::Path(ref mut type_path) = field.ty {
                    if let Some(last_segment) = type_path.path.segments.last_mut() {
                        last_segment.ident = format_ident!("KMutex");
                        last_segment.arguments = syn::PathArguments::AngleBracketed(
                            syn::parse2(quote! { <#class_ident, #mutex_type> }).unwrap(),
                        );
                    }
                }
            } else if let Some(guarded_field) =
                guarded_fields.iter().find(|f| f.ident == *field_ident)
            {
                let mutex_field_opt =
                    mutex_fields_processed.iter().find(|m| m.ident == guarded_field.mutex_ident);

                if let Some(mutex_field) = mutex_field_opt {
                    let original_ty = &field.ty;
                    let class_ident = &mutex_field.class_ident;

                    field.ty = syn::parse2(quote! {
                        ::ksync::KCell<#original_ty, #class_ident>
                    })
                    .unwrap();
                    field.attrs.push(syn::parse_quote!(#[allow(dead_code)]));
                } else {
                    errors.push(syn::Error::new(
                        guarded_field.mutex_ident.span(),
                        format!(
                            "Guarded field '{}' references non-existent mutex '{}'",
                            field_ident, guarded_field.mutex_ident
                        ),
                    ));
                }
            }
        }
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* }.into();
    }

    let (impl_generics, ty_generics, where_clause) = input_struct.generics.split_for_impl();

    // Prepare generics for Guard (add 'a).
    let mut guard_generics = input_struct.generics.clone();
    let lifetime_a: GenericParam = syn::parse2(quote! { 'a }).unwrap();
    guard_generics.params.insert(0, lifetime_a);
    let (guard_impl_generics, guard_ty_generics, _) = guard_generics.split_for_impl();

    // Prepare generics for Fields and FieldsMut (add 'b).
    let mut fields_generics = input_struct.generics.clone();
    let lifetime_b: GenericParam = syn::parse2(quote! { 'b }).unwrap();
    fields_generics.params.insert(0, lifetime_b);
    let (fields_impl_generics, fields_ty_generics, _) = fields_generics.split_for_impl();

    let ty_params: Vec<&Ident> = input_struct.generics.type_params().map(|p| &p.ident).collect();

    // 1. ZST Lock Class structures are shared globally
    let mut marker_structs = quote! {};
    for mutex in &mutex_fields_processed {
        let class_ident = &mutex.class_ident;
        marker_structs.extend(quote! {
            #[allow(non_camel_case_types)]
            #struct_vis struct #class_ident;
        });
    }

    let mut generated_code = quote! {};

    for mutex in &mutex_fields_processed {
        let mu_ident = &mutex.ident;
        let class_ident = &mutex.class_ident;
        let mutex_type = &mutex.mutex_type;

        let mu_camel = to_camel_case(&mu_ident.to_string());
        let mut guard_ident = format_ident!("{}{}Guard", struct_ident, mu_camel);
        guard_ident.set_span(mu_ident.span());

        let mut fields_ident = format_ident!("{}{}Fields", struct_ident, mu_camel);
        fields_ident.set_span(mu_ident.span());

        let mut fields_mut_ident = format_ident!("{}{}FieldsMut", struct_ident, mu_camel);
        fields_mut_ident.set_span(mu_ident.span());

        let mut lock_method_ident = format_ident!("lock_{}", mu_ident);
        lock_method_ident.set_span(mu_ident.span());

        let this_guarded_fields: Vec<&GuardedField> =
            guarded_fields.iter().filter(|f| f.mutex_ident == *mu_ident).collect();

        // Accessors generation (unified readers & pinned projection writers)
        let mut guard_accessors = quote! {};
        let mut fields_decl = quote! {};
        let mut fields_mut_decl = quote! {};
        let mut fields_init = quote! {};
        let mut fields_mut_init = quote! {};

        for field in &this_guarded_fields {
            let f_ident = &field.ident;
            let f_ty = &field.ty;
            let f_vis = &field.vis;
            let f_mut_ident = format_ident!("{}_mut", f_ident);

            guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The token is from the same parent instance as the cell.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }

                #[inline]
                #f_vis fn #f_mut_ident(self: ::core::pin::Pin<&mut Self>) -> &mut #f_ty {
                    // SAFETY: Safe projection since target fields are non-address-sensitive
                    let me = unsafe { self.get_unchecked_mut() };
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    unsafe { me.parent.#f_ident.get_mut(inner_pin.token_mut()) }
                }
            });

            fields_decl.extend(quote! {
                #f_vis #f_ident: &'b #f_ty,
            });

            fields_mut_decl.extend(quote! {
                #f_vis #f_ident: &'b mut #f_ty,
            });

            fields_init.extend(quote! {
                // SAFETY: The token is from the same parent instance as the cell.
                #f_ident: unsafe { self.parent.#f_ident.get(self.inner.token()) },
            });

            fields_mut_init.extend(quote! {
                // SAFETY: Safe projection inside dynamic stack-pinned context
                #f_ident: unsafe { &mut *me.parent.#f_ident.as_mut_ptr(token) },
            });
        }

        let non_lifetime_params: Vec<proc_macro2::TokenStream> = input_struct
            .generics
            .params
            .iter()
            .filter_map(|p| match p {
                syn::GenericParam::Type(type_param) => {
                    let ident = &type_param.ident;
                    Some(quote! { #ident })
                }
                syn::GenericParam::Const(const_param) => {
                    let ident = &const_param.ident;
                    Some(quote! { #ident })
                }
                syn::GenericParam::Lifetime(_) => None,
            })
            .collect();

        let return_ty_generics = if non_lifetime_params.is_empty() {
            quote! { <'_> }
        } else {
            quote! { <'_, #(#non_lifetime_params),*> }
        };

        // Static registration names for the kernel lock classes (using uppercase names)
        let struct_upper = struct_ident.to_string().to_ascii_uppercase();
        let mu_upper = mu_camel.to_string().to_ascii_uppercase();
        let reg_ident = format_ident!("{}_{}_REGISTRATION", struct_upper, mu_upper);
        let string_reg_ident = format_ident!("{}_{}_STRING_REG", struct_upper, mu_upper);
        let path_name = format!("{}::{}", struct_ident, mu_ident);

        generated_code.extend(quote! {
            #[allow(dead_code)]
            #struct_vis struct #fields_ident #fields_impl_generics #where_clause {
                #fields_decl
                _marker: ::core::marker::PhantomData<(&'b (), #(#ty_params),*)>,
            }

            #[allow(dead_code)]
            #struct_vis struct #fields_mut_ident #fields_impl_generics #where_clause {
                #fields_mut_decl
                _marker: ::core::marker::PhantomData<(&'b (), #(#ty_params),*)>,
            }

            ::ksync::declare_interned_string!(#string_reg_ident, #path_name);

            #[unsafe(link_section = "rust_lock_classes")]
            #[used]
            static #reg_ident: ::ksync::LockClassRegistration = ::ksync::LockClassRegistration::new(&#string_reg_ident);

            impl ::ksync::LockClass for #class_ident {
                const ID: *mut ::core::ffi::c_void = #reg_ident.get();
            }

            #[pin_init::pin_data(PinnedDrop)]
            #struct_vis struct #guard_ident #guard_impl_generics #where_clause {
                parent: &'a #struct_ident #ty_generics,
                #[pin]
                inner: ::ksync::KMutexGuard<'a, #class_ident, #mutex_type>,
            }

            #[pin_init::pinned_drop]
            impl #guard_impl_generics pin_init::PinnedDrop for #guard_ident #guard_ty_generics #where_clause {
                fn drop(self: ::core::pin::Pin<&mut Self>) {
                    // Pinned drop handles inner sub-pins automatically
                }
            }

            impl #guard_impl_generics #guard_ident #guard_ty_generics #where_clause {
                #guard_accessors

                #[inline]
                #struct_vis fn fields<'b>(&'b self) -> #fields_ident #fields_ty_generics {
                    #fields_ident {
                        #fields_init
                        _marker: ::core::marker::PhantomData,
                    }
                }

                #[inline]
                #struct_vis fn fields_mut<'b>(self: ::core::pin::Pin<&'b mut Self>) -> #fields_mut_ident #fields_ty_generics {
                    let me = unsafe { self.get_unchecked_mut() };
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let token = inner_pin.token_mut();
                    #fields_mut_ident {
                        #fields_mut_init
                        _marker: ::core::marker::PhantomData,
                    }
                }
            }

            impl #impl_generics #struct_ident #ty_generics #where_clause {
                #[inline]
                #struct_vis fn #lock_method_ident(&self) -> impl pin_init::PinInit<#guard_ident #return_ty_generics, ::core::convert::Infallible> {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new(&self.#mu_ident),
                    })
                }
            }
        });
    }

    let expanded = quote! {
        #marker_structs
        #input_struct
        #generated_code
    };

    TokenStream::from(expanded)
}

fn to_camel_case(s: &str) -> String {
    let mut camel = String::new();
    let mut capitalize = true;
    for c in s.chars() {
        if c == '_' {
            capitalize = true;
        } else if capitalize {
            camel.push(c.to_ascii_uppercase());
            capitalize = false;
        } else {
            camel.push(c);
        }
    }
    camel
}

fn is_kmutex_type(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.iter().any(|seg| seg.ident == "KMutex")
    } else {
        false
    }
}

fn extract_lock_type(ty: &Type) -> Result<Option<proc_macro2::TokenStream>, syn::Error> {
    if let Type::Path(type_path) = ty {
        if let Some(last_segment) = type_path.path.segments.last() {
            if last_segment.ident == "KMutex" {
                match &last_segment.arguments {
                    syn::PathArguments::None => return Ok(None),
                    syn::PathArguments::AngleBracketed(args) => {
                        if args.args.len() == 1 {
                            let first_arg = &args.args[0];
                            return Ok(Some(quote! { #first_arg }));
                        } else if args.args.is_empty() {
                            return Ok(None);
                        } else {
                            return Err(syn::Error::new(
                                args.span(),
                                "KMutex expects at most 1 generic argument for the lock type in struct definition",
                            ));
                        }
                    }
                    syn::PathArguments::Parenthesized(args) => {
                        return Err(syn::Error::new(
                            args.span(),
                            "KMutex does not support parenthesized generic arguments",
                        ));
                    }
                }
            }
        }
    }
    Ok(None)
}
