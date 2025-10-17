// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use regex::{Captures, Regex};
use serde_json::Value;
use std::path::PathBuf;

mod build;
mod cache;
mod config;
mod data;
pub(crate) mod env_var;
mod file_check;
mod filter;
mod flatten;
mod home;
mod runtime;
mod shared_data;
mod workspace;

pub(crate) use self::build::build;
pub(crate) use self::home::home;
pub(crate) use cache::cache;
pub(crate) use config::config;
pub(crate) use data::data;
pub(crate) use env_var::env_var;
pub(crate) use file_check::file_check;
pub(crate) use filter::filter;
pub(crate) use flatten::flatten;
pub(crate) use runtime::runtime;
pub(crate) use shared_data::shared_data;
pub(crate) use workspace::workspace;

// Negative lookbehind (or lookahead for that matter) is not supported in Rust's regex.
// Instead, replace with this string - which hopefully will not be used by anyone in the
// configuration.  Insert joke here about how hope is not a strategy.
const TEMP_REPLACE: &str = "#<#ffx!!replace#>#";

fn preprocess(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.to_string()).map(|s| s.replace("$$", TEMP_REPLACE))
}

fn postprocess(value: String) -> Value {
    Value::String(value.replace(TEMP_REPLACE, "$"))
}

fn replace_regex<T>(value: &String, regex: &Regex, replacer: T) -> String
where
    T: Fn(&str) -> Result<String>,
{
    regex
        .replace_all(value, |caps: &Captures<'_>| {
            // Skip the first one since that'll be the whole string.
            caps.iter().skip(1).map(|cap| cap.map(|c| replacer(c.as_str()))).fold(
                String::new(),
                |acc, v| {
                    if let Some(Ok(s)) = v { acc + &s } else { acc }
                },
            )
        })
        .into_owned()
}

// Replace at most one occurrence of the regex with the replacer(str). If the replacer
// returns an Err, this function returns that error.
// Test-only until the next CL
#[cfg(test)]
fn try_replace_regex<T>(value: &String, regex: &Regex, replacer: T) -> Result<String>
where
    T: Fn(&str) -> Result<String>,
{
    // Check if we need the replacement before actually doing the replacement, so we can determine
    // whether the replacer is returning an error.
    // Note: this check is why this function only works on the first
    // replacement. But there is no use-case where we need multiple
    // replacements, so it keeps the code simple.
    if let Some(m) = regex.find(value) {
        let replacement = replacer(m.as_str())?;
        Ok(regex.replace(value, regex::NoExpand(&replacement)).into_owned())
    } else {
        Ok(value.clone())
    }
}

fn replace<'a, P>(regex: &'a Regex, base_path: P, value: Value) -> Option<Value>
where
    P: Fn() -> Result<PathBuf> + Sync + Send + 'a,
{
    preprocess(&value)
        .as_ref()
        .map(|s| {
            replace_regex(s, regex, |v| {
                match base_path() {
                    Ok(p) => Ok(p.to_str().map_or_else(|| v.to_string(), |s| s.to_string())),
                    Err(_) => Ok(v.to_string()), //just pass through
                }
            })
        })
        .map(postprocess)
        .or(Some(value))
}

// Test-only until the next CL
#[cfg(test)]
fn try_replace<'a, P>(regex: &'a Regex, base_path: P, value: Value) -> Result<Value>
where
    P: Fn() -> Result<PathBuf> + Sync + Send + 'a,
{
    if let Some(ref s) = preprocess(&value) {
        Ok(postprocess(try_replace_regex(s, regex, |_| {
            // Don't invoke the base_path closure until we know we actually need it (i.e. when regex.replace
            // actually calls the "replacer" closure)
            let p = base_path()?;
            p.to_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {p:?}"))
        })?))
    } else {
        // Not a string
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serde_json::json;

    #[test]
    fn test_try_replace_regex_success() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = "hello $TEST world".to_string();
        let result = try_replace_regex(&value, &regex, |_| Ok("replaced".to_string()));
        assert_eq!(result.unwrap(), "hello replaced world");
    }

    #[test]
    fn test_try_replace_regex_error() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = "hello $TEST world".to_string();
        let result = try_replace_regex(&value, &regex, |_| Err(anyhow!("test error")));
        assert!(result.is_err());
    }

    #[test]
    fn test_try_replace_regex_no_match() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = "hello world".to_string();
        let result = try_replace_regex(&value, &regex, |_| Ok("replaced".to_string()));
        assert_eq!(result.unwrap(), "hello world");
    }

    #[test]
    fn test_try_replace_success() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = json!("hello $TEST world");
        let result = try_replace(&regex, || Ok(PathBuf::from("/test")), value);
        assert_eq!(result.unwrap(), json!("hello /test world"));
    }

    #[test]
    fn test_try_replace_error() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = json!("hello $TEST world");
        let result = try_replace(&regex, || Err(anyhow!("test error")), value);
        assert!(result.is_err());
    }

    #[test]
    fn test_try_replace_no_match() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = json!("hello world");
        let result = try_replace(&regex, || Ok(PathBuf::from("/test")), value);
        assert_eq!(result.unwrap(), json!("hello world"));
    }

    #[test]
    fn test_try_replace_not_string() {
        let regex = Regex::new(r"\$TEST").unwrap();
        let value = json!(123);
        let result = try_replace(&regex, || Ok(PathBuf::from("/test")), value.clone());
        assert_eq!(result.unwrap(), value);
    }
}
