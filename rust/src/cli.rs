use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{output, Config, FileMatches, OutputMode, SearchOptions, Xgrep};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "xg", about = "Ultra-fast indexed code search (xgrep)", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search pattern
    pattern: Option<String>,

    /// Files or directories to search (default: current directory).
    /// Each directory uses its own index; results are concatenated.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Output format (default, llm)
    #[arg(long, default_value = "default")]
    format: String,

    /// Approximate token budget for --format llm output; output stops with a
    /// "truncated" notice once the next line would exceed it (default: unlimited)
    #[arg(long, value_name = "NUM")]
    max_tokens: Option<usize>,

    /// Truncate lines longer than NUM characters, showing "…[+N chars]"
    /// (0 = no limit; default: no limit, 300 for --format llm)
    #[arg(long, short = 'M', value_name = "NUM")]
    max_columns: Option<usize>,

    /// Case-insensitive search (default: smart-case — insensitive unless
    /// the pattern contains an uppercase letter)
    #[arg(short = 'i')]
    case_insensitive: bool,

    /// Force case-sensitive search (disables smart-case)
    #[arg(short = 's', long = "case-sensitive")]
    case_sensitive: bool,

    /// Context lines (default: 3 for --format llm, none for default)
    #[arg(short = 'C')]
    context: Option<usize>,

    /// Lines of context after each match (overrides -C)
    #[arg(short = 'A', long = "after-context", value_name = "NUM")]
    after_context: Option<usize>,

    /// Lines of context before each match (overrides -C)
    #[arg(short = 'B', long = "before-context", value_name = "NUM")]
    before_context: Option<usize>,

    /// Search only in git changed files (unstaged + staged)
    #[arg(long)]
    changed: bool,

    /// Search files changed within duration (e.g., 1h, 30m, 2d, 1w, 3.commits)
    #[arg(long)]
    since: Option<String>,

    /// Use regex pattern
    #[arg(short = 'e')]
    regex: bool,

    /// Only show matches surrounded by word boundaries
    #[arg(short = 'w', long = "word-regexp")]
    word_regexp: bool,

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

    /// Output as JSON Lines: one {"type":"match",...} object per matching
    /// line, then a {"type":"summary",...} object (with --find: a JSON array)
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

    /// Include or exclude files by glob (prefix with ! to exclude; repeatable)
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    globs: Vec<String>,

    /// Only report files that also contain PATTERN (file-level AND;
    /// repeatable). Interpreted like the main pattern (-e, -w, case)
    #[arg(long = "and", value_name = "PATTERN")]
    and_patterns: Vec<String>,

    /// Skip files that contain PATTERN (file-level NOT; repeatable).
    /// Interpreted like the main pattern (-e, -w, case)
    #[arg(long = "not", value_name = "PATTERN")]
    not_patterns: Vec<String>,
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

