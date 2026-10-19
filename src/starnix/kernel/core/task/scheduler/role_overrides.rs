// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::ByteSlice;
use regex_lite::{Error, Regex};
use starnix_task_command::TaskCommand;

/// Per-container overrides for thread roles.
#[derive(Debug)]
pub struct RoleOverrides {
    process_filter: Vec<Regex>,
    thread_filter: Vec<Regex>,
    cgroup_filter: Vec<Option<Regex>>,
    is_leader_filter: Vec<Option<bool>>,
    min_nice_filter: Vec<Option<i8>>,
    max_nice_filter: Vec<Option<i8>>,
    role_names: Vec<String>,
}

impl RoleOverrides {
    /// Create a new builder for role overrides.
    pub fn new() -> RoleOverridesBuilder {
        RoleOverridesBuilder {
            process_patterns: vec![],
            thread_patterns: vec![],
            cgroup_patterns: vec![],
            is_leader_filters: vec![],
            min_nice_filters: vec![],
            max_nice_filters: vec![],
            role_names: vec![],
        }
    }

    /// Get the overridden role name (if any) for provided process and thread names.
    pub fn get_role_name<'a>(
        &self,
        process_name: &TaskCommand,
        thread_name: &TaskCommand,
        cgroup_path: &str,
    ) -> Option<&str> {
        self.get_role_name_with_filters(process_name, thread_name, cgroup_path, false, 0)
    }

    /// Get the overridden role name (if any) for provided process, thread, cgroup, leadership, and
    /// nice attributes.
    pub fn get_role_name_with_filters<'a>(
        &self,
        process_name: &TaskCommand,
        thread_name: &TaskCommand,
        cgroup_path: &str,
        is_leader: bool,
        nice: i8,
    ) -> Option<&str> {
        debug_assert_eq!(self.process_filter.len(), self.role_names.len());
        debug_assert_eq!(self.thread_filter.len(), self.role_names.len());
        debug_assert_eq!(self.cgroup_filter.len(), self.role_names.len());
        debug_assert_eq!(self.is_leader_filter.len(), self.role_names.len());
        debug_assert_eq!(self.min_nice_filter.len(), self.role_names.len());
        debug_assert_eq!(self.max_nice_filter.len(), self.role_names.len());

        // NOTE(https://fxbug.dev/483609435): This used to be more elegantly expressed
        // via use of regex::bytes::RegexSet, but regex_lite doesn't (yet?) offer RegexSet.
        let process_name = process_name.as_bytes().to_str().ok()?;
        let thread_name = thread_name.as_bytes().to_str().ok()?;
        for index in 0..self.process_filter.len() {
            if self.is_leader_filter[index].is_none_or(|expected| expected == is_leader)
                && self.min_nice_filter[index].is_none_or(|min| nice >= min)
                && self.max_nice_filter[index].is_none_or(|max| nice <= max)
                && self.process_filter[index].is_match(process_name)
                && self.thread_filter[index].is_match(thread_name)
                && self.cgroup_filter[index].as_ref().is_none_or(|r| r.is_match(cgroup_path))
            {
                return Some(self.role_names[index].as_str());
            }
        }
        None
    }
}

/// Builder for `RoleOverrides`.
pub struct RoleOverridesBuilder {
    process_patterns: Vec<String>,
    thread_patterns: Vec<String>,
    cgroup_patterns: Vec<Option<String>>,
    is_leader_filters: Vec<Option<bool>>,
    min_nice_filters: Vec<Option<i8>>,
    max_nice_filters: Vec<Option<i8>>,
    role_names: Vec<String>,
}

impl RoleOverridesBuilder {
    /// Add a new override to the configuration.
    pub fn add(
        &mut self,
        process: impl Into<String>,
        thread: impl Into<String>,
        cgroup: Option<String>,
        role_name: impl Into<String>,
    ) {
        self.add_with_filters(process, thread, cgroup, None, None, None, role_name);
    }

    /// Add a new override with optional thread-group leader and nice range filters.
    pub fn add_with_filters(
        &mut self,
        process: impl Into<String>,
        thread: impl Into<String>,
        cgroup: Option<String>,
        is_leader: Option<bool>,
        min_nice: Option<i8>,
        max_nice: Option<i8>,
        role_name: impl Into<String>,
    ) {
        self.process_patterns.push(process.into());
        self.thread_patterns.push(thread.into());
        self.cgroup_patterns.push(cgroup);
        self.is_leader_filters.push(is_leader);
        self.min_nice_filters.push(min_nice);
        self.max_nice_filters.push(max_nice);
        self.role_names.push(role_name.into());
    }

