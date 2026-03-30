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

    /// Directory to search (default: current directory)
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

/// Resolve the target directory from an optional path argument.
/// Returns the canonicalized directory path, or an error if the path is invalid.
fn resolve_dir(path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("path does not exist: {}", p.display());
            }
            if !p.is_dir() {
                anyhow::bail!("not a directory: {}", p.display());
            }
            Ok(p.canonicalize()?)
        }
        None => Ok(env::current_dir()?),
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

            let dir = resolve_dir(cli.path.as_deref())?;
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

            if results.is_empty() {
                std::process::exit(1);
            }

            if cli.count {
                let mut counts: std::collections::BTreeMap<&str, usize> =
                    std::collections::BTreeMap::new();
                for r in &results {
                    *counts.entry(&r.file).or_insert(0) += 1;
                }
                for (file, count) in counts {
                    println!("{}:{}", file, count);
                }
            } else if cli.files_only {
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
                        output::format_llm(&results, &dir, ctx, None)?
                    }
                    _ => {
                        if let Some(ctx) = cli.context {
                            output::format_default_context(&results, &dir, ctx)?
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
        assert!(err.to_string().contains("not a directory"));
        fs::remove_file(&tmp).ok();
    }
}
