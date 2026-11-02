use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;

use aerc_rs::backend;
use aerc_rs::config::Config;
use aerc_rs::runtime::run_tui;

#[derive(Debug, Parser)]
#[command(version, about = "A fast terminal email client foundation inspired by aerc")]
struct Cli {
    /// Config file. Defaults to ~/.config/aerc-rs/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Open one Maildir directly, bypassing config.
    #[arg(long)]
    maildir: Option<PathBuf>,

    /// Account name used with --maildir.
    #[arg(long, default_value = "Local")]
    name: String,

    /// Validate accounts and benchmark folder/header scanning without opening the TUI.
    #[arg(long)]
    headless_check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(&cli)?;
    if cli.headless_check {
        return headless_check(&config);
    }
    run_tui(config)
}

fn load_config(cli: &Cli) -> Result<Config> {
    if let Some(maildir) = &cli.maildir {
        return Ok(Config::single_maildir(cli.name.clone(), maildir.clone()));
    }
    Config::load(cli.config.as_deref())
}

fn headless_check(config: &Config) -> Result<()> {
    for account in &config.accounts {
        let mut backend = backend::from_source(&account.source)?;
        let connect_start = Instant::now();
        backend.connect()?;
        let connect_elapsed = connect_start.elapsed();

        let list_start = Instant::now();
        let mailboxes = backend.list_mailboxes()?;
        let list_elapsed = list_start.elapsed();

        let target = mailboxes
            .iter()
            .find(|mailbox| mailbox.name == account.default_mailbox)
            .or_else(|| mailboxes.first())
            .with_context(|| format!("{} has no mailboxes", account.name))?;

        let scan_start = Instant::now();
        let messages = backend.open_mailbox(&target.name)?;
        let scan_elapsed = scan_start.elapsed();
        println!(
            "{}: {} | connect={:.3} ms list={} folders/{:.3} ms scan={} messages/{:.3} ms ({:.1} msg/s)",
            account.name,
            account.source,
            connect_elapsed.as_secs_f64() * 1000.0,
            mailboxes.len(),
            list_elapsed.as_secs_f64() * 1000.0,
            messages.len(),
            scan_elapsed.as_secs_f64() * 1000.0,
            if scan_elapsed.is_zero() {
                0.0
            } else {
                messages.len() as f64 / scan_elapsed.as_secs_f64()
            }
        );
    }
    Ok(())
}

