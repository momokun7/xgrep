use clap::{Parser, Subcommand};
use anyhow::Result;
use std::path::PathBuf;
use std::env;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use xgrep::index;
use xgrep::search;
use xgrep::output;
use xgrep::git;
use xgrep::filetype;

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

fn main() -> Result<()> {
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
                    results.into_iter().filter(|r| {
                        std::path::Path::new(&r.file)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map_or(false, |e| exts.contains(&e))
                    }).collect()
                } else {
                    eprintln!("warning: unknown file type '{}', showing all results", ft);
                    results
                }
            } else {
                results
            };

            if !results.is_empty() {
                let output_str = match cli.format.as_str() {
                    "llm" => {
                        let ctx = cli.context.unwrap_or(3);
                        output::format_llm(&results, &cwd, ctx)?
                    }
                    _ => {
                        if let Some(ctx) = cli.context {
                            output::format_default_context(&results, &cwd, ctx)?
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
