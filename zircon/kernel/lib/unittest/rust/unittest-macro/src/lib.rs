// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Block, Error, Expr, Ident, Item, ItemFn, ItemMod, ItemUse, Lit, Meta, ReturnType,
    Type, parse_macro_input,
};

const SECTION_NAME: &str = ".data.rel.ro.unittest_testcases";

#[proc_macro_attribute]
pub fn test_suite(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return Error::new_spanned(
            TokenStream2::from(attr),
            "test_suite attribute does not take arguments",
        )
        .to_compile_error()
        .into();
    }

    let suite = parse_macro_input!(item as TestSuite);
    quote!(#suite).into()
}

// Represents a test function, as defined within a #[test_suite]-annotated
// module.
struct TestCase {
    // The name of the test.
    ident: Ident,
    // A brief description of the test.
    docstring: String,
    // The test function's body.
    block: Block,
}

impl TestCase {
    fn from_item_fn(item_fn: ItemFn) -> Result<Self, Error> {
        if !item_fn.sig.generics.params.is_empty() {
            return Err(Error::new_spanned(
                &item_fn.sig.generics,
                "a test function cannot be generic",
            ));
        }

        if !item_fn.sig.inputs.is_empty() {
            return Err(Error::new_spanned(
                &item_fn.sig.inputs,
                "a test function must take no arguments",
            ));
        }

        match &item_fn.sig.output {
            ReturnType::Default => {}
            ReturnType::Type(_, ty) => {
                let unit_type = if let Type::Tuple(type_tuple) = &**ty {
                    type_tuple.elems.is_empty()
                } else {
                    false
                };
                if !unit_type {
                    return Err(Error::new_spanned(ty, "a test function must return ()"));
                }
            }
        }

        let docstring =
            get_single_line_docstring(&item_fn.sig.ident, &item_fn.attrs, "test function")?;

        Ok(Self { ident: item_fn.sig.ident, docstring, block: *item_fn.block })
    }
}

impl ToTokens for TestCase {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let ident = &self.ident;
        let block = &self.block;
        let doc_comment = format!(" {}", self.docstring);

        // Declarative macros can't reference variables not defined in their
        // scope, but they can reference other macros, so we introduce
        // record_failure!() for use in the assert/expect macros.
        tokens.extend(quote! {
            #[doc = #doc_comment]
            #[allow(unused_assignments)]
            pub extern "C" fn #ident() -> bool {
                let mut all_ok = true;
                #[allow(unused_macros)]
                macro_rules! record_failure {
                    () => { all_ok = false; };
                }
                #block
                all_ok
            }
        });
    }
}

struct TestSuite {
    ident: Ident,
    docstring: String,
    uses: Vec<ItemUse>,
    cases: Vec<TestCase>,
}

impl Parse for TestSuite {
    fn parse(input: ParseStream<'_>) -> Result<Self, Error> {
        let mut input_mod: ItemMod = input.parse()?;
        let mod_ident = &input_mod.ident;

        let docstring =
            get_single_line_docstring(mod_ident, &input_mod.attrs, "test suite module")?;

        let content = input_mod
            .content
            .take()
            .ok_or_else(|| Error::new_spanned(mod_ident, "a test suite module must by non-empty)"))?
            .1;

        let mut cases = Vec::new();
        let mut uses = Vec::new();
        for item in content {
            match item {
                Item::Fn(item_fn) => {
                    cases.push(TestCase::from_item_fn(item_fn)?);
                }
                Item::Use(item_use) => {
                    uses.push(item_use);
                }
                _ => {
                    return Err(Error::new_spanned(
                        item,
                        "a test suite module may only contain test functions and use statements",
                    ));
                }
            }
        }

        if cases.is_empty() {
            return Err(Error::new_spanned(
                mod_ident,
                "a test suite module must contain at least one test function",
            ));
        }

        Ok(Self { ident: mod_ident.clone(), docstring, uses, cases })
    }
}

impl ToTokens for TestSuite {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let mod_ident = &self.ident;
        let suite_name_c_str = format!("{mod_ident}\0");
        let suite_desc_c_str = format!("{}\0", self.docstring);
        let doc_comment = format!(" {}", self.docstring);

        let uses = &self.uses;
        let cases = &self.cases;
        let reg_entries = cases.iter().map(|case| {
            let ident = &case.ident;
            let case_name_c_str = format!("{ident}\0");
            quote! {
                ::unittest::TestCaseRegistration {
                    name: #case_name_c_str.as_ptr() as *const core::ffi::c_char,
                    fn_: #ident,
                }
            }
        });

        let test_count = cases.len();
        tokens.extend(quote! {
            #[doc = #doc_comment]
            pub mod #mod_ident {
                use super::*;

                #( #uses )*

                #[cfg(not(ktest))]
                compile_error!("#[test_suite] may only be used in a cfg(ktest) context");

                #( #cases )*

                const _: () = {
                    static TESTS: [::unittest::TestCaseRegistration; #test_count] = [
                        #( #reg_entries ),*
                    ];

                    #[unsafe(link_section = #SECTION_NAME)]
                    #[used]
                    static SUITE: ::unittest::TestSuiteRegistration = ::unittest::TestSuiteRegistration {
                        name: #suite_name_c_str.as_ptr() as *const core::ffi::c_char,
                        desc: #suite_desc_c_str.as_ptr() as *const core::ffi::c_char,
                        tests: TESTS.as_ptr(),
                        test_cnt: #test_count,
                    };
                };
            }
        });
    }
}

// Gets an item's docstring, if present, and enforces that it be a single line
// the sake of terminal printing brevity.
fn get_single_line_docstring(
    item: &Ident,
    attrs: &[Attribute],
    what: &str,
) -> Result<String, Error> {
    let mut doc_attr = None;
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if doc_attr.is_some() {
            return Err(Error::new_spanned(
                item,
                format!("a {what}'s docstring must be a single line. Keep it brief!"),
            ));
        }
        doc_attr = Some(attr);
    }
    let Some(doc_attr) = doc_attr else {
        return Err(Error::new_spanned(
            item,
            format!("a {what} must have a docstring description"),
        ));
    };

    if let Meta::NameValue(meta_name_value) = &doc_attr.meta {
        if let Expr::Lit(expr_lit) = &meta_name_value.value {
            if let Lit::Str(lit_str) = &expr_lit.lit {
                let val = lit_str.value().trim().to_string();
                if !val.is_empty() {
                    return Ok(val);
                }
            }
        }
    }
    Err(Error::new_spanned(item, format!("a {what} docstring cannot be empty")))
}
