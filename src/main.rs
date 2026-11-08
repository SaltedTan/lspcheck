// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

use clap::{Parser, Subcommand};
use lspcheck::checks::{self, Config};
use lspcheck::process::{self, SENTINEL_ARG, ServerCommand};
use lspcheck::profile;
use lspcheck::replay;
use lspcheck::report;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

/// Checks that a language server speaks the Language Server Protocol correctly.
///
/// Example: lspcheck -- gopls
///
/// A command-line flag overrides the profile, which overrides the default.
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

    /// Read a TOML profile: the server command, timeouts, workspace files,
    /// client settings, what to allow, and the document K1 asks about.
    #[arg(long, value_name = "PATH")]
    profile: Option<PathBuf>,

    /// Seconds to wait for responses other than initialize and R3's probe [default: 10, or the profile's request_timeout].
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,

    /// Seconds to wait for initialize and R3's probe after opening the large document [default: 30, or the profile's startup_timeout].
    #[arg(long, value_name = "SECS")]
    startup_timeout: Option<u64>,

    /// Seconds to wait for the server to exit after exit [default: 5, or the profile's exit_timeout].
    #[arg(long, value_name = "SECS")]
    exit_timeout: Option<u64>,

    /// Run only these checks (comma-separated ids, such as L4,L6).
    #[arg(long, value_delimiter = ',', value_name = "IDS")]
    only: Vec<String>,

    /// The largest frame body to inspect, in MiB. A larger frame is skipped
    /// and makes the scenario's failures inconclusive.
    #[arg(long, default_value_t = 64, value_name = "MIB")]
    max_frame_mib: usize,

    /// MiB of text in the large document sent by R3 (must be greater than zero).
    #[arg(long, default_value = "4", value_name = "MIB")]
    large_document_mib: NonZeroUsize,

    /// The most stdout to inspect per scenario, in MiB.
    #[arg(long, default_value_t = 256, value_name = "MIB")]
    max_output_mib: usize,

    /// The server command and its arguments, after `--` (else the profile's command).
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
    // A bad profile fails here, before any server starts.
    let loaded = match &cli.profile {
        Some(path) => match profile::load(path) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("lspcheck: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    let Some(argv) = profile::server_argv(&cli.server, loaded.as_ref()) else {
        match &cli.profile {
            Some(path) => {
                eprintln!("lspcheck: give the server command after `--` or as server.command in {}", path.display())
            }
            None => eprintln!("lspcheck: give the server command after `--`, for example: lspcheck -- gopls"),
        }
        std::process::exit(2);
    };
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
    let p = loaded.unwrap_or_default();
    let cfg = Config {
        server: ServerCommand::new(&argv),
        sentinel: std::env::current_exe().ok(),
        request_timeout: profile::timeout(cli.timeout, p.request_timeout, 10),
        startup_timeout: profile::timeout(cli.startup_timeout, p.startup_timeout, 30),
        exit_timeout: profile::timeout(cli.exit_timeout, p.exit_timeout, 5),
        leftover_grace: Duration::from_secs(1),
        large_document_mib: cli.large_document_mib.get(),
        max_body: cli.max_frame_mib.saturating_mul(1024 * 1024),
        max_output: cli.max_output_mib.saturating_mul(1024 * 1024),
        transcripts: cli.transcripts.clone(),
        workspace: p.workspace,
        initialization_options: p.initialization_options,
        configuration: p.configuration,
        allowed_descendants: p.allowed_descendants,
        skip: p.skip,
        document: p.document,
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
    let profile_path = cli.profile.as_deref();
    let _ = report::print_terminal(&mut std::io::stdout().lock(), &server, profile_path, &runs);
    let argv: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if let Some(path) = &cli.json {
        let report = report::json_report(&argv, profile_path, &runs);
        let text = serde_json::to_string_pretty(&report).expect("report serializes");
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
