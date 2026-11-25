mod config;
mod github;
mod tui;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;
use watcher::{Delivery, NtfyDelivery, Store};

#[derive(Parser)]
#[command(version, about = "Track team pull requests and quiet review queues")]
struct Cli {
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch once, update state, and send configured notifications.
    Check {
        /// Show proposed alerts without sending or changing PR state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Open the dashboard; refreshes every five minutes and with r.
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Commands::Check { dry_run } => {
            let delivery = if dry_run {
                None
            } else {
                config
                    .ntfy
                    .as_ref()
                    .filter(|ntfy| ntfy.enabled)
                    .map(NtfyDelivery::new)
                    .transpose()?
            };
            let mut store = Store::open(&config.database)?;
            let alerts = store.check(
                &config,
                &github::Github::default(),
                delivery.as_ref().map(|d| d as &dyn Delivery),
                chrono::Utc::now,
                dry_run,
            )?;
            for alert in &alerts {
                println!(
                    "{}: {}",
                    if dry_run {
                        "Would notify"
                    } else if delivery.is_some() {
                        "Notified"
                    } else {
                        "Pending (ntfy disabled)"
                    },
                    alert.message()
                );
            }
            println!(
                "{} refresh: {} proposed alert(s).",
                if dry_run { "Dry-run" } else { "Completed" },
                alerts.len()
            );
        }
        Commands::Tui => tui::run(&config)?,
    }
    Ok(())
}
