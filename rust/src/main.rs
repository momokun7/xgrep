use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use xgrep::{output, SearchOptions, Xgrep};

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
}

#[derive(Subcommand)]
enum Commands {
    /// Build search index
    Init {
        /// Store index in .xgrep/ instead of ~/.cache/xgrep/
        #[arg(long)]
        local: bool,
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
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

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
            let xg = if local {
                Xgrep::open_local(&cwd)?
            } else {
                Xgrep::open(&cwd)?
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
            let root_path = root.map(std::path::PathBuf::from).unwrap_or(cwd);
            let xg = Xgrep::open(&root_path)?;
            xgrep::mcp_server::start(xg);
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

            let xg = Xgrep::open(&cwd)?;
            let opts = SearchOptions {
                case_insensitive: cli.case_insensitive,
                regex: cli.regex,
                file_type: cli.file_type,
                max_count: cli.max_count,
                changed_only: cli.changed,
                since: cli.since,
                path_pattern: None,
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
