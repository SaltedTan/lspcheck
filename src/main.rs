// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

use clap::{Parser, Subcommand};
use lspcheck::checks::{self, Config};
use lspcheck::process::{self, SENTINEL_ARG, ServerCommand};
use lspcheck::replay;
use lspcheck::report;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Checks that a language server speaks the Language Server Protocol correctly.
///
/// Example: lspcheck -- gopls
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Write a JSON report here.
    #[arg(long, value_name = "PATH")]
    json: Option<PathBuf>,

    /// Write a JUnit XML report here.
    #[arg(long, value_name = "PATH")]
    junit: Option<PathBuf>,

    /// Write a Markdown summary here.
    #[arg(long, value_name = "PATH")]
    markdown: Option<PathBuf>,

    /// Write each scenario's transcript as JSON Lines into this directory.
    #[arg(long, value_name = "DIR")]
    transcripts: Option<PathBuf>,

    /// Exit with status 1 when any check fails, not only a MUST check.
    #[arg(long)]
    strict: bool,

    /// Seconds to wait for each response.
    #[arg(long, default_value_t = 10, value_name = "SECS")]
    timeout: u64,

    /// Seconds to wait for the reply to initialize.
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    startup_timeout: u64,

    /// Seconds to wait for the server to exit after exit.
    #[arg(long, default_value_t = 5, value_name = "SECS")]
    exit_timeout: u64,

    /// Run only these checks (comma-separated ids, such as L4,L6).
    #[arg(long, value_delimiter = ',', value_name = "IDS")]
    only: Vec<String>,

    /// The largest frame body to inspect, in MiB. A larger frame is skipped
    /// and makes the scenario's failures inconclusive.
    #[arg(long, default_value_t = 64, value_name = "MIB")]
    max_frame_mib: usize,

    /// The most stdout to inspect per scenario, in MiB.
    #[arg(long, default_value_t = 256, value_name = "MIB")]
    max_output_mib: usize,

    /// The server command and its arguments, after `--`.
    #[arg(last = true, value_name = "SERVER")]
    server: Vec<OsString>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List every check with its basis and specification section.
    List,
    /// Re-run a recorded session against a server, without judging checks.
    Replay {
        /// The transcript to replay (a .jsonl file from --transcripts).
        transcript: PathBuf,
        /// The server command and its arguments, after `--`.
        #[arg(last = true, value_name = "SERVER")]
        server: Vec<OsString>,
        /// Seconds to wait for each response.
        #[arg(long, default_value_t = 10, value_name = "SECS")]
        timeout: u64,
        /// Seconds to wait for the server to exit.
        #[arg(long, default_value_t = 5, value_name = "SECS")]
        exit_timeout: u64,
        /// Write the replay's own transcript here.
        #[arg(long = "transcript", value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

fn main() {
    if std::env::args_os().nth(1).is_some_and(|a| a == SENTINEL_ARG) {
        // Hold the process group open until lspcheck closes our stdin.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        return;
    }
    let cli = Cli::parse();
    if let Some(Cmd::List) = cli.command {
        for c in checks::CATALOGUE {
            println!("{:<3} {:<6} {}\n    {}", c.id, c.basis.label(), c.title, checks::spec_url(c));
        }
        return;
    }
    if let Some(Cmd::Replay { transcript, server, timeout, exit_timeout, output }) = cli.command {
        if server.is_empty() {
            eprintln!(
                "lspcheck replay: give the server command after `--`, for example: lspcheck replay t.jsonl -- gopls"
            );
            std::process::exit(2);
        }
        let output = output.unwrap_or_else(|| {
            let mut p = transcript.clone();
            let stem =
                transcript.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "transcript".into());
            p.set_file_name(format!("{stem}.replay.jsonl"));
            p
        });
        match std::fs::symlink_metadata(&output) {
            Ok(_) => {
                eprintln!(
                    "lspcheck replay: output {} already exists; pass --transcript to choose another path or remove it",
                    output.display()
                );
                std::process::exit(2);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("lspcheck replay: can't inspect output {}: {e}", output.display());
                std::process::exit(2);
            }
        }
        process::enable_subreaper();
        let server_cmd = ServerCommand::new(&server);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        match rt.block_on(replay::replay(
            &transcript,
            &server_cmd,
            Duration::from_secs(timeout),
            Duration::from_secs(exit_timeout),
            &output,
        )) {
            Ok(summary) => {
                print!("{}", summary.render());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("lspcheck replay: {e}");
                std::process::exit(2);
            }
        }
    }
    if cli.server.is_empty() {
        eprintln!("lspcheck: give the server command after `--`, for example: lspcheck -- gopls");
        std::process::exit(2);
    }
    for id in &cli.only {
        if !checks::CATALOGUE.iter().any(|c| c.id == id) {
            eprintln!("lspcheck: unknown check {id:?}; `lspcheck list` shows them");
            std::process::exit(2);
        }
    }
    if let Some(dir) = &cli.transcripts
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("lspcheck: can't create {}: {e}", dir.display());
        std::process::exit(2);
    }
    process::enable_subreaper();
    let cfg = Config {
        server: ServerCommand::new(&cli.server),
        sentinel: std::env::current_exe().ok(),
        request_timeout: Duration::from_secs(cli.timeout),
        startup_timeout: Duration::from_secs(cli.startup_timeout),
        exit_timeout: Duration::from_secs(cli.exit_timeout),
        leftover_grace: Duration::from_secs(1),
        max_body: cli.max_frame_mib.saturating_mul(1024 * 1024),
        max_output: cli.max_output_mib.saturating_mul(1024 * 1024),
        transcripts: cli.transcripts.clone(),
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let runs = match rt.block_on(checks::run(&cfg, &cli.only)) {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("lspcheck: {e}");
            std::process::exit(2);
        }
    };
    let server = cfg.server.display();
    let _ = report::print_terminal(&mut std::io::stdout().lock(), &server, &runs);
    let argv: Vec<String> = cli.server.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if let Some(path) = &cli.json {
        let text = serde_json::to_string_pretty(&report::json_report(&argv, &runs)).expect("report serializes");
        if let Err(e) = std::fs::write(path, text + "\n") {
            eprintln!("lspcheck: can't write {}: {e}", path.display());
            std::process::exit(2);
        }
    }
    if let Some(path) = &cli.junit
        && let Err(e) = std::fs::write(path, report::junit_report(&argv, &runs))
    {
        eprintln!("lspcheck: can't write {}: {e}", path.display());
        std::process::exit(2);
    }
    if let Some(path) = &cli.markdown
        && let Err(e) = std::fs::write(path, report::markdown_report(&argv, &runs))
    {
        eprintln!("lspcheck: can't write {}: {e}", path.display());
        std::process::exit(2);
    }
    std::process::exit(report::exit_code(&runs, cli.strict));
}
