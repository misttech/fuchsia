// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::EvalOutcome;
use crate::args::Args;
use crate::collections::{FlatMap, FlatSet};
use crate::serialization::{BStrExt, Deserialize, Serialize};
use crate::string::parse_int;
use bstr::{BStr, BString, ByteSlice};
use std::ffi::{CString, NulError};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;

const DEFAULT_PATH: &str = "/bin:/pkg/bin";
const DEFAULT_SHELL_NAME: &str = "zxsh";

/// Represents a running background process tracked by the shell.
pub struct BgJob {
    /// Handle to the underlying Zircon process.
    pub process: zx::Process,
}

/// A RAII guard that temporarily modifies variable state and restores the original values on drop.
pub struct StateBackupGuard<'a> {
    /// Mutable reference to the active shell state.
    pub state: &'a mut ShellState,
    /// List of backed-up variables: `(var_name, previous_value_or_none_if_unset)`.
    pub backups: Vec<(BString, Option<BString>)>,
}

impl<'a> Drop for StateBackupGuard<'a> {
    fn drop(&mut self) {
        for (var_name, old_val) in &self.backups {
            if let Some(val) = old_val {
                let _ = self.state.set_var(var_name.as_ref(), val.as_ref());
            } else {
                let _ = self.state.unset_var(var_name.as_ref());
            }
        }
    }
}

/// Represents a variable call stack frame (e.g. inside a shell function invocation).
#[derive(Clone)]
pub struct Frame {
    /// Variables declared as local within this function frame.
    pub local_vars: FlatMap<BString, BString>,
    /// Positional parameters (`$1`, `$2`, etc.) active within this frame.
    pub args: Vec<BString>,
}

/// An entry in the cached resolved command table (hash table).
#[derive(Clone)]
pub struct HashEntry {
    /// Absolute filesystem path to the resolved binary.
    pub path: BString,
    /// Number of times this cache entry has been hit.
    pub hits: u32,
}

/// Resource limit constant for maximum file size (`ulimit -f`).
pub const RLIMIT_FSIZE: i32 = 1;
/// Resource limit constant for maximum open file descriptors (`ulimit -n`).
pub const RLIMIT_NOFILE: i32 = 2;
/// Resource limit constant for maximum core dump size (`ulimit -c`).
pub const RLIMIT_CORE: i32 = 3;
/// Represents an unlimited resource limit threshold.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// Maintains the global runtime state of the shell session.
pub struct ShellState {
    /// Table of global shell variables (`name -> value`).
    vars: FlatMap<BString, BString>,
    /// Set of variable names that are marked for export to child processes.
    exported: FlatSet<BString>,
    /// Table of shell function definitions (`name -> serialized_ast_bytes`).
    functions: FlatMap<BString, Vec<u8>>,
    /// Stack of execution frames for local variable scoping and positional arguments.
    pub frames: Vec<Frame>,
    /// Table of command aliases (`name -> expansion`).
    pub aliases: FlatMap<BString, BString>,
    /// File creation permission mask (`umask`).
    umask: u32,
    /// Set of read-only variable names that cannot be modified or unset.
    readonly: FlatSet<BString>,
    /// Cache of resolved binary command paths (`command_name -> HashEntry`).
    command_cache: FlatMap<BString, HashEntry>,
    /// Table of configured resource limits (`resource_id -> value`).
    rlimits: FlatMap<i32, u64>,
    /// The name of the script currently executing (`$0`).
    pub script_name: BString,
    /// Global positional parameters (`$1`, `$2`, etc.).
    pub args: Vec<BString>,
    /// Exit immediately if a command exits with a non-zero status (`set -e`).
    pub opt_errexit: bool,
    /// Print commands and arguments when they are executed (`set -x`).
    pub opt_xtrace: bool,
    /// Treat unset variables as an error during parameter expansion (`set -u`).
    pub opt_nounset: bool,
    /// Disable pathname expansion / globbing (`set -f`).
    pub opt_noglob: bool,
    /// Mark variables which are modified or created for export (`set -a`).
    pub opt_allexport: bool,
    /// True if the shell is running interactively (`set -i` or detected TTY).
    pub opt_interactive: bool,
    /// Prevent overwriting existing files via output redirection (`set -C`).
    pub opt_noclobber: bool,
    /// Read commands but do not execute them (`set -n`).
    pub opt_noexec: bool,
    /// Print shell input lines as they are read (`set -v`).
    pub opt_verbose: bool,
    /// Do not exit interactive shell upon reading EOF (`set -I`).
    pub opt_ignoreeof: bool,
    /// Counter tracking nesting depth inside conditionals/loops where `errexit` is ignored.
    pub ignore_err_depth: usize,
    /// Table of signal traps (`signal_name -> action_command`).
    pub traps: FlatMap<BString, BString>,
    /// List of active background jobs spawned by the shell.
    pub bg_jobs: Vec<BgJob>,
    /// Process ID of the most recently executed background command (`$!`).
    pub last_bg_pid: Option<u64>,
}

