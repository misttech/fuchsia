// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::TokenStream;
use std::collections::{BTreeMap, BTreeSet};
use syn::Ident;

#[derive(Clone, PartialEq, Eq, Ord, PartialOrd)]
struct Edge {
    from: Ident,
    to: Ident,
}

impl syn::parse::Parse for Edge {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let from = input.parse::<syn::Ident>()?;
        input.parse::<syn::Token![=>]>()?;
        let to: syn::Ident = input.parse()?;
        let _ = input.parse::<syn::Token![,]>();
        Ok(Edge { from, to })
    }
}

struct Graph {
    levels: BTreeSet<Ident>,
    edges: BTreeSet<Edge>,
}

impl Graph {
    fn in_degrees(&self) -> BTreeMap<Ident, usize> {
        let mut in_degrees: BTreeMap<Ident, usize> =
            self.levels.iter().map(|l| (l.clone(), 0)).collect();
        for Edge { to, .. } in self.edges.iter() {
            *in_degrees.get_mut(to).unwrap() += 1;
        }
        in_degrees
    }
}

impl syn::parse::Parse for Graph {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut levels = BTreeSet::new();
        let mut edges = BTreeSet::new();
        while !input.is_empty() {
            let edge: Edge = input.parse()?;
            let Edge { from, to } = edge.clone();
            levels.insert(to);
            levels.insert(from);
            edges.insert(edge);
        }
        Ok(Self { levels, edges })
    }
}

/// Collect the list of all pairs of nodes where one can be reached from another.
fn build_lock_graph(
    current: &Ident,
    past: &mut Vec<Ident>,
    adj_list: &BTreeMap<Ident, BTreeSet<Ident>>,
    all_paths: &mut BTreeSet<Edge>,
) {
    for p in past.iter() {
        if p == current {
            panic!("Detected a cycle in the lock ordering graph on level {p}.");
        }
        all_paths.insert(Edge { from: p.clone(), to: current.clone() });
    }
    let node = current.clone();
    past.push(node);
    for id in &adj_list[current] {
        build_lock_graph(&id, past, adj_list, all_paths)
    }
    past.pop();
}

/// This macro takes a definition of the lock ordering graph in the form of
/// lock_ordering!{
///     Unlocked -> A,
///     A -> B,
///     Unlocked -> C,
/// }
///
/// and defines the edges as lock level, as well as implementing LockBefore<X>
/// for all the levels from which X is reachable.
#[proc_macro]
pub fn lock_ordering(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as Graph);
    let levels = &graph.levels;
    let edges = &graph.edges;
    let mut adj_list: BTreeMap<Ident, BTreeSet<Ident>> = BTreeMap::new();

    let mut result = proc_macro2::TokenStream::new();
    for level in levels.iter() {
        adj_list.insert(level.clone(), BTreeSet::new());
        if *level != "Unlocked" {
            result.extend(quote::quote! {
                pub enum #level {}
                impl starnix_sync::LockEqualOrBefore<#level> for #level {}
            });
        }
    }
    for Edge { from, to } in edges.iter() {
        adj_list
            .get_mut(&from)
            .expect("Unexpected level in lock leveling graph")
            .insert(to.clone());
    }

    let unlocked_id = Ident::new("Unlocked", proc_macro2::Span::call_site());
    let mut past: Vec<Ident> = vec![];
    let mut all_edges: BTreeSet<Edge> = BTreeSet::new();
    build_lock_graph(&unlocked_id, &mut past, &adj_list, &mut all_edges);

    let mut in_degree = graph.in_degrees();

    let mut queue: std::collections::BTreeSet<Ident> = in_degree
        .iter()
        .filter_map(|(k, &v)| if v == 0 { Some(k.clone()) } else { None })
        .collect();

    let mut next_id: usize = 0;
    let mut lock_ids: BTreeMap<Ident, usize> = BTreeMap::new();

    while let Some(node) = queue.pop_first() {
        if node != "Unlocked" {
            lock_ids.insert(node.clone(), next_id);
            // Space out IDs by 16 (4 bits) for subclassing
            next_id += 16;
        }
        if let Some(neighbors) = adj_list.get(&node) {
            for neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                assert!(*deg > 0);
                *deg -= 1;
                if *deg == 0 {
                    queue.insert(neighbor.clone());
                }
            }
        }
    }

    for Edge { from, to } in all_edges.into_iter() {
        result.extend(quote::quote! {
            impl starnix_sync::LockAfter<#from> for #to {}
        });
    }

    for (level, id) in lock_ids {
        let name = level.to_string();
        result.extend(quote::quote! {
            impl #level {
                pub const LOCK_ID: usize = #id;
            }
            impl starnix_sync::LockLevel for #level {
                const LOCK_ID: usize = #id;
                fn name() -> &'static str { #name }
            }
        });
    }

    result.into()
}