    /// Compile all of the provided regular expressions and return a `RoleOverrides`.
    pub fn build(self) -> Result<RoleOverrides, Error> {
        let cgroup_filter = self
            .cgroup_patterns
            .into_iter()
            .map(|opt| opt.map(|p| Regex::new(p.as_str())).transpose())
            .collect::<Result<Vec<Option<Regex>>, Error>>()?;

        Ok(RoleOverrides {
            process_filter: self
                .process_patterns
                .into_iter()
                .map(|pattern| Regex::new(pattern.as_str()))
                .collect::<Result<Vec<Regex>, Error>>()?,
            thread_filter: self
                .thread_patterns
                .into_iter()
                .map(|pattern| Regex::new(pattern.as_str()))
                .collect::<Result<Vec<Regex>, Error>>()?,
            cgroup_filter,
            is_leader_filter: self.is_leader_filters,
            min_nice_filter: self.min_nice_filters,
            max_nice_filter: self.max_nice_filters,
            role_names: self.role_names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_role_name<'a>(
        mappings: &'a RoleOverrides,
        process_name: &str,
        thread_name: &str,
        cpuset_path: &str,
    ) -> Option<&'a str> {
        mappings.get_role_name(
            &TaskCommand::new(process_name.as_bytes()),
            &TaskCommand::new(thread_name.as_bytes()),
            cpuset_path,
        )
    }

    #[fuchsia::test]
    fn single_pattern() {
        let mut builder = RoleOverrides::new();
        builder.add("process_prefix_.+", "thread_prefix_.+", None, "replacement_role");
        let mappings = builder.build().unwrap();

        assert_eq!(
            str_role_name(&mappings, "process_prefix_foo", "thread_prefix_bar", "/"),
            Some("replacement_role")
        );
        assert_eq!(str_role_name(&mappings, "process_prefix_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "process_prefix_bar", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "non_matching", "/"), None);
    }

    #[fuchsia::test]
    fn multiple_patterns() {
        let mut builder = RoleOverrides::new();
        builder.add("pre_one.+", "pre_one.+", None, "replace_one");
        builder.add("pre_two.+", "pre_two.+", None, "replace_two");
        builder.add("pre_three.+", "pre_three.+", None, "replace_three");
        builder.add("pre_four.+", "pre_four.+", None, "replace_four");
        let mappings = builder.build().unwrap();

        assert_eq!(
            str_role_name(&mappings, "pre_one_foo", "pre_one_bar", "/"),
            Some("replace_one")
        );
        assert_eq!(str_role_name(&mappings, "pre_one_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_one_bar", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "non_matching", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_two_foo", "pre_two_bar", "/"),
            Some("replace_two")
        );
        assert_eq!(str_role_name(&mappings, "pre_two_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_two_bar", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_three_foo", "pre_three_bar", "/"),
            Some("replace_three")
        );
        assert_eq!(str_role_name(&mappings, "pre_three_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_three_bar", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_four_foo", "pre_four_bar", "/"),
            Some("replace_four")
        );
        assert_eq!(str_role_name(&mappings, "pre_four_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_four_bar", "/"), None);
    }

    #[fuchsia::test]
    fn cgroup_patterns() {
        let mut builder = RoleOverrides::new();
        builder.add("proc", "thread", Some("/background".to_string()), "bg_role");
        builder.add("proc", "thread", Some("/foreground".to_string()), "fg_role");
        builder.add("proc", "thread", None, "default_role");
        let mappings = builder.build().unwrap();

        assert_eq!(str_role_name(&mappings, "proc", "thread", "/background"), Some("bg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/foreground"), Some("fg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/other"), Some("default_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/"), Some("default_role"));
    }

    #[fuchsia::test]
    fn leader_and_nice_filters() {
        let mut builder = RoleOverrides::new();
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            Some(true),
            None,
            None,
            "leader_role",
        );
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            None,
            Some(1),
            None,
            "low_prio_worker_role",
        );
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            None,
            None,
            Some(-1),
            "boosted_worker_role",
        );
        builder.add(".+", ".+", Some("^/active$".to_string()), "default_worker_role");
        let mappings = builder.build().unwrap();

        let proc_cmd = TaskCommand::new(b"app");
        let worker_cmd = TaskCommand::new(b"worker");

        assert_eq!(
            mappings.get_role_name_with_filters(&proc_cmd, &proc_cmd, "/active", true, 0),
            Some("leader_role")
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc_cmd, &worker_cmd, "/active", false, 0),
            Some("default_worker_role")
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc_cmd, &worker_cmd, "/active", false, 10),
            Some("low_prio_worker_role")
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc_cmd, &worker_cmd, "/active", false, -2),
            Some("boosted_worker_role")
        );
    }
}