impl ShellState {
    fn options_string(&self) -> BString {
        let mut s = Vec::new();
        if self.opt_errexit {
            s.push(b'e');
        }
        if self.opt_xtrace {
            s.push(b'x');
        }
        if self.opt_nounset {
            s.push(b'u');
        }
        if self.opt_noglob {
            s.push(b'f');
        }
        if self.opt_allexport {
            s.push(b'a');
        }
        if self.opt_interactive {
            s.push(b'i');
        }
        if self.opt_noclobber {
            s.push(b'C');
        }
        if self.opt_noexec {
            s.push(b'n');
        }
        if self.opt_verbose {
            s.push(b'v');
        }
        if self.opt_ignoreeof {
            s.push(b'I');
        }
        BString::from(s)
    }

    fn set_options_from_string(&mut self, s: &BStr) {
        self.opt_errexit = false;
        self.opt_xtrace = false;
        self.opt_nounset = false;
        self.opt_noglob = false;
        self.opt_allexport = false;
        self.opt_interactive = false;
        self.opt_noclobber = false;
        self.opt_noexec = false;
        self.opt_verbose = false;
        self.opt_ignoreeof = false;
        for &c in s.as_bytes() {
            match c {
                b'e' => self.opt_errexit = true,
                b'x' => self.opt_xtrace = true,
                b'u' => self.opt_nounset = true,
                b'f' => self.opt_noglob = true,
                b'a' => self.opt_allexport = true,
                b'i' => self.opt_interactive = true,
                b'C' => self.opt_noclobber = true,
                b'n' => self.opt_noexec = true,
                b'v' => self.opt_verbose = true,
                b'I' => self.opt_ignoreeof = true,
                _ => {}
            }
        }
    }
}

impl Serialize for ShellState {
    fn serialize_into(&self, buf: &mut Vec<u8>) {
        self.vars.serialize_into(buf);
        self.exported.serialize_into(buf);
        self.functions.serialize_into(buf);
        self.aliases.serialize_into(buf);
        self.readonly.serialize_into(buf);
        self.umask.serialize_into(buf);
        self.rlimits.serialize_into(buf);
        self.script_name.serialize_into(buf);
        self.args.serialize_into(buf);
        self.options_string().serialize_into(buf);
        self.traps.serialize_into(buf);
    }
}

impl Deserialize for ShellState {
    fn deserialize(bytes: &[u8], offset: &mut usize) -> Result<Self, String> {
        let vars = FlatMap::deserialize(bytes, offset)?;
        let exported = FlatSet::deserialize(bytes, offset)?;
        let functions = FlatMap::deserialize(bytes, offset)?;
        let aliases = FlatMap::deserialize(bytes, offset)?;
        let readonly = FlatSet::deserialize(bytes, offset)?;
        let umask = u32::deserialize(bytes, offset)?;
        let rlimits = FlatMap::deserialize(bytes, offset)?;
        let script_name = BString::deserialize(bytes, offset)?;
        let args = Vec::<BString>::deserialize(bytes, offset)?;
        let options_str = BString::deserialize(bytes, offset)?;
        let traps = FlatMap::deserialize(bytes, offset)?;

        let mut state = ShellState {
            vars,
            exported,
            functions,
            frames: Vec::new(),
            aliases,
            umask,
            readonly,
            command_cache: FlatMap::new(),
            rlimits,
            script_name,
            args,
            opt_errexit: false,
            opt_xtrace: false,
            opt_nounset: false,
            opt_noglob: false,
            opt_allexport: false,
            opt_interactive: false,
            opt_noclobber: false,
            opt_noexec: false,
            opt_verbose: false,
            opt_ignoreeof: false,
            ignore_err_depth: 0,
            traps,
            bg_jobs: Vec::new(),
            last_bg_pid: None,
        };
        state.set_options_from_string(options_str.as_ref());

        Ok(state)
    }
}

