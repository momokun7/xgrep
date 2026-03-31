use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;
use std::path::{Path, PathBuf};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use xgrep_search::{output, SearchOptions, Xgrep};

#[derive(Parser)]
#[command(name = "xg", about = "Ultra-fast indexed code search (xgrep)", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search pattern
    pattern: Option<String>,

    /// File or directory to search (default: current directory)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Output format (default, llm)
    #[arg(long, default_value = "default")]
    format: String,

    /// Case-insensitive search
    #[arg(short = 'i')]
    case_insensitive: bool,

    /// Context lines (default: 3 for --format llm, none for default)
    #[arg(short = 'C')]
    context: Option<usize>,

    /// Search only in git changed files (unstaged + staged)
    #[arg(long)]
    changed: bool,

    /// Search files changed within duration (e.g., 1h, 30m, 2d, 1w, 3.commits)
    #[arg(long)]
    since: Option<String>,

    /// Use regex pattern
    #[arg(short = 'e')]
    regex: bool,

    /// Filter by file type (e.g., rs, py, js)
    #[arg(long = "type", short = 't')]
    file_type: Option<String>,

    /// Only print count of matching lines per file
    #[arg(short = 'c')]
    count: bool,

    /// Only print file names with matches
    #[arg(short = 'l')]
    files_only: bool,

    /// Maximum number of results
    #[arg(long)]
    max_count: Option<usize>,

    /// Output as JSON
    #[arg(long = "json")]
    json_output: bool,

    /// Check index freshness and include changed files (slower but always up-to-date)
    #[arg(long)]
    fresh: bool,

    /// List all supported file types
    #[arg(long)]
    list_types: bool,

    /// Show absolute paths in output
    #[arg(long)]
    absolute_paths: bool,

    /// Find files by name pattern (glob or substring) instead of searching contents
    #[arg(long)]
    find: bool,

    /// Suppress hint messages (regex detection, etc.)
    #[arg(long)]
    no_hints: bool,

    /// Exclude files matching path substring (can be repeated)
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Build search index
    Init {
        /// Store index in .xgrep/ instead of ~/.cache/xgrep/
        #[arg(long)]
        local: bool,

        /// Directory to index (default: current directory)
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Start MCP server (stdio transport)
    Serve {
        /// Root directory to search (default: current directory)
        #[arg(long)]
        root: Option<String>,
    },
    /// Show index status
    Status {
        /// Directory to check (default: current directory)
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

fn main() {
    #[cfg(unix)]
    unsafe {
        // SAFETY: Restore SIGPIPE to default behavior to prevent panic on broken pipe.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if let Err(e) = run() {
        eprintln!("error: {}", e);
        std::process::exit(2);
    }
}

/// Resolved path target: either a directory or a single file.
enum ResolvedPath {
    Dir(PathBuf),
    File { dir: PathBuf, file: PathBuf },
}

/// Resolve the target path from an optional path argument.
/// Accepts both directories and single files.
fn resolve_path(path: Option<&Path>) -> Result<ResolvedPath> {
    match path {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("path does not exist: {}", p.display());
            }
            let canonical = p.canonicalize()?;
            if canonical.is_dir() {
                Ok(ResolvedPath::Dir(canonical))
            } else {
                let dir = canonical
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("cannot determine parent directory"))?
                    .to_path_buf();
                Ok(ResolvedPath::File {
                    dir,
                    file: canonical,
                })
            }
        }
        None => Ok(ResolvedPath::Dir(env::current_dir()?.canonicalize()?)),
    }
}

