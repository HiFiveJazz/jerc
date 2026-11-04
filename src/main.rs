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

    /// Sign in to a configured Google/Microsoft OAuth account using a browser.
    /// Tokens are stored privately, not in config.toml.
    #[arg(long, value_name = "ACCOUNT_NAME")]
    oauth_login: Option<String>,

    /// Show the account's OAuth redirect, permissions, and consent options
    /// without opening a browser or making a network request.
    #[arg(long, value_name = "ACCOUNT_NAME", conflicts_with = "oauth_login")]
    oauth_inspect: Option<String>,

    /// Test Microsoft/Google token refresh, live IMAP XOAUTH2, and (if set)
    /// SMTP OAuth authentication. Does not read mail or send any messages.
    #[arg(long, value_name = "ACCOUNT_NAME", conflicts_with_all = ["oauth_login", "oauth_inspect"])]
    oauth_probe: Option<String>,

    /// Validate accounts and benchmark folder/header scanning without opening the TUI.
    #[arg(long)]
    headless_check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(&cli)?;
    if let Some(name) = &cli.oauth_login {
        return aerc_rs::oauth::login_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_inspect {
        return aerc_rs::oauth::inspect_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_probe {
        return oauth_probe(&config, name);
    }
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

fn oauth_probe(config: &Config, name: &str) -> Result<()> {
    let account = config.accounts.iter().find(|account| account.name == name)
        .with_context(|| format!("no account named {name:?} in config"))?;
    let settings = aerc_rs::oauth::OAuthSettings::for_account(account)?
        .with_context(|| format!("account {name:?} has no [accounts.oauth] configuration"))?;
    println!("Testing OAuth for {name:?} (no email will be sent)");

    // Deliberately discard the access token without displaying it. The IMAP
    // and SMTP probes use their real, existing protocol authentication paths.
    drop(settings.access_token().context("refreshing OAuth token")?);
    println!("  OAuth refresh: OK");

    let mut backend = backend::from_account(account)?;
    backend.connect().context("connecting/authenticating to IMAP using XOAUTH2")?;
    println!("  IMAP XOAUTH2: OK");
    let mailboxes = backend.list_mailboxes().context("listing IMAP mailboxes")?;
    println!("  IMAP mailbox LIST: OK ({} folders)", mailboxes.len());

    if let Some(outgoing) = account.outgoing.as_deref() {
        if outgoing.starts_with("smtp://") || outgoing.starts_with("smtps://") ||
           outgoing.starts_with("smtp+") || outgoing.starts_with("smtps+") {
            let source = aerc_rs::outgoing::SmtpSource::parse(outgoing)?
                .with_account_oauth(account)?;
            let sender = aerc_rs::outgoing::SmtpSender::new(source, account.outgoing_cred_cmd.clone());
            sender.probe_oauth().context("authenticating to SMTP without sending a message")?;
            println!("  SMTP OAuth authentication: OK (no message sent)");
        } else {
            println!("  SMTP: skipped (not an SMTP outgoing URI)");
        }
    } else {
        println!("  SMTP: skipped (no outgoing server configured)");
    }
    println!("OAuth/IMAP/SMTP probe completed for {name:?}.");
    Ok(())
}

fn headless_check(config: &Config) -> Result<()> {
    for account in &config.accounts {
        let mut backend = backend::from_account(account)?;
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
        let page = backend.open_mailbox(&target.name, 0, account.max_messages)?;
        let messages = &page.messages;
        let scan_elapsed = scan_start.elapsed();
        let body_metric = if let Some(message) = messages.first() {
            let body_start = Instant::now();
            backend.fetch_body(&target.name, &message.id)?;
            format!(" body={:.3} ms", body_start.elapsed().as_secs_f64() * 1000.0)
        } else {
            String::new()
        };
        println!(
            "{}: {} | connect={:.3} ms list={} folders/{:.3} ms scan={} messages/{:.3} ms ({:.1} msg/s){}",
            account.name,
            backend::redact_source(&account.source),
            connect_elapsed.as_secs_f64() * 1000.0,
            mailboxes.len(),
            list_elapsed.as_secs_f64() * 1000.0,
            messages.len(),
            scan_elapsed.as_secs_f64() * 1000.0,
            if scan_elapsed.is_zero() {
                0.0
            } else {
                messages.len() as f64 / scan_elapsed.as_secs_f64()
            },
            body_metric,
        );
    }
    Ok(())
}