fn assert_valid_name(name: &BStr) {
    assert!(!name.contains(&b'='), "name cannot contain '=': {:?}", name);
}

impl ShellState {
    /// Returns a map of environment variables inherited from the host process.
    pub fn inherited_vars() -> FlatMap<BString, BString> {
        let mut vars = FlatMap::new();
        for (k, v) in std::env::vars() {
            vars.insert(BString::from(k), BString::from(v));
        }
        vars
    }

    /// Creates a new `ShellState` with default arguments and an empty environment for testing.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_args(Args::default(), FlatMap::new()).unwrap()
    }

    /// Initializes a new `ShellState` using parsed command line arguments and initial host
    /// variables.
    pub fn with_args(args: Args, initial_vars: FlatMap<BString, BString>) -> Result<Self, String> {
        let mut vars = initial_vars;
        let mut exported = FlatSet::new();
        for (k, _) in vars.iter() {
            assert_valid_name(k.as_bstr());
            exported.insert(k.clone());
        }
        vars.insert(BString::from("?"), BString::from("0"));

        let script_name = args.script_name.unwrap_or_else(|| BString::from(DEFAULT_SHELL_NAME));

        let mut state = Self {
            vars,
            exported,
            functions: FlatMap::new(),
            frames: Vec::new(),
            aliases: FlatMap::new(),
            umask: 0o022,
            readonly: FlatSet::new(),
            command_cache: FlatMap::new(),
            rlimits: FlatMap::from(vec![
                (RLIMIT_FSIZE, RLIM_INFINITY),
                (RLIMIT_NOFILE, 1024),
                (RLIMIT_CORE, 0),
            ]),
            script_name,
            args: args.positional_args,
            opt_errexit: args.opt_errexit.unwrap_or(false),
            opt_xtrace: args.opt_xtrace.unwrap_or(false),
            opt_nounset: args.opt_nounset.unwrap_or(false),
            opt_noglob: args.opt_noglob.unwrap_or(false),
            opt_allexport: args.opt_allexport.unwrap_or(false),
            opt_interactive: args.opt_interactive,
            opt_noclobber: args.opt_noclobber.unwrap_or(false),
            opt_noexec: args.opt_noexec.unwrap_or(false),
            opt_verbose: args.opt_verbose.unwrap_or(false),
            opt_ignoreeof: args.opt_ignoreeof.unwrap_or(false),
            ignore_err_depth: 0,
            traps: FlatMap::new(),
            bg_jobs: Vec::new(),
            last_bg_pid: None,
        };

        for opt in &args.options_to_set {
            state.set_option_by_name(opt.as_bstr(), true)?;
        }
        for opt in &args.options_to_clear {
            state.set_option_by_name(opt.as_bstr(), false)?;
        }

        Ok(state)
    }

    fn active_args(&self) -> &[BString] {
        if let Some(frame) = self.frames.last() { &frame.args } else { &self.args }
    }

    /// Replaces positional parameters (`$1`, `$2`, etc.) in the active call frame or global scope.
    pub fn set_args(&mut self, new_args: Vec<BString>) {
        if let Some(frame) = self.frames.last_mut() {
            frame.args = new_args;
        } else {
            self.args = new_args;
        }
    }

    /// Returns a copy of the currently active positional parameters (`$1`, `$2`, etc.).
    pub fn get_args(&self) -> Vec<BString> {
        self.active_args().to_vec()
    }

    /// Processes an evaluation outcome, intercepting non-zero errors to trigger immediate exit if
    /// `errexit` (`set -e`) is active.
    pub fn handle_outcome(&self, outcome: EvalOutcome) -> EvalOutcome {
        match outcome {
            EvalOutcome::Code(code)
                if code != 0 && self.opt_errexit && self.ignore_err_depth == 0 =>
            {
                EvalOutcome::Exit(code)
            }
            o => o,
        }
    }

    /// Looks up a variable or special parameter value by name (e.g. `$?`, `$#`, `$1`, `$-`,
    /// `$VAR`).
    pub fn get_var(&self, name: &BStr) -> Option<BString> {
        if let Some(idx) = parse_int::<usize>(name) {
            if idx == 0 {
                return Some(self.script_name.clone());
            } else {
                let current_args = self.active_args();
                if idx <= current_args.len() {
                    return Some(current_args[idx - 1].clone());
                } else {
                    return Some(BString::default());
                }
            }
        }

        match name.as_bytes() {
            b"#" => {
                let current_args = self.active_args();
                return Some(BString::from(current_args.len().to_string()));
            }
            b"?" => return self.vars.get(BStr::new(b"?")).cloned(),
            b"@" | b"*" => {
                let current_args = self.active_args();
                let mut joined = Vec::new();
                for (i, arg) in current_args.iter().enumerate() {
                    if i > 0 {
                        joined.push(b' ');
                    }
                    joined.extend_from_slice(arg.as_bytes());
                }
                return Some(BString::from(joined));
            }
            b"$" => {
                let koid = fuchsia_runtime::process_self().koid().unwrap().raw_koid();
                return Some(BString::from(koid.to_string()));
            }
            b"!" => {
                if let Some(pid) = self.last_bg_pid {
                    return Some(BString::from(pid.to_string()));
                }
                return Some(BString::default());
            }
            b"-" => {
                let mut opts = String::new();
                if self.opt_errexit {
                    opts.push('e');
                }
                if self.opt_xtrace {
                    opts.push('x');
                }
                if self.opt_nounset {
                    opts.push('u');
                }
                if self.opt_noglob {
                    opts.push('f');
                }
                if self.opt_allexport {
                    opts.push('a');
                }
                if self.opt_interactive {
                    opts.push('i');
                }
                if self.opt_noclobber {
                    opts.push('C');
                }
                if self.opt_noexec {
                    opts.push('n');
                }
                if self.opt_verbose {
                    opts.push('v');
                }
                return Some(BString::from(opts));
            }
            _ => {}
        }

        for frame in self.frames.iter().rev() {
            if let Some(val) = frame.local_vars.get(name) {
                return Some(val.clone());
            }
        }

        self.vars.get(name).cloned()
    }

    /// Returns `true` if the specified variable is marked read-only.
    pub fn is_readonly(&self, name: &BStr) -> bool {
        self.readonly.contains(name)
    }

    /// Returns a reference to the set of read-only variable names.
    pub fn readonly(&self) -> &FlatSet<BString> {
        &self.readonly
    }

    /// Marks the specified variable as read-only.
    pub fn make_readonly(&mut self, name: &BStr) {
        assert_valid_name(name);
        self.readonly.insert(name.to_owned());
    }

    /// Sets the value of a variable in the innermost active local frame or global scope.
    pub fn set_var(&mut self, name: &BStr, val: &BStr) {
        assert_valid_name(name);
        if self.is_readonly(name) {
            let _ = writeln!(std::io::stderr(), "zxsh: {}: readonly variable", name);
            return;
        }
        for frame in self.frames.iter_mut().rev() {
            if frame.local_vars.contains_key(name) {
                frame.local_vars.insert(name.to_owned(), val.to_owned());
                return;
            }
        }
        self.vars.insert(name.to_owned(), val.to_owned());
        if self.opt_allexport {
            self.export_var(name);
        }
    }

    /// Marks the specified variable for export to child environment processes.
    pub fn export_var(&mut self, name: &BStr) {
        assert_valid_name(name);
        self.exported.insert(name.to_owned());
    }

    /// Removes a variable from the innermost active local frame or global scope.
    pub fn unset_var(&mut self, name: &BStr) {
        assert_valid_name(name);
        if self.is_readonly(name) {
            let _ = writeln!(std::io::stderr(), "zxsh: {}: readonly variable", name);
            return;
        }
        for frame in self.frames.iter_mut().rev() {
            if frame.local_vars.contains_key(name) {
                frame.local_vars.remove(name);
                return;
            }
        }
        self.vars.remove(name);
        self.exported.remove(name);
    }

    /// Registers a shell function definition with its serialized AST body bytes.
    pub fn add_function(&mut self, name: BString, body_bytes: Vec<u8>) {
        assert_valid_name(name.as_bstr());
        self.functions.insert(name, body_bytes);
    }

    /// Removes a shell function definition by name, returning its serialized AST body if present.
    pub fn remove_function(&mut self, name: &BStr) -> Option<Vec<u8>> {
        self.functions.remove(name)
    }

    /// Retrieves a reference to the serialized AST body bytes of a registered shell function.
    pub fn get_function(&self, name: &BStr) -> Option<&Vec<u8>> {
        self.functions.get(name)
    }

    /// Returns the current file creation permission mask (`umask`).
    pub fn umask(&self) -> u32 {
        self.umask
    }

    /// Sets the current file creation permission mask (`umask`).
    pub fn set_umask(&mut self, umask: u32) {
        self.umask = umask;
    }

    /// Enables or disables a shell option by its long name (e.g. `errexit`, `xtrace`).
    pub fn set_option_by_name(&mut self, name: &BStr, enable: bool) -> Result<(), String> {
        match name.as_bytes() {
            b"allexport" => self.opt_allexport = enable,
            b"noclobber" => self.opt_noclobber = enable,
            b"errexit" => self.opt_errexit = enable,
            b"noglob" => self.opt_noglob = enable,
            b"noexec" => self.opt_noexec = enable,
            b"xtrace" => self.opt_xtrace = enable,
            b"verbose" => self.opt_verbose = enable,
            b"nounset" => self.opt_nounset = enable,
            b"interactive" => self.opt_interactive = enable,
            b"ignoreeof" => self.opt_ignoreeof = enable,
            b"monitor" | b"notify" | b"nolog" | b"debug" | b"vi" | b"emacs" => {}
            _ => return Err(format!("unknown option: {}", name)),
        }
        Ok(())
    }

    /// Returns a shared reference to the cached resolved binary paths table.
    pub fn command_cache(&self) -> &FlatMap<BString, HashEntry> {
        &self.command_cache
    }

    /// Clears all entries from the cached binary path hash table (`hash -r`).
    pub fn clear_command_cache(&mut self) {
        self.command_cache.clear();
    }

    /// Inserts or updates an entry in the cached binary path table (`hash -p`).
    pub fn insert_command_cache(&mut self, name: BString, path: BString, hits: u32) {
        self.command_cache.insert(name, HashEntry { path, hits });
    }

    /// Resolves a command binary name to an absolute filesystem path using the cache and `$PATH`.
    pub fn resolve_command_path(&mut self, cmd: &BStr) -> Option<BString> {
        if cmd.ends_with(b"/") || cmd.find(b"/").is_some() {
            return Some(cmd.to_owned());
        }

        if let Some(entry) = self.command_cache.get_mut(cmd) {
            if let Ok(entry_path) = entry.path.to_path() {
                if entry_path.exists() {
                    entry.hits += 1;
                    return Some(entry.path.clone());
                }
            }
        }

        // Cache miss or deleted file
        self.command_cache.remove(cmd);
        if let Some(resolved) = self.path().resolve(cmd) {
            self.command_cache
                .insert(cmd.to_owned(), HashEntry { path: resolved.clone(), hits: 1 });
            return Some(resolved);
        }
        None
    }

    /// Returns the configured threshold for the specified resource limit ID.
    pub fn get_rlimit(&self, resource: i32) -> Option<u64> {
        self.rlimits.get(&resource).copied()
    }

    /// Configures the threshold for the specified resource limit ID.
    pub fn set_rlimit(&mut self, resource: i32, value: u64) {
        self.rlimits.insert(resource, value);
    }

    /// Returns `true` if execution is currently inside a function call stack frame.
    pub fn is_in_function(&self) -> bool {
        !self.frames.is_empty()
    }

    /// Declares or updates a local variable within the active function frame (`local VAR=val`).
    pub fn declare_local(&mut self, name: &BStr, val: Option<&BStr>) {
        assert_valid_name(name);
        if let Some(frame) = self.frames.last_mut() {
            frame
                .local_vars
                .insert(name.to_owned(), val.unwrap_or_else(|| BStr::new(b"")).to_owned());
        }
    }

    /// Returns the exported environment variables as a `ShellEnv`.
    pub fn vars(&self) -> ShellEnv {
        let mut res = Vec::new();
        for k in self.exported.iter() {
            if let Some(v) = self.vars.get(k) {
                res.push((k.clone(), v.clone()));
            }
        }
        ShellEnv::new(res)
    }

    /// Returns a reference to the table of all global shell variables.
    pub fn all_vars(&self) -> &FlatMap<BString, BString> {
        &self.vars
    }

    /// Parses and returns the current `$PATH` environment variable as a `ShellPath` wrapper.
    pub fn path(&self) -> ShellPath {
        self.get_var(BStr::new(b"PATH")).map(ShellPath::new).unwrap_or_default()
    }
}

