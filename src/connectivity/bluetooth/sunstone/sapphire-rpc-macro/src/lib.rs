// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemImpl, Pat, ReturnType, parse_macro_input};

struct MethodInfo {
    ident: syn::Ident,
    variant_ident: syn::Ident,
    is_mut_self: bool,
    is_async: bool,
    args: Vec<(syn::Ident, syn::Type)>,
    ret_ty: syn::Type,
    doc_and_cfg_attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,
}

#[proc_macro_attribute]
pub fn rpc(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_impl = parse_macro_input!(item as ItemImpl);

    let server_ident = match &*item_impl.self_ty {
        syn::Type::Path(p) => {
            if let Some(seg) = p.path.segments.last() {
                seg.ident.clone()
            } else {
                return syn::Error::new_spanned(&item_impl.self_ty, "Expected valid type path")
                    .to_compile_error()
                    .into();
            }
        }
        _ => {
            return syn::Error::new_spanned(&item_impl.self_ty, "Expected struct/enum path type")
                .to_compile_error()
                .into();
        }
    };

    let request_ident = format_ident!("{}Request", server_ident);
    let response_ident = format_ident!("{}Response", server_ident);
    let rpc_ident = format_ident!("{}Rpc", server_ident);
    let client_ident = format_ident!("{}Client", server_ident);

    let mut methods = Vec::new();
    for item in &item_impl.items {
        if let syn::ImplItem::Fn(method) = item {
            let Some(FnArg::Receiver(receiver)) = method.sig.inputs.first() else {
                continue;
            };

            let is_mut_self = receiver.mutability.is_some();
            let is_async = method.sig.asyncness.is_some();
            let ident = method.sig.ident.clone();
            let variant_ident = ident.clone();

            let mut args = Vec::new();
            for (idx, input) in method.sig.inputs.iter().skip(1).enumerate() {
                if let FnArg::Typed(pat_ty) = input {
                    let arg_ident = match &*pat_ty.pat {
                        Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                        _ => syn::Ident::new(&format!("arg{}", idx), method.sig.ident.span()),
                    };
                    args.push((arg_ident, (*pat_ty.ty).clone()));
                }
            }

            let ret_ty = match &method.sig.output {
                ReturnType::Default => syn::parse_quote!(()),
                ReturnType::Type(_, ty) => (**ty).clone(),
            };

            let doc_and_cfg_attrs: Vec<syn::Attribute> = method
                .attrs
                .iter()
                .filter(|attr| attr.path().is_ident("doc") || attr.path().is_ident("cfg"))
                .cloned()
                .collect();

            let vis = method.vis.clone();

            methods.push(MethodInfo {
                ident,
                variant_ident,
                is_mut_self,
                is_async,
                args,
                ret_ty,
                doc_and_cfg_attrs,
                vis,
            });
        }
    }

    let any_mut_self = methods.iter().any(|m| m.is_mut_self);
    let route_self_arg = if any_mut_self { quote!(&mut self) } else { quote!(&self) };

    let req_variants = methods.iter().map(|m| {
        let v_ident = &m.variant_ident;
        let attrs = &m.doc_and_cfg_attrs;
        if m.args.is_empty() {
            quote! { #(#attrs)* #v_ident }
        } else {
            let idents = m.args.iter().map(|(id, _)| id);
            let types = m.args.iter().map(|(_, ty)| ty);
            quote! { #(#attrs)* #v_ident { #(#idents: #types),* } }
        }
    });

    let res_variants = methods.iter().map(|m| {
        let v_ident = &m.variant_ident;
        let attrs = &m.doc_and_cfg_attrs;
        let ret_ty = &m.ret_ty;
        quote! { #(#attrs)* #v_ident(#ret_ty) }
    });

    let client_methods = methods.iter().map(|m| {
        let ident = &m.ident;
        let v_ident = &m.variant_ident;
        let ret_ty = &m.ret_ty;
        let vis = &m.vis;
        let attrs = &m.doc_and_cfg_attrs;
        let arg_idents: Vec<_> = m.args.iter().map(|(id, _)| id).collect();
        let arg_types: Vec<_> = m.args.iter().map(|(_, ty)| ty).collect();

        let req_init = if m.args.is_empty() {
            quote! { #request_ident::#v_ident }
        } else {
            quote! { #request_ident::#v_ident { #(#arg_idents),* } }
        };

        quote! {
            #(#attrs)*
            #[allow(non_snake_case, dead_code)]
            #vis async fn #ident(&self, #(#arg_idents: #arg_types),*) -> ::core::result::Result<#ret_ty, ::sapphire_async::rpc::CallError> {
                let req = #req_init;
                let res = self.client.call(req).await?;
                match res {
                    #response_ident::#v_ident(val) => ::core::result::Result::Ok(val),
                    _ => ::core::unreachable!("RPC response mismatch for {}", ::core::stringify!(#ident)),
                }
            }
        }
    });

    let route_arms = methods.iter().map(|m| {
        let ident = &m.ident;
        let v_ident = &m.variant_ident;
        let arg_idents: Vec<_> = m.args.iter().map(|(id, _)| id).collect();

        let pat = if m.args.is_empty() {
            quote! { #request_ident::#v_ident }
        } else {
            quote! { #request_ident::#v_ident { #(#arg_idents),* } }
        };

        let call_expr = if m.is_async {
            quote! { self.#ident(#(#arg_idents),*).await }
        } else {
            quote! { self.#ident(#(#arg_idents),*) }
        };

        quote! {
            #pat => {
                let res = #call_expr;
                responder.respond(#response_ident::#v_ident(res));
            }
        }
    });

    let route_method: syn::ImplItem = if methods.is_empty() {
        syn::parse_quote! {
            async fn route_request<Cfg, C>(
                #route_self_arg,
                request: #request_ident,
                _responder: ::sapphire_async::rpc::Responder<#rpc_ident, Cfg, C>,
            ) where
                Cfg: ::sapphire_async::rpc::RpcCfg,
                C: ::core::ops::Deref<Target = ::sapphire_async::rpc::RpcChannel<#rpc_ident, Cfg>>,
            {
                match request {
                    _ => ::core::unreachable!("No RPC endpoints defined"),
                }
            }
        }
    } else {
        syn::parse_quote! {
            async fn route_request<Cfg, C>(
                #route_self_arg,
                request: #request_ident,
                responder: ::sapphire_async::rpc::Responder<#rpc_ident, Cfg, C>,
            ) where
                Cfg: ::sapphire_async::rpc::RpcCfg,
                C: ::core::ops::Deref<Target = ::sapphire_async::rpc::RpcChannel<#rpc_ident, Cfg>>,
            {
                match request {
                    #(#route_arms)*
                }
            }
        }
    };

    let (impl_generics, _, where_clause) = item_impl.generics.split_for_impl();
    let self_ty = &item_impl.self_ty;

    let output = quote! {
        #[derive(::core::fmt::Debug)]
        #[allow(non_camel_case_types, non_snake_case, dead_code)]
        pub enum #request_ident {
            #(#req_variants),*
        }

        #[derive(::core::fmt::Debug)]
        #[allow(non_camel_case_types, non_snake_case, dead_code)]
        pub enum #response_ident {
            #(#res_variants),*
        }

        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
        pub struct #rpc_ident;

        impl ::sapphire_async::rpc::Rpc for #rpc_ident {
            type Request = #request_ident;
            type Response = #response_ident;
        }

        #[derive(::core::fmt::Debug)]
        pub struct #client_ident<C: ::sapphire_async::rpc::RpcHandles> {
            client: ::sapphire_async::rpc::Client<C>,
        }

        impl<C: ::sapphire_async::rpc::RpcHandles> #client_ident<C> {
            pub const fn new(client: ::sapphire_async::rpc::Client<C>) -> Self {
                Self { client }
            }
        }

        impl<C: ::sapphire_async::rpc::RpcHandles> ::core::convert::From<::sapphire_async::rpc::Client<C>> for #client_ident<C> {
            fn from(client: ::sapphire_async::rpc::Client<C>) -> Self {
                Self::new(client)
            }
        }

        impl<C: ::sapphire_async::rpc::RpcHandles + ::core::clone::Clone> ::core::clone::Clone for #client_ident<C> {
            fn clone(&self) -> Self {
                Self { client: self.client.clone() }
            }
        }

        impl<C: ::sapphire_async::rpc::RpcHandles> ::core::ops::Deref for #client_ident<C> {
            type Target = ::sapphire_async::rpc::Client<C>;
            fn deref(&self) -> &Self::Target {
                &self.client
            }
        }

        impl<C, Cfg> #client_ident<C>
        where
            C: ::sapphire_async::rpc::RpcHandles + ::core::ops::Deref<Target = ::sapphire_async::rpc::RpcChannel<#rpc_ident, Cfg>>,
            Cfg: ::sapphire_async::rpc::RpcCfg,
        {
            #(#client_methods)*
        }

        #item_impl

        impl #impl_generics #self_ty #where_clause {
            #route_method
        }
    };

    output.into()
}
