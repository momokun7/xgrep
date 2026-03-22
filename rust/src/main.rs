use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use xgrep::filetype;
use xgrep::git;
use xgrep::index;
use xgrep::output;
use xgrep::search;

#[derive(Parser)]
#[command(name = "xgrep", about = "Ultra-fast indexed code search")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search pattern
    pattern: Option<String>,

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

    /// Disable color output
    #[arg(long)]
    no_color: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Build search index
    Init {
        /// Store index in .xgrep/ instead of ~/.cache/xgrep/
        #[arg(long)]
        local: bool,
    },
}

fn index_path(local: bool) -> Result<PathBuf> {
    if local {
        Ok(PathBuf::from(".xgrep/index"))
    } else {
        let cwd = env::current_dir()?;
        let hash = xxhash_rust::xxh64::xxh64(cwd.to_string_lossy().as_bytes(), 0);
        let cache_dir = dirs_next::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("xgrep")
            .join(format!("{:016x}", hash));
        std::fs::create_dir_all(&cache_dir)?;
        Ok(cache_dir.join("index"))
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {}", e);
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;

    match cli.command {
        Some(Commands::Init { local }) => {
            let idx = index_path(local)?;
            if local {
                std::fs::create_dir_all(".xgrep")?;
            }
            let start = std::time::Instant::now();
            index::builder::build_index(&cwd, &idx)?;
            let elapsed = start.elapsed();
            let meta = std::fs::metadata(&idx)?;
            eprintln!(
                "Index built: {} ({} bytes) in {:.2}s",
                idx.display(),
                meta.len(),
                elapsed.as_secs_f64()
            );
        }
        None => {
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
                std::process::exit(1);
            });

            let results = if cli.changed || cli.since.is_some() {
                // Git連携検索
                if !git::is_git_repo(&cwd) {
                    eprintln!("error: not a git repository");
                    std::process::exit(1);
                }
                let mut files = Vec::new();
                if cli.changed {
                    files.extend(git::changed_files(&cwd)?);
                }
                if let Some(ref since) = cli.since {
                    files.extend(git::since_files(&cwd, since)?);
                }
                files.sort();
                files.dedup();
                if cli.regex {
                    search::search_files_regex(&cwd, &files, &pattern, cli.case_insensitive)?
                } else {
                    search::search_files(&cwd, &files, &pattern, cli.case_insensitive)?
                }
            } else {
                // インデックス検索
                let local_idx = PathBuf::from(".xgrep/index");
                let idx = if local_idx.exists() {
                    local_idx
                } else {
                    let cache_idx = index_path(false)?;
                    if !cache_idx.exists() {
                        eprintln!("[indexing...]");
                        index::builder::build_index(&cwd, &cache_idx)?;
                        eprintln!("[done]");
                    }
                    cache_idx
                };
                let reader = index::reader::IndexReader::open(&idx)?;
                if cli.regex {
                    search::search_regex(&reader, &cwd, &pattern, cli.case_insensitive)?
                } else {
                    search::search(&reader, &cwd, &pattern, cli.case_insensitive)?
                }
            };

            // ファイルタイプフィルタ適用
            let results = if let Some(ref ft) = cli.file_type {
                if let Some(exts) = filetype::extensions_for_type(ft) {
                    results
                        .into_iter()
                        .filter(|r| {
                            std::path::Path::new(&r.file)
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| exts.contains(&e))
                        })
                        .collect()
                } else {
                    eprintln!("warning: unknown file type '{}', showing all results", ft);
                    results
                }
            } else {
                results
            };

            // max_count適用
            let results = if let Some(max) = cli.max_count {
                results.into_iter().take(max).collect::<Vec<_>>()
            } else {
                results
            };

            if results.is_empty() {
                std::process::exit(1);
            }

            if cli.count {
                // ファイルごとのマッチ数を表示
                let mut counts: std::collections::BTreeMap<&str, usize> =
                    std::collections::BTreeMap::new();
                for r in &results {
                    *counts.entry(&r.file).or_insert(0) += 1;
                }
                for (file, count) in counts {
                    println!("{}:{}", file, count);
                }
            } else if cli.files_only {
                // マッチしたファイル名のみ表示
                let mut seen = std::collections::BTreeSet::new();
                for r in &results {
                    if seen.insert(&r.file) {
                        println!("{}", r.file);
                    }
                }
            } else if cli.json_output {
                println!("{}", output::format_json(&results));
            } else {
                let output_str = match cli.format.as_str() {
                    "llm" => {
                        let ctx = cli.context.unwrap_or(3);
                        output::format_llm(&results, &cwd, ctx)?
                    }
                    _ => {
                        if let Some(ctx) = cli.context {
                            output::format_default_context(&results, &cwd, ctx)?
                        } else if !cli.no_color {
                            output::print_results_color(&results, &pattern);
                            return Ok(());
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