/// Helper structure wrapping a parsed `$PATH` variable for binary lookup.
pub struct ShellPath {
    /// Raw string value of the `PATH` environment variable.
    path_val: BString,
}

impl ShellPath {
    /// Creates a new `ShellPath` wrapper around the provided path string.
    pub fn new(path_val: BString) -> Self {
        Self { path_val }
    }

    /// Returns an iterator over the colon-separated directory paths in `$PATH`.
    pub fn entries(&self) -> impl Iterator<Item = &BStr> {
        self.path_val.as_bstr().split_byte(b':')
    }

    /// Searches the `$PATH` directory entries to resolve a binary command name into an absolute
    /// path.
    pub fn resolve(&self, command: &BStr) -> Option<BString> {
        if command.contains(&b'/') {
            let path = match command.to_path() {
                Ok(p) => p,
                Err(_) => return None,
            };
            if path.exists() {
                return Some(BString::from(command));
            } else {
                return None;
            }
        }
        for dir in self.entries() {
            let dir_path = match dir.to_path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cmd_path = match command.to_path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let filepath = dir_path.join(cmd_path);
            if filepath.exists() {
                return Some(BString::from(filepath.as_os_str().as_bytes()));
            }
        }
        None
    }
}

impl Default for ShellPath {
    fn default() -> Self {
        Self::new(BString::from(DEFAULT_PATH))
    }
}