pub fn run_cli() {
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
            let rebuilt = xg.build_index()?;
            if rebuilt {
                let meta = std::fs::metadata(xg.index_path())?;
                eprintln!(
                    "Index built: {} ({} bytes) in {:.2}s",
                    xg.index_path().display(),
                    meta.len(),
                    start.elapsed().as_secs_f64()
                );
            } else {
                eprintln!(
                    "Index is up to date. ({:.2}s)",
                    start.elapsed().as_secs_f64()
                );
            }
        }
        Some(Commands::Serve { root }) => {
            let root_path = root
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| env::current_dir().expect("failed to get current directory"));
            let xg = Xgrep::open(&root_path)?.with_config(Config { quiet: true });
            crate::start_mcp_server(xg);
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
                let types = crate::list_all_types();
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
            if cli.max_tokens.is_some() && (cli.format != "llm" || cli.count || cli.files_only) {
                eprintln!("error: --max-tokens requires --format llm");
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
                if let Some(hint) = crate::hints::detect_regex_hint(&pattern) {
                    eprintln!("{}", hint);
                }
            }

            if cli.find {
                if cli.paths.len() > 1 {
                    anyhow::bail!("--find accepts a single PATH");
                }
                let dir = resolve_dir(cli.paths.first().map(PathBuf::as_path))?;
                let xg = Xgrep::open(&dir)?;

                let mut files = if cli.changed {
                    // --find + --changed: only search changed files
                    let changed = crate::git_changed_files(&dir)?;
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
                    if let Some(exts) = crate::extensions_for_type(ft) {
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

            let case_insensitive = if cli.case_insensitive {
                true
            } else if cli.case_sensitive {
                false
            } else {
                !crate::pattern_has_uppercase(&pattern, cli.regex)
            };

            // One-shot CLI: background index rebuilds may spawn a detached
            // `xg init` (this executable) instead of a thread that would die
            // with the process.
            crate::set_background_rebuild_process(true);

            let base_opts = SearchOptions {
                case_insensitive,
                regex: cli.regex,
                word: cli.word_regexp,
                globs: cli.globs.clone(),
                and_patterns: cli.and_patterns.clone(),
                not_patterns: cli.not_patterns.clone(),
                ..Default::default()
            };
            let excluded = |file: &str| {
                cli.exclude
                    .iter()
                    .any(|ex| !ex.is_empty() && file.contains(ex.as_str()))
            };
            // 0 means "no limit", like ripgrep's --max-columns.
            let max_columns = cli.max_columns.filter(|&n| n > 0);
            let text_opts = |base: usize| output::TextOptions {
                before_context: cli.before_context.unwrap_or(base),
                after_context: cli.after_context.unwrap_or(base),
                absolute_paths: use_absolute,
                max_columns,
            };
            let llm_opts = output::LlmOptions {
                before_context: 0,
                after_context: 0,
                max_tokens: cli.max_tokens,
                absolute_paths: use_absolute,
                max_line_chars: match cli.max_columns {
                    Some(0) => None,
                    Some(n) => Some(n),
                    None => Some(output::DEFAULT_LLM_MAX_LINE_CHARS),
                },
            };

            let targets: Vec<Option<&Path>> = if cli.paths.is_empty() {
                vec![None]
            } else {
                cli.paths.iter().map(|p| Some(p.as_path())).collect()
            };
            let resolved = targets
                .into_iter()
                .map(resolve_path)
                .collect::<Result<Vec<_>>>()?;
            let dirs: Vec<PathBuf> = resolved
                .iter()
                .map(|r| match r {
                    ResolvedPath::Dir(d) => d.clone(),
                    ResolvedPath::File { dir, .. } => dir.clone(),
                })
                .collect();
            // With several PATHs, results are re-rooted at the deepest common
            // ancestor so file names stay unambiguous.
            let base = common_base(&dirs);
            let multi = resolved.len() > 1;

            let stdout = std::io::stdout();
            let mut out = std::io::BufWriter::new(stdout.lock());

            let has_context = cli.context.is_some()
                || cli.before_context.is_some()
                || cli.after_context.is_some();
            let ctx_opts = text_opts(cli.context.unwrap_or(0));
            let llm_opts = {
                let base_ctx = cli.context.unwrap_or_else(|| {
                    std::env::var("XGREP_LLM_CONTEXT")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(3)
                });
                output::LlmOptions {
                    before_context: cli.before_context.unwrap_or(base_ctx),
                    after_context: cli.after_context.unwrap_or(base_ctx),
                    ..llm_opts
                }
            };
            let mut emitter = if cli.count {
                Emitter::Count
            } else if cli.files_only {
                Emitter::Files
            } else if cli.json_output {
                Emitter::Json(output::JsonLinesWriter::new(&base, use_absolute))
            } else if cli.format == "llm" {
                Emitter::Llm(output::LlmWriter::new(&base, &llm_opts))
            } else if has_context {
                Emitter::Context(output::ContextWriter::new(&base, &ctx_opts))
            } else {
                Emitter::Plain(&ctx_opts)
            };
            // -l with a line limit needs exact counts to honour the limit.
            let mode = if cli.count || (cli.files_only && cli.max_count.is_some()) {
                OutputMode::Count
            } else if cli.files_only {
                OutputMode::FilesOnly
            } else {
                OutputMode::Lines
            };

            let mut remaining = cli.max_count.unwrap_or(usize::MAX);
            let mut total = 0usize;
            let mut seen: std::collections::HashSet<Arc<str>> = Default::default();
            let mut io_err: Option<std::io::Error> = None;
            for (target, dir) in resolved.iter().zip(&dirs) {
                if remaining == 0 {
                    break;
                }
                let prefix = dir.strip_prefix(&base).unwrap_or(Path::new(""));
                // Results stream out file by file; --exclude, multi-PATH
                // de-duplication and --max-count are applied here.
                let mut sink = |mut fm: FileMatches| -> bool {
                    if !prefix.as_os_str().is_empty() {
                        let file: Arc<str> = prefix.join(&*fm.file).to_string_lossy().into();
                        for l in &mut fm.lines {
                            l.file = Arc::clone(&file);
                        }
                        fm.file = file;
                    }
                    if excluded(&fm.file) || (multi && !seen.insert(Arc::clone(&fm.file))) {
                        return true;
                    }
                    fm.lines.truncate(remaining);
                    fm.count = fm.count.min(remaining);
                    remaining -= fm.count;
                    total += fm.count;
                    if let Err(e) = emitter.write(&mut out, &fm, &base, use_absolute) {
                        io_err = Some(e);
                        return false;
                    }
                    remaining > 0
                };
                let xg = Xgrep::open(dir)?;
                match target {
                    ResolvedPath::Dir(_) => {
                        let opts = SearchOptions {
                            file_type: cli.file_type.clone(),
                            changed_only: cli.changed,
                            since: cli.since.clone(),
                            fresh: cli.fresh,
                            ..base_opts.clone()
                        };
                        xg.search_stream(&pattern, &opts, mode, &mut sink)?;
                    }
                    ResolvedPath::File { dir, file } => {
                        let rel_path = file.strip_prefix(dir).unwrap_or(file).to_path_buf();
                        xg.search_files_stream(&[rel_path], &pattern, &base_opts, mode, &mut sink)?;
                    }
                }
                if let Some(e) = io_err.take() {
                    return Err(e.into());
                }
            }

            if total == 0 {
                std::process::exit(1);
            }
            // The search stopped at --max-count, so there may be more matches.
            let hit_max_count = cli.max_count.is_some_and(|m| total >= m);
            emitter.finish(&mut out, hit_max_count)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Path as printed: relative to `root`, or joined with it.
fn make_display_path(root: &Path, rel: &str, absolute: bool) -> String {
    if absolute {
        root.join(rel).to_string_lossy().to_string()
    } else {
        rel.to_string()
    }
}

/// Deepest common ancestor of `dirs` (the directory itself for one PATH).
fn common_base(dirs: &[PathBuf]) -> PathBuf {
    let mut base = dirs[0].clone();
    for dir in &dirs[1..] {
        while !dir.starts_with(&base) {
            if !base.pop() {
                break;
            }
        }
    }
    base
}

/// Writes streamed per-file results in the selected output format.
enum Emitter<'a> {
    Count,
    Files,
    Json(output::JsonLinesWriter<'a>),
    Llm(output::LlmWriter<'a>),
    Context(output::ContextWriter<'a>),
    Plain(&'a output::TextOptions),
}

impl Emitter<'_> {
    fn write<W: Write>(
        &mut self,
        w: &mut W,
        fm: &FileMatches,
        root: &Path,
        absolute: bool,
    ) -> std::io::Result<()> {
        match self {
            Emitter::Count => writeln!(
                w,
                "{}:{}",
                make_display_path(root, &fm.file, absolute),
                fm.count
            ),
            Emitter::Files => writeln!(w, "{}", make_display_path(root, &fm.file, absolute)),
            Emitter::Json(jw) => jw.write_results(w, &fm.lines),
            Emitter::Llm(lw) => lw.write_results(w, &fm.lines),
            Emitter::Context(cw) => cw.write_results(w, &fm.lines),
            Emitter::Plain(opts) => output::write_default(w, &fm.lines, root, opts),
        }
    }

    fn finish<W: Write>(self, w: &mut W, truncated: bool) -> std::io::Result<()> {
        match self {
            Emitter::Json(jw) => jw.finish(w, truncated),
            Emitter::Llm(lw) => lw.finish(w).map(|_| ()),
            _ => Ok(()),
        }
    }
}
