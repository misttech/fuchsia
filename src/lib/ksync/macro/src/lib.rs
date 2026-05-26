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

            for (idx, attr) in field.attrs.iter().enumerate() {
                if attr.path().is_ident("mutex") {
                    is_mutex = true;
                    attrs_to_remove.push(idx);

                    if !attr.meta.require_path_only().is_ok() {
                        errors.push(syn::Error::new(
                            attr.meta.span(),
                            "#[mutex] attribute does not accept any parameters",
                        ));
                    }
                } else if attr.path().is_ident("guarded_by") {
                    if let syn::Meta::List(ref meta_list) = attr.meta {
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
                mutex_fields.push(field.clone());
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

    let struct_ident = &input_struct.ident;
    let struct_vis = &input_struct.vis;

    let mut mutex_fields_processed = Vec::new();
    let mut generated_names = std::collections::HashSet::new();

    for field in mutex_fields {
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

        mutex_fields_processed.push(MutexField { ident: field_ident, class_ident });
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* }.into();
    }

    // Rewrite fields in the struct.
    if let Fields::Named(ref mut fields) = input_struct.fields {
        for field in fields.named.iter_mut() {
            let field_ident = field.ident.as_ref().unwrap();

            if let Some(mutex_field) =
                mutex_fields_processed.iter().find(|m| m.ident == *field_ident)
            {
                let class_ident = &mutex_field.class_ident;
                if let Type::Path(ref mut type_path) = field.ty {
                    if let Some(last_segment) = type_path.path.segments.last_mut() {
                        last_segment.arguments = syn::PathArguments::AngleBracketed(
                            syn::parse2(quote! { <#class_ident> }).unwrap(),
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
                #f_vis fn #f_mut_ident(&mut self) -> &mut #f_ty {
                    // SAFETY: The token is from the same parent instance as the cell.
                    unsafe { self.parent.#f_ident.get_mut(self.inner.token_mut()) }
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
                // SAFETY:
                // 1. We have exclusive access to the Guard (&mut self).
                // 2. The fields in the struct are disjoint.
                // 3. The returned references are bound to the lifetime 'b of the guard borrow,
                //    preventing them from outliving the guard.
                #f_ident: unsafe { &mut *self.parent.#f_ident.as_mut_ptr(token) },
            });
        }

        let return_ty_generics = if ty_params.is_empty() {
            quote! { <'_> }
        } else {
            quote! { <'_, #(#ty_params),*> }
        };

        generated_code.extend(quote! {
            // Guard Struct
            #struct_vis struct #guard_ident #guard_impl_generics #where_clause {
                parent: &'a #struct_ident #ty_generics,
                inner: ::ksync::KMutexGuard<'a, #class_ident>,
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
                #struct_vis fn fields_mut<'b>(&'b mut self) -> #fields_mut_ident #fields_ty_generics {
                    let token = self.inner.token_mut();
                    #fields_mut_ident {
                        #fields_mut_init
                        _marker: ::core::marker::PhantomData,
                    }
                }
            }


            // Fields Struct (Shared)
            #struct_vis struct #fields_ident #fields_impl_generics #where_clause {
                #fields_decl
                _marker: ::core::marker::PhantomData<(&'b (), #(#ty_params),*)>,
            }

            // Fields Struct (Mut)
            #struct_vis struct #fields_mut_ident #fields_impl_generics #where_clause {
                #fields_mut_decl
                _marker: ::core::marker::PhantomData<(&'b (), #(#ty_params),*)>,
            }

            // Lock method on parent struct
            impl #impl_generics #struct_ident #ty_generics #where_clause {
                #[inline]
                #struct_vis fn #lock_method_ident(&self) -> #guard_ident #return_ty_generics {
                    #guard_ident {
                        parent: self,
                        inner: self.#mu_ident.lock(),
                    }
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