/// Represents a collection of exported shell environment variables.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ShellEnv {
    vars: Vec<(BString, BString)>,
}

impl ShellEnv {
    /// Creates a new `ShellEnv` from a vector of key-value pairs.
    pub fn new(vars: Vec<(BString, BString)>) -> Self {
        for (k, _) in &vars {
            assert_valid_name(k.as_bstr());
        }
        Self { vars }
    }

    /// Returns a slice of the key-value pairs in this environment.
    pub fn as_slice(&self) -> &[(BString, BString)] {
        &self.vars
    }

    /// Returns a mutable slice of the key-value pairs in this environment.
    pub fn as_mut_slice(&mut self) -> &mut [(BString, BString)] {
        &mut self.vars
    }

    /// Returns an iterator over the key-value pairs in this environment.
    pub fn iter(&self) -> impl Iterator<Item = &(BString, BString)> {
        self.vars.iter()
    }

    /// Returns the underlying vector of key-value pairs.
    pub fn into_vec(self) -> Vec<(BString, BString)> {
        self.vars
    }

    /// Returns the `ShellPath` representing the `PATH` environment variable in this environment.
    pub fn path(&self) -> ShellPath {
        self.vars
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| ShellPath::new(v.clone()))
            .unwrap_or_default()
    }

    /// Converts this environment into a vector of `CString`s formatted as `KEY=VAL`,
    /// suitable for use with `fdio::spawn_etc`.
    pub fn to_spawn_env(&self) -> Result<Vec<CString>, NulError> {
        let mut env_cstrs = Vec::with_capacity(self.vars.len());
        for (k, v) in &self.vars {
            let mut env_bytes = Vec::with_capacity(k.len() + 1 + v.len());
            env_bytes.extend_from_slice(k.as_bytes());
            env_bytes.push(b'=');
            env_bytes.extend_from_slice(v.as_bytes());
            env_cstrs.push(crate::string::bstr_to_cstring(&env_bytes)?);
        }
        Ok(env_cstrs)
    }
}

impl IntoIterator for ShellEnv {
    type Item = (BString, BString);
    type IntoIter = std::vec::IntoIter<(BString, BString)>;

    fn into_iter(self) -> Self::IntoIter {
        self.vars.into_iter()
    }
}

impl<'a> IntoIterator for &'a ShellEnv {
    type Item = &'a (BString, BString);
    type IntoIter = std::slice::Iter<'a, (BString, BString)>;

    fn into_iter(self) -> Self::IntoIter {
        self.vars.iter()
    }
}

impl<'a> IntoIterator for &'a mut ShellEnv {
    type Item = &'a mut (BString, BString);
    type IntoIter = std::slice::IterMut<'a, (BString, BString)>;

    fn into_iter(self) -> Self::IntoIter {
        self.vars.iter_mut()
    }
}

impl From<Vec<(BString, BString)>> for ShellEnv {
    fn from(vars: Vec<(BString, BString)>) -> Self {
        Self::new(vars)
    }
}

impl FromIterator<(BString, BString)> for ShellEnv {
    fn from_iter<T: IntoIterator<Item = (BString, BString)>>(iter: T) -> Self {
        Self::new(iter.into_iter().collect())
    }
}
