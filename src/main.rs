// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

use clap::{Parser, Subcommand};
use lspcheck::checks::{self, Config, SCENARIOS};
use lspcheck::process::{self, SENTINEL_ARG, ServerCommand};
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

    /// The server command and its arguments, after `--`.
    #[arg(last = true, value_name = "SERVER")]
    server: Vec<OsString>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List every check with its basis and specification section.
    List,
}

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == SENTINEL_ARG)
    {
        // Hold the process group open until lspcheck closes our stdin.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        return;
    }
    let cli = Cli::parse();
    if let Some(Cmd::List) = cli.command {
        for c in checks::CATALOGUE {
            println!(
                "{:<3} {:<6} {}\n    {}",
                c.id,
                c.basis.label(),
                c.title,
                checks::spec_url(c)
            );
        }
        return;
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
    process::enable_subreaper();
    let cfg = Config {
        server: ServerCommand::new(&cli.server),
        sentinel: std::env::current_exe().ok(),
        request_timeout: Duration::from_secs(cli.timeout),
        startup_timeout: Duration::from_secs(cli.startup_timeout),
        exit_timeout: Duration::from_secs(cli.exit_timeout),
        leftover_grace: Duration::from_secs(1),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut runs = rt.block_on(async {
        let mut runs = Vec::new();
        for s in SCENARIOS {
            if cli.only.is_empty() || s.checks.iter().any(|c| cli.only.iter().any(|o| o == c)) {
                runs.push(checks::run_scenario(s.name, &cfg).await);
            }
        }
        runs
    });
    if !cli.only.is_empty() {
        for run in &mut runs {
            run.results.retain(|r| cli.only.iter().any(|o| o == r.id));
        }
    }

    if let Some(dir) = &cli.transcripts {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("lspcheck: can't create {}: {e}", dir.display());
            std::process::exit(2);
        }
        for run in &runs {
            if let Some(f) = &run.finish
                && let Err(e) = report::write_transcript(
                    &dir.join(format!("{}.jsonl", run.name)),
                    &f.transcript,
                )
            {
                eprintln!("lspcheck: can't write a transcript: {e}");
                std::process::exit(2);
            }
        }
    }
    let server = cfg.server.display();
    let _ = report::print_terminal(
        &mut std::io::stdout().lock(),
        &server,
        &runs,
        cli.transcripts.as_deref(),
    );
    if let Some(path) = &cli.json {
        let argv: Vec<String> = cli
            .server
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let text = serde_json::to_string_pretty(&report::json_report(&argv, &runs))
            .expect("report serializes");
        if let Err(e) = std::fs::write(path, text + "\n") {
            eprintln!("lspcheck: can't write {}: {e}", path.display());
            std::process::exit(2);
        }
    }
    std::process::exit(report::exit_code(&runs, cli.strict));
}
