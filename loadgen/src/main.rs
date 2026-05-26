//! `extenddb-bench` entry point.

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod client;
mod compare;
mod histogram;
mod lg_health;
mod metrics;
mod output;
mod preseed;
mod runner;
mod sweep;
mod workload;

use cli::{Cli, Command};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => runner::run(args).await.context("bench run failed"),
        Command::Preseed(args) => match preseed::run(args).await? {
            preseed::Outcome::Seeded(m) => {
                println!("preseed: seeded {} items @ {:.0} rps", m.items_written, m.achieved_rps);
                Ok(())
            }
            preseed::Outcome::Skipped(reason) => {
                println!("preseed: skipped ({reason})");
                Ok(())
            }
        },
        Command::Report(args) => output::re_render_summary(&args.input).context("report failed"),
        Command::ReportCompare(args) => compare::report_compare(&args).context("report-compare failed"),
        Command::Version => {
            print_version();
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("extenddb_bench=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}

fn print_version() {
    println!("extenddb-bench {}", env!("CARGO_PKG_VERSION"));
    println!("aws-sdk-dynamodb {}", aws_sdk_dynamodb::meta::PKG_VERSION);
}
