//! Reads changes and file contents from a git repository.

use crate::analyze::{CppFinder, name_similarity};
use crate::cpp::DeclComments;
use crate::extract::{self, attach_decl_comments};
use crate::input::{ChangeSet, Version};
use crate::model::{Function, Lang};
use crate::{layout, normalize, patch};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Git {
    pub dir: PathBuf,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Git {
        Git { dir: dir.into() }
    }

    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn is_repo(&self) -> bool {
        self.run(&["rev-parse", "--git-dir"]).is_ok()
    }

    /// Contents of `path` at `rev`.
    pub fn show(&self, rev: &str, path: &str) -> Option<String> {
        self.run(&["show", &format!("{rev}:{path}")]).ok()
    }

    /// Contents of a blob by (possibly abbreviated) id.
    pub fn blob(&self, id: &str) -> Option<String> {
        self.run(&["cat-file", "-p", id]).ok()
    }

    /// Splits `A..B` into `(A, B)`, and `R` into `(R^, R)`.
    pub fn range(spec: &str) -> (String, String) {
        match spec.split_once("..") {
            Some((a, b)) => (
                if a.is_empty() { "HEAD".into() } else { a.into() },
                if b.is_empty() { "HEAD".into() } else { b.trim_start_matches('.').into() },
            ),
            None => (format!("{spec}^"), spec.to_string()),
        }
    }

    /// The files changed between two revisions, with full contents.
    pub fn changeset(&self, base: &str, head: &str) -> Result<ChangeSet, String> {
        let diff = self.run(&["diff", "--no-color", "--no-ext-diff", "-U0", "-M", base, head])?;
        let mut cs = ChangeSet::default();
        for f in patch::parse(&diff) {
            let Some(lang) = Lang::from_path(f.path()) else {
                continue;
            };
            if lang == Lang::Cpp {
                if let Some(p) = &f.old_path {
                    if let Some(text) = self.show(base, p) {
                        cs.cpp_old.push(Version {
                            path: p.clone(),
                            text,
                            changed: Some(f.removed()),
                        });
                    }
                }
            }
            if let Some(p) = &f.new_path {
                if let Some(text) = self.show(head, p) {
                    let v = Version { path: p.clone(), text, changed: Some(f.added()) };
                    match lang {
                        Lang::Cpp => cs.cpp_new.push(v),
                        Lang::Rust => cs.rust_new.push(v),
                    }
                }
            }
        }
        self.load_companion_headers(&mut cs, head);
        Ok(cs)
    }

    /// Loads unchanged companion headers included by modified C++ source
    /// files so `extern-signature` can verify `cpp_*` header prototypes.
    pub fn load_companion_headers(&self, cs: &mut ChangeSet, rev: &str) {
        let include_re = regex::Regex::new(r#"(?m)^\s*#\s*include\s*[<"]([^>"]+)[>"]"#).unwrap();
        let mut extra_headers = Vec::new();
        for v in &cs.cpp_new {
            if !(v.path.ends_with(".cc") || v.path.ends_with(".cpp")) {
                continue;
            }
            let want = stem(&v.path);
            for cap in include_re.captures_iter(&v.text) {
                let inc = &cap[1];
                if stem(inc) != want {
                    continue;
                }
                let mut dir = v.path.as_str();
                while let Some((parent, _)) = dir.rsplit_once('/') {
                    dir = parent;
                    for cand in [format!("{dir}/include/{inc}"), format!("{dir}/{inc}")] {
                        if cs.cpp_new.iter().any(|x| x.path == cand)
                            || extra_headers.iter().any(|x: &Version| x.path == cand)
                        {
                            continue;
                        }
                        if let Some(text) = self.show(rev, &cand) {
                            extra_headers.push(Version {
                                path: cand,
                                text,
                                changed: Some(std::collections::BTreeSet::new()),
                            });
                            break;
                        }
                    }
                }
            }
        }
        cs.cpp_new.extend(extra_headers);
    }
}

/// Finds C++ that a change did not touch, for Rust functions that have no
/// removed C++ to pair with. Looks first at C++ files named like the Rust
/// file, then greps the tree for the function's name in CamelCase and
/// snake_case.
pub struct RepoFinder {
    git: Git,
    rev: String,
    files: Option<Vec<String>>,
    parsed: HashMap<String, (Vec<Function>, DeclComments)>,
    /// Maximum number of files to parse per lookup.
    pub max_files: usize,
}

impl RepoFinder {
    pub fn new(git: Git, rev: impl Into<String>) -> RepoFinder {
        RepoFinder { git, rev: rev.into(), files: None, parsed: HashMap::new(), max_files: 24 }
    }

    fn cpp_files(&mut self) -> &[String] {
        if self.files.is_none() {
            let list = self
                .git
                .run(&["ls-tree", "-r", "--name-only", &self.rev])
                .unwrap_or_default()
                .lines()
                .filter(|p| Lang::from_path(p) == Some(Lang::Cpp))
                .map(str::to_string)
                .collect();
            self.files = Some(list);
        }
        self.files.as_deref().unwrap_or(&[])
    }

    fn parse(&mut self, path: &str) {
        if self.parsed.contains_key(path) {
            return;
        }
        let entry = match self.git.show(&self.rev, path) {
            Some(text) => {
                let e = extract::extract(Lang::Cpp, path, &text);
                (e.functions, e.decl_comments)
            }
            None => (Vec::new(), DeclComments::new()),
        };
        self.parsed.insert(path.to_string(), entry);
    }
}

fn camel(snake: &str) -> String {
    normalize::words(snake)
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let s = base.split('.').next().unwrap_or(base);
    s.strip_suffix("_ffi").unwrap_or(s).to_string()
}

impl CppFinder for RepoFinder {
    fn find(&mut self, rust: &Function) -> Vec<Function> {
        if rust.base.starts_with("test_") || rust.base == "drop" || rust.base == "fmt" {
            // Tests and trait plumbing have no C++ counterpart to find.
            return Vec::new();
        }
        let want = stem(&rust.path);
        let mut paths: Vec<String> =
            self.cpp_files().iter().filter(|p| stem(p) == want).cloned().collect();
        let camel_name = camel(&rust.base);
        let mut args = vec!["grep".to_string(), "-l".into(), "-w".into(), "-F".into()];
        for n in [camel_name.as_str(), rust.base.as_str()] {
            if n.len() >= 4 {
                args.push("-e".into());
                args.push(n.to_string());
            }
        }
        if args.len() > 4 {
            args.push(self.rev.clone());
            args.push("--".into());
            // Search the Rust file's top-level directory (e.g. `zircon/`),
            // which keeps grep fast in a tree the size of Fuchsia's.
            let top = rust.path.split_once('/').map(|(t, _)| format!("{t}/")).unwrap_or_default();
            for ext in ["cc", "cpp", "h", "hpp"] {
                args.push(format!(":(glob){top}**/*.{ext}"));
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Ok(out) = self.git.run(&argv) {
                let prefix = format!("{}:", self.rev);
                for l in out.lines() {
                    let p = l.strip_prefix(&prefix).unwrap_or(l).to_string();
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
        }
        // Never another architecture's C++, and files where the Rust's C++
        // is expected first.
        paths.retain(|p| !layout::arch_conflict(p, &rust.path));
        paths.sort_by_key(|p| (!layout::related(p, &rust.path), stem(p) != want, p.len()));
        paths.truncate(self.max_files);

        let mut decls = DeclComments::new();
        let mut out = Vec::new();
        for p in &paths {
            self.parse(p);
            let (fs, ds) = &self.parsed[p];
            for (k, v) in ds {
                decls.entry(k.clone()).or_insert_with(|| v.clone());
            }
            out.extend(fs.iter().filter(|f| name_similarity(f, rust) >= 0.6).cloned());
        }
        attach_decl_comments(&mut out, &decls, &Default::default());
        out
    }

    fn still_present(&mut self, lines: &[String], changed: &[String]) -> HashSet<String> {
        let mut found = HashSet::new();
        // Search the top-level directory of the changed files, as `find`
        // does.
        let top = changed
            .first()
            .and_then(|p| p.split_once('/'))
            .map(|(t, _)| format!("{t}/"))
            .unwrap_or_default();
        let prefix = format!("{}:", self.rev);
        for chunk in lines.chunks(200) {
            let mut args: Vec<String> = vec!["grep".into(), "-F".into(), "-I".into()];
            for l in chunk {
                args.push("-e".into());
                args.push(l.clone());
            }
            args.push(self.rev.clone());
            args.push("--".into());
            for ext in ["cc", "cpp", "h", "hpp"] {
                args.push(format!(":(glob){top}**/*.{ext}"));
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let Ok(out) = self.git.run(&argv) else {
                continue;
            };
            for l in out.lines() {
                let rest = l.strip_prefix(&prefix).unwrap_or(l);
                let Some((path, text)) = rest.split_once(':') else {
                    continue;
                };
                if changed.iter().any(|c| c == path) {
                    continue;
                }
                for want in chunk {
                    if text.contains(want.as_str()) {
                        found.insert(want.clone());
                    }
                }
            }
        }
        found
    }
}
