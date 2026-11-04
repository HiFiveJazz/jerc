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

    /// Try Microsoft's documented two-step XOAUTH2 challenge/response during
    /// the no-send SMTP probe. Does not change ordinary sending behavior.
    #[arg(long, requires = "oauth_probe")]
    oauth_probe_smtp_challenge: bool,

    /// Discover secure IMAP/SMTP settings using provider HTTPS autoconfig and Thunderbird ISPDB.
    /// Prints a proposed TOML account without modifying your config or credentials.
    #[arg(long, value_name = "EMAIL_ADDRESS", conflicts_with_all = ["maildir", "oauth_login", "oauth_inspect", "oauth_probe", "headless_check"])]
    discover_email: Option<String>,

    /// Select a known provider when domain discovery is missing or ambiguous.
    /// Does not confirm account ownership or bypass OAuth approval.
    #[arg(long, value_parser = ["auto", "microsoft", "google"], requires = "discover_email")]
    discover_provider: Option<String>,

    /// Suggest a separate IMAP/SMTP username in the generated config (no OAuth request).
    #[arg(long, value_name = "LOGIN_UPN", requires = "discover_email", conflicts_with = "discover_oauth_account")]
    discover_username: Option<String>,

    /// Suggest a Microsoft login from a configured account's OAuth token.
    /// Requires a previous --oauth-login; never changes account config.
    #[arg(long, value_name = "ACCOUNT_NAME", requires = "discover_email", conflicts_with = "discover_username")]
    discover_oauth_account: Option<String>,

    /// Validate accounts and benchmark folder/header scanning without opening the TUI.
    #[arg(long)]
    headless_check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(email) = &cli.discover_email {
        let mut provider = cli.discover_provider.as_deref().unwrap_or("auto");
        let mut oauth_settings = None;
        let mut configured_logins = (None, None);
        if let Some(name) = &cli.discover_oauth_account {
            let config = Config::load(cli.config.as_deref())?;
            let account = config.accounts.iter().find(|account| &account.name == name)
                .with_context(|| format!("no configured account named {name:?}"))?;
            anyhow::ensure!(
                account.email.as_deref().is_some_and(|address| address.eq_ignore_ascii_case(email)),
                "--discover-oauth-account must refer to an account with the same visible email address"
            );
            let settings = aerc_rs::oauth::OAuthSettings::for_account(account)?
                .with_context(|| format!("account {name:?} has no OAuth configuration"))?;
            anyhow::ensure!(settings.provider == "microsoft",
                "OAuth identity suggestions currently support Microsoft accounts only");
            if provider == "auto" { provider = "microsoft"; }
            anyhow::ensure!(provider == "microsoft",
                "--discover-provider must be microsoft when using a Microsoft OAuth account");
            configured_logins = (account.imap_username.clone(), account.smtp_username.clone());
            oauth_settings = Some(settings);
        }
        let mut proposal = aerc_rs::discovery::discover_with_provider(email, provider)?;
        if let Some(username) = &cli.discover_username {
            proposal = proposal.with_login_username(username)?;
            println!("# Login username supplied explicitly; verify before applying.");
        }
        if let Some(settings) = oauth_settings {
            let (imap_login, smtp_login) = configured_logins;
            // Explicit configuration takes precedence over unverified claims.
            // Only refresh the OAuth token if at least one login is unknown.
            let hint = if imap_login.is_some() && smtp_login.is_some() {
                None
            } else {
                settings.suggested_login_username()?
            };
            proposal = proposal.with_login_usernames(
                imap_login.as_deref().or(hint.as_deref()),
                smtp_login.as_deref().or(hint.as_deref()),
            )?;
            if imap_login.is_some() || smtp_login.is_some() {
                println!("# Existing explicitly configured IMAP/SMTP usernames preserved.");
            }
            if hint.is_some() {
                println!("# Missing login username(s) suggested from Microsoft access-token claims (unverified).");
                println!("# Confirm the UPN against your provider before applying this config.");
            } else if imap_login.is_none() || smtp_login.is_none() {
                println!("# Access token has no readable login hint; use --discover-username LOGIN_UPN if needed.");
            }
        }
        println!("{}", proposal.to_toml());
        return Ok(());
    }
    let config = load_config(&cli)?;
    if let Some(name) = &cli.oauth_login {
        return aerc_rs::oauth::login_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_inspect {
        return aerc_rs::oauth::inspect_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_probe {
        return oauth_probe(&config, name, cli.oauth_probe_smtp_challenge);
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
    if cli.oauth_login.is_none() && cli.oauth_inspect.is_none()
        && cli.oauth_probe.is_none() && !cli.headless_check
    {
        Config::load_or_empty(cli.config.as_deref())
    } else {
        Config::load(cli.config.as_deref())
    }
}

fn oauth_probe(config: &Config, name: &str, smtp_challenge: bool) -> Result<()> {
    let account = config.accounts.iter().find(|account| account.name == name)
        .with_context(|| format!("no account named {name:?} in config"))?;
    let settings = aerc_rs::oauth::OAuthSettings::for_account(account)?
        .with_context(|| format!("account {name:?} has no [accounts.oauth] configuration"))?;
    println!("Testing OAuth for {name:?} (no email will be sent)");

    // Refresh the token and inspect the response's scope metadata without
    // exposing bearer tokens, refresh tokens, or the full response payload.
    let reported_scopes = settings.reported_protocol_scopes()
        .context("refreshing OAuth token / inspecting reported scopes")?;
    println!("  OAuth refresh: OK");
    if settings.provider == "microsoft" {
        match reported_scopes {
            Some((imap, smtp)) => println!(
                "  Token response reports IMAP permission: {} | SMTP.Send: {}",
                if imap { "yes" } else { "no" },
                if smtp { "yes" } else { "no" },
            ),
            None => println!(
                "  Token response did not include scope metadata (inconclusive)"
            ),
        }
    }

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
            if smtp_challenge {
                println!("  SMTP OAuth mode: two-step XOAUTH2 challenge/response");
            }
            sender.probe_oauth_with_challenge(smtp_challenge)
                .context("authenticating to SMTP without sending a message")?;
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