/// Resolve the target directory from an optional path argument (for init/serve).
fn resolve_dir(path: Option<&Path>) -> Result<PathBuf> {
    match resolve_path(path)? {
        ResolvedPath::Dir(d) => Ok(d),
        ResolvedPath::File { .. } => {
            anyhow::bail!("expected a directory, not a file");
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Init { local, path }) => {
            let dir = resolve_dir(path.as_deref())?;
            let xg = if local {
                Xgrep::open_local(&dir)?
            } else {
                Xgrep::open(&dir)?
            };
            let start = std::time::Instant::now();
            xg.build_index()?;
            let meta = std::fs::metadata(xg.index_path())?;
            eprintln!(
                "Index built: {} ({} bytes) in {:.2}s",
                xg.index_path().display(),
                meta.len(),
                start.elapsed().as_secs_f64()
            );
        }
        Some(Commands::Serve { root }) => {
            let root_path = root
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| env::current_dir().expect("failed to get current directory"));
            let xg = Xgrep::open(&root_path)?;
            xgrep_search::start_mcp_server(xg);
        }
        Some(Commands::Status { path }) => {
            let dir = resolve_dir(path.as_deref())?;
            let xg = Xgrep::open(&dir)?;
            if xg.index_path().exists() {
                let info = xg.index_status()?;
                println!("{}", info);
                if let Ok(meta) = std::fs::metadata(xg.index_path()) {
                    if let Ok(modified) = meta.modified() {
                        let age = modified.elapsed().unwrap_or_default();
                        println!("Last built: {}s ago", age.as_secs());
                    }
                }
            } else {
                println!("No index found");
                println!("Index path: {}", xg.index_path().display());
                println!("Run 'xg init' to build the index");
            }
        }
        None => {
            if cli.list_types {
                let types = xgrep_search::list_all_types();
                for (name, exts) in &types {
                    let ext_str: Vec<String> = exts.iter().map(|e| format!("*.{}", e)).collect();
                    println!("{:<14}{}", name, ext_str.join(", "));
                }
                return Ok(());
            }

            if cli.json_output && cli.format != "default" {
                eprintln!("error: --json cannot be combined with --format");
                std::process::exit(2);
            }
            if (cli.count as u8 + cli.files_only as u8 + cli.json_output as u8) > 1 {
                eprintln!("error: -c, -l, and --json are mutually exclusive");
                std::process::exit(2);
            }

            let pattern = cli.pattern.unwrap_or_else(|| {
                eprintln!("Usage: xgrep <pattern> or xgrep init");
                std::process::exit(2);
            });

            let use_absolute = cli.absolute_paths
                || std::env::var("XGREP_ABSOLUTE_PATHS")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false);

            // Regex hint detection (only in literal mode, not --find, not suppressed)
            let show_hints = !cli.regex
                && !cli.find
                && !cli.no_hints
                && std::env::var("XGREP_NO_HINTS")
                    .map(|v| v != "1")
                    .unwrap_or(true);
            if show_hints {
                if let Some(hint) = xgrep_search::hints::detect_regex_hint(&pattern) {
                    eprintln!("{}", hint);
                }
            }

            if cli.find {
                let dir = resolve_dir(cli.path.as_deref())?;
                let xg = Xgrep::open(&dir)?;

                let mut files = if cli.changed {
                    // --find + --changed: only search changed files
                    let changed = xgrep_search::git_changed_files(&dir)?;
                    let pattern_lower = pattern.to_lowercase();
                    let is_glob =
                        pattern.contains('*') || pattern.contains('?') || pattern.contains('[');
                    if is_glob {
                        let glob = glob::Pattern::new(&pattern)
                            .map_err(|e| anyhow::anyhow!("invalid glob pattern: {}", e))?;
                        changed
                            .into_iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .filter(|p| glob.matches(p))
                            .collect::<Vec<_>>()
                    } else {
                        changed
                            .into_iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .filter(|p| p.to_lowercase().contains(&pattern_lower))
                            .collect::<Vec<_>>()
                    }
                } else {
                    xg.find_files(&pattern)?
                };

                // Apply -t file type filter
                if let Some(ref ft) = cli.file_type {
                    if let Some(exts) = xgrep_search::extensions_for_type(ft) {
                        files.retain(|f| {
                            Path::new(f)
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| exts.contains(&e))
                        });
                    } else {
                        eprintln!("warning: unknown file type '{}', showing all results", ft);
                    }
                }

                // Apply --exclude filter
                if !cli.exclude.is_empty() {
                    files.retain(|f| {
                        !cli.exclude
                            .iter()
                            .any(|ex| !ex.is_empty() && f.contains(ex.as_str()))
                    });
                }

                if files.is_empty() {
                    std::process::exit(1);
                }

                let limit = cli.max_count.unwrap_or(usize::MAX);
                let files: Vec<String> = files.into_iter().take(limit).collect();

                if cli.count {
                    println!("{}", files.len());
                } else if cli.json_output {
                    let paths: Vec<String> = if use_absolute {
                        files
                            .iter()
                            .map(|f| dir.join(f).to_string_lossy().to_string())
                            .collect()
                    } else {
                        files
                    };
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&paths).unwrap_or_else(|_| "[]".to_string())
                    );
                } else {
                    for f in &files {
                        if use_absolute {
                            println!("{}", dir.join(f).display());
                        } else {
                            println!("{}", f);
                        }
                    }
                }
                return Ok(());
            }

            let resolved = resolve_path(cli.path.as_deref())?;
            let (dir, results) = match resolved {
                ResolvedPath::Dir(dir) => {
                    let xg = Xgrep::open(&dir)?;
                    let opts = SearchOptions {
                        case_insensitive: cli.case_insensitive,
                        regex: cli.regex,
                        file_type: cli.file_type,
                        max_count: cli.max_count,
                        changed_only: cli.changed,
                        since: cli.since,
                        path_pattern: None,
                        fresh: cli.fresh,
                    };
                    let results = xg.search(&pattern, &opts)?;
                    (dir, results)
                }
                ResolvedPath::File { dir, file } => {
                    let rel_path = file.strip_prefix(&dir).unwrap_or(&file).to_path_buf();
                    let results = if cli.regex {
                        xgrep_search::search::search_files_regex(
                            &dir,
                            &[rel_path],
                            &pattern,
                            cli.case_insensitive,
                        )?
                    } else {
                        xgrep_search::search::search_files(
                            &dir,
                            &[rel_path],
                            &pattern,
                            cli.case_insensitive,
                        )?
                    };
                    (dir, results)
                }
            };

            // Apply --exclude filter
            let results = if cli.exclude.is_empty() {
                results
            } else {
                results
                    .into_iter()
                    .filter(|r| {
                        !cli.exclude
                            .iter()
                            .any(|ex| !ex.is_empty() && r.file.contains(ex.as_str()))
                    })
                    .collect()
            };

            if results.is_empty() {
                std::process::exit(1);
            }

            let make_path = |rel: &str| -> String {
                if use_absolute {
                    dir.join(rel).to_string_lossy().to_string()
                } else {
                    rel.to_string()
                }
            };

            if cli.count {
                let mut counts: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for r in &results {
                    *counts.entry(make_path(&r.file)).or_insert(0) += 1;
                }
                for (file, count) in counts {
                    println!("{}:{}", file, count);
                }
            } else if cli.files_only {
                let mut seen = std::collections::BTreeSet::new();
                for r in &results {
                    let p = make_path(&r.file);
                    if seen.insert(p.clone()) {
                        println!("{}", p);
                    }
                }
            } else if cli.json_output {
                if use_absolute {
                    let abs_results: Vec<_> = results
                        .iter()
                        .map(|r| xgrep_search::SearchResult {
                            file: make_path(&r.file),
                            line_number: r.line_number,
                            line: r.line.clone(),
                        })
                        .collect();
                    println!("{}", output::format_json(&abs_results));
                } else {
                    println!("{}", output::format_json(&results));
                }
            } else {
                let output_str = match cli.format.as_str() {
                    "llm" => {
                        let ctx = cli.context.unwrap_or_else(|| {
                            std::env::var("XGREP_LLM_CONTEXT")
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(3)
                        });
                        output::format_llm(&results, &dir, ctx, None, use_absolute)?
                    }
                    _ => {
                        if let Some(ctx) = cli.context {
                            output::format_default_context(&results, &dir, ctx, use_absolute)?
                        } else if use_absolute {
                            let abs_results: Vec<_> = results
                                .iter()
                                .map(|r| xgrep_search::SearchResult {
                                    file: make_path(&r.file),
                                    line_number: r.line_number,
                                    line: r.line.clone(),
                                })
                                .collect();
                            output::format_default(&abs_results)
                        } else {
                            output::format_default(&results)
                        }
                    }
                };
                println!("{}", output_str);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_dir_none_returns_cwd() {
        let result = resolve_dir(None).unwrap();
        let cwd = env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(result, cwd);
    }

    #[test]
    fn resolve_dir_absolute_path() {
        let tmp = env::temp_dir();
        let result = resolve_dir(Some(&tmp)).unwrap();
        assert_eq!(result, tmp.canonicalize().unwrap());
    }

    #[test]
    fn resolve_dir_nonexistent_path() {
        let bad = PathBuf::from("/nonexistent_xgrep_test_path");
        let err = resolve_dir(Some(&bad)).unwrap_err();
        assert!(err.to_string().contains("path does not exist"));
    }

    #[test]
    fn resolve_dir_file_path() {
        let tmp = env::temp_dir().join("xgrep_test_resolve_dir_file");
        fs::write(&tmp, "test").unwrap();
        let err = resolve_dir(Some(&tmp)).unwrap_err();
        assert!(err.to_string().contains("expected a directory"));
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn resolve_path_returns_file_for_file() {
        let tmp = env::temp_dir().join("xgrep_test_resolve_path_file");
        fs::write(&tmp, "test content").unwrap();
        let result = resolve_path(Some(&tmp)).unwrap();
        match result {
            ResolvedPath::File { dir, file } => {
                assert_eq!(file, tmp.canonicalize().unwrap());
                assert_eq!(dir, tmp.parent().unwrap().canonicalize().unwrap());
            }
            ResolvedPath::Dir(_) => panic!("expected File variant"),
        }
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn resolve_path_returns_dir_for_dir() {
        let tmp = env::temp_dir();
        let result = resolve_path(Some(&tmp)).unwrap();
        match result {
            ResolvedPath::Dir(d) => assert_eq!(d, tmp.canonicalize().unwrap()),
            ResolvedPath::File { .. } => panic!("expected Dir variant"),
        }
    }

    #[test]
    fn resolve_path_none_returns_cwd() {
        let result = resolve_path(None).unwrap();
        match result {
            ResolvedPath::Dir(d) => {
                assert_eq!(d, env::current_dir().unwrap().canonicalize().unwrap())
            }
            ResolvedPath::File { .. } => panic!("expected Dir variant"),
        }
    }
}
