use babeldiff::analyze::{self, CppFinder, NoFinder};
use babeldiff::git::{Git, RepoFinder};
use babeldiff::html;
use babeldiff::input::ChangeSet;
use babeldiff::render::{self, Layout, RenderOptions};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

/// Line up C++ removed by a change with the Rust that replaces it, and check
/// that comments, error returns, locks, calls and control flow correspond.
///
/// Exit status is 0 when no issues are found, 1 when there are issues, and 2
/// on error.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Download the latest patchset of a change from fuchsia-review.googlesource.com and compare it.
    #[arg(long = "cl", visible_aliases = ["review", "gerrit"], value_name = "CL", global = true)]
    cl: Option<String>,

    /// Git repository directory (defaults to "." for `git` and `cl` modes).
    #[arg(short = 'C', long, value_name = "DIR", global = true)]
    repo: Option<PathBuf>,

    /// Output format: plain text, a self-contained HTML page for people, or
    /// JSON for agents and scripts.
    #[arg(long, value_enum, default_value_t = FormatArg::Text, global = true)]
    format: FormatArg,

    /// Write the report to FILE instead of standard output.
    #[arg(long, short = 'o', value_name = "FILE", global = true)]
    output: Option<PathBuf>,

    /// Output layout for the text format.
    #[arg(long, value_enum, default_value_t = LayoutArg::SideBySide, global = true)]
    layout: LayoutArg,

    /// Total output width for the side-by-side layout (default: $COLUMNS or 160).
    #[arg(long, global = true)]
    width: Option<usize>,

    /// Only show rows within N rows of a difference.
    #[arg(long, short = 'U', global = true)]
    context: Option<usize>,

    /// Print only the per-function summaries and findings.
    #[arg(long, global = true)]
    summary: bool,

    /// Leave out notes, and pairs with no issues.
    #[arg(long, global = true)]
    issues_only: bool,

    /// Force a pairing, as CPP_NAME=RUST_NAME (qualified or unqualified). Repeatable.
    #[arg(long = "pair", value_name = "CPP=RUST", global = true)]
    pairs: Vec<String>,

    /// Minimum fraction of a function's lines the change must touch for the
    /// function to be compared.
    #[arg(long, default_value_t = babeldiff::DEFAULT_MIN_CHANGED, global = true)]
    min_changed: f64,

    /// Don't search the repository for unchanged C++ that corresponds to
    /// otherwise unpaired Rust.
    #[arg(long, global = true)]
    no_search: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compare a git commit (REV^..REV) or range (A..B). Defaults to HEAD.
    Git {
        #[arg(default_value = "HEAD")]
        rev: String,
    },
    /// Compare a unified diff or `git format-patch` file ("-" or omitted for stdin).
    Patch {
        file: Option<PathBuf>,
        /// Revision to search for unchanged C++ (the patch's parent).
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Download the latest patchset of a change from fuchsia-review.googlesource.com and compare it.
    #[command(visible_aliases = ["review", "gerrit"])]
    Cl {
        /// Fuchsia review number (e.g. 1885120, 1885120/5, fxr/1885120, or a Gerrit URL).
        cl: String,
    },
    /// Compare every function in the given C++ and Rust files.
    Files {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum FormatArg {
    Text,
    Html,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    SideBySide,
    Stacked,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(issues) => {
            if issues > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("babeldiff: {e}");
            ExitCode::from(2)
        }
    }
}

enum Op {
    Git { rev: String, repo: PathBuf },
    Patch { file: Option<PathBuf>, repo: Option<PathBuf>, base: String },
    Cl { cl: String, repo: PathBuf },
    Files { files: Vec<PathBuf> },
}

fn run(cli: &Cli) -> Result<usize, String> {
    let mut forced = Vec::new();
    for p in &cli.pairs {
        let (c, r) =
            p.split_once('=').ok_or_else(|| format!("--pair expects CPP=RUST, got {p:?}"))?;
        forced.push((c.to_string(), r.to_string()));
    }
    let repo_or_dot = || cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let op = if let Some(cl) = &cli.cl {
        Op::Cl { cl: cl.clone(), repo: repo_or_dot() }
    } else {
        match &cli.command {
            Some(Cmd::Cl { cl }) => Op::Cl { cl: cl.clone(), repo: repo_or_dot() },
            Some(Cmd::Git { rev }) => Op::Git { rev: rev.clone(), repo: repo_or_dot() },
            Some(Cmd::Patch { file, base }) => {
                Op::Patch { file: file.clone(), repo: cli.repo.clone(), base: base.clone() }
            }
            Some(Cmd::Files { files }) => Op::Files { files: files.clone() },
            None => {
                return Err(
                    "a subcommand (git, patch, cl, files) or --cl <CL> is required".to_string()
                );
            }
        }
    };
    let (cs, mut finder, title): (ChangeSet, Box<dyn CppFinder>, String) = match op {
        Op::Git { rev, repo } => {
            let git = Git::new(repo);
            let (base, head) = Git::range(&rev);
            let cs = git.changeset(&base, &head)?;
            let title = git
                .run(&["log", "-1", "--format=%h %s", &head])
                .map(|s| s.trim().to_string())
                .unwrap_or(rev);
            let finder: Box<dyn CppFinder> = if cli.no_search {
                Box::new(NoFinder)
            } else {
                Box::new(RepoFinder::new(git, base))
            };
            (cs, finder, title)
        }
        Op::Patch { file, repo, base } => {
            let text = match file.as_deref() {
                None => read_stdin()?,
                Some(p) if p.as_os_str() == "-" => read_stdin()?,
                Some(p) => {
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?
                }
            };
            let files = babeldiff::patch::parse(&text);
            let title = patch_title(&text).unwrap_or_else(|| match file {
                Some(p) => p.display().to_string(),
                None => "patch from standard input".into(),
            });
            let git = repo.as_ref().map(Git::new).filter(Git::is_repo);
            let cs = match &git {
                Some(g) => ChangeSet::from_patch(&files, &mut |id| g.blob(id)),
                None => ChangeSet::from_patch(&files, &mut |_| None),
            };
            let finder: Box<dyn CppFinder> = match git {
                Some(g) if !cli.no_search => Box::new(RepoFinder::new(g, base)),
                _ => Box::new(NoFinder),
            };
            (cs, finder, title)
        }
        Op::Cl { cl, repo } => {
            let (change_num, patchset) = babeldiff::cl::parse_cl_spec(&cl)?;
            let url = babeldiff::cl::gerrit_patch_url(&change_num, patchset.as_deref());
            let text = babeldiff::cl::download_patch(&url)?;
            let files = babeldiff::patch::parse(&text);
            let git = Some(Git::new(&repo)).filter(Git::is_repo);
            let mut cs = match &git {
                Some(g) => ChangeSet::from_patch(&files, &mut |id| g.blob(id)),
                None => ChangeSet::from_patch(&files, &mut |_| None),
            };
            let finder: Box<dyn CppFinder> = match git {
                Some(g) => {
                    let base = match babeldiff::cl::patch_commit_sha(&text) {
                        Some(sha)
                            if g.run(&[
                                "rev-parse",
                                "--verify",
                                "--quiet",
                                &format!("{sha}^^{{commit}}"),
                            ])
                            .is_ok() =>
                        {
                            format!("{sha}^")
                        }
                        _ => "HEAD".to_string(),
                    };
                    g.load_companion_headers(&mut cs, &base);
                    if cli.no_search {
                        Box::new(NoFinder)
                    } else {
                        Box::new(RepoFinder::new(g, base))
                    }
                }
                None => Box::new(NoFinder),
            };
            let label = match &patchset {
                Some(ps) => format!("{change_num}/{ps}"),
                None => change_num,
            };
            let title = match patch_title(&text) {
                Some(subj) => format!("fxr/{label}: {subj}"),
                None => format!("fxr/{label}"),
            };
            (cs, finder, title)
        }
        Op::Files { files } => {
            let mut loaded = Vec::new();
            for p in &files {
                let text =
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                loaded.push((p.display().to_string(), text));
            }
            let title = loaded.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(" ");
            (ChangeSet::from_files(&loaded), Box::new(NoFinder), title)
        }
    };
    let mut report = babeldiff::run_with(
        &cs,
        &analyze::Options::default(),
        finder.as_mut(),
        cli.min_changed,
        forced,
    );
    if cli.issues_only {
        report.retain_issues();
    }
    let width = cli
        .width
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()))
        .unwrap_or(160);
    let opts = RenderOptions {
        layout: match cli.layout {
            LayoutArg::SideBySide => Layout::SideBySide,
            LayoutArg::Stacked => Layout::Stacked,
        },
        width,
        context: cli.context,
        summary_only: cli.summary,
    };
    let text = match cli.format {
        FormatArg::Text => render::render(&report, &opts),
        FormatArg::Html => html::render_html(&report, &html::HtmlOptions { title }),
        FormatArg::Json => babeldiff::json::render_json(&report, &title),
    };
    match &cli.output {
        Some(path) => {
            std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))?
        }
        // A closed pipe (`babeldiff ... | head`) is not an error.
        None => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(text.as_bytes());
        }
    }
    Ok(report.issues() + report.lint_issues())
}

/// The subject of a `git format-patch` file, without its `[PATCH]` tag.
fn patch_title(text: &str) -> Option<String> {
    let line = text.lines().take(40).find(|l| l.starts_with("Subject: "))?;
    let s = line.trim_start_matches("Subject: ");
    let s = match s.strip_prefix('[') {
        Some(rest) if s.starts_with("[PATCH") => rest.split_once("] ").map_or(s, |(_, t)| t),
        _ => s,
    };
    Some(s.trim().to_string())
}

fn read_stdin() -> Result<String, String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).map_err(|e| format!("reading stdin: {e}"))?;
    Ok(s)
}
