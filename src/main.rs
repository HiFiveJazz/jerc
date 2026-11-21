use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{ArgAction, Parser};

use jerc::backend;
use jerc::config::Config;
use jerc::runtime::{run_tui_with_options, LaunchOptions};

mod cli_input;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "A keyboard-driven terminal email client",
    disable_version_flag = true,
    disable_help_flag = true
)]
struct Cli {
    /// Print help.
    // These actions exit before storing a value; keep the derived fields optional.
    #[arg(
        short = 'h',
        long,
        action = ArgAction::Help,
        required = false,
        display_order = 0
    )]
    _help: Option<bool>,

    /// Print version.
    #[arg(
        short = 'v',
        long,
        short_alias = 'V',
        action = ArgAction::Version,
        required = false,
        display_order = 1
    )]
    _version: Option<bool>,

    /// Select a configured account (other accounts remain loaded).
    #[arg(short = 'a', long, value_name = "NAME", display_order = 2, conflicts_with_all = ["maildir", "input"])]
    account: Option<String>,

    /// Config file (default: ~/.config/jerc/config.toml).
    #[arg(short = 'c', long, value_name = "FILE", display_order = 3)]
    config: Option<PathBuf>,

    /// Open a Maildir directly, bypassing config.
    #[arg(short = 'm', long, value_name = "DIR", display_order = 4)]
    maildir: Option<PathBuf>,

    /// Name for --maildir.
    #[arg(short = 'n', long, default_value = "Local", display_order = 5)]
    name: String,

    /// Start with cached mail only, without network connections.
    #[arg(long, display_order = 6, conflicts_with_all = ["headless_check", "oauth_login", "oauth_probe", "discover_email"])]
    offline: bool,

    /// Check config and account settings without opening Jerc.
    #[arg(long, display_order = 7, conflicts_with_all = ["oauth_login", "oauth_inspect", "oauth_probe", "oauth_build_info", "discover_email", "headless_check", "maildir", "account", "input", "draft_patch"])]
    check_config: bool,

    /// Open a patch in the composer, without sending it.
    #[arg(long, value_name = "FILE", display_order = 8, conflicts_with_all = ["input", "maildir", "oauth_login", "oauth_probe", "discover_email", "headless_check"])]
    draft_patch: Option<PathBuf>,

    /// Attach a file to a mailto: or --draft-patch composer.
    #[arg(long, value_name = "FILE", display_order = 9)]
    attach: Vec<PathBuf>,

    /// Open .patch, .diff, .eml, .mbox, mailto:URI, or - (stdin).
    #[arg(value_name = "FILE|URI|-", conflicts_with_all = ["maildir", "draft_patch", "oauth_login", "oauth_probe", "discover_email", "headless_check"])]
    input: Option<String>,

    /// Sign in to an OAuth account via browser.
    #[arg(long, help_heading = "OAuth", value_name = "ACCOUNT")]
    oauth_login: Option<String>,

    /// Check embedded OAuth registrations.
    #[arg(long, help_heading = "OAuth", conflicts_with_all = ["maildir", "oauth_login", "oauth_inspect", "oauth_probe", "discover_email", "headless_check"])]
    oauth_build_info: bool,

    /// Inspect OAuth settings without connecting.
    #[arg(long, help_heading = "OAuth", value_name = "ACCOUNT", conflicts_with = "oauth_login")]
    oauth_inspect: Option<String>,

    /// Test OAuth and mail authentication.
    #[arg(long, help_heading = "OAuth", value_name = "ACCOUNT", conflicts_with_all = ["oauth_login", "oauth_inspect"])]
    oauth_probe: Option<String>,

    /// Use two-step SMTP XOAUTH2 in probe.
    #[arg(long, help_heading = "OAuth", requires = "oauth_probe")]
    oauth_probe_smtp_challenge: bool,

    /// Discover email settings; print TOML.
    #[arg(long, help_heading = "Discovery", value_name = "EMAIL", conflicts_with_all = ["maildir", "oauth_login", "oauth_inspect", "oauth_probe", "headless_check"])]
    discover_email: Option<String>,

    /// Choose provider for discovery.
    #[arg(long, help_heading = "Discovery", value_parser = ["auto", "microsoft", "google", "jmap"], requires = "discover_email")]
    discover_provider: Option<String>,

    /// Set discovered IMAP/SMTP login.
    #[arg(
        long,
        help_heading = "Discovery",
        value_name = "LOGIN_UPN",
        requires = "discover_email",
        conflicts_with = "discover_oauth_account"
    )]
    discover_username: Option<String>,

    /// Suggest login from OAuth token.
    #[arg(
        long,
        help_heading = "Discovery",
        value_name = "ACCOUNT",
        requires = "discover_email",
        conflicts_with = "discover_username"
    )]
    discover_oauth_account: Option<String>,

    /// Test accounts and scan speed without TUI.
    #[arg(long, help_heading = "Diagnostics")]
    headless_check: bool,

}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.oauth_build_info {
        // This works on a completely fresh machine; never print client IDs
        // or Google desktop public secrets into terminal logs.
        let google = jerc::oauth::embedded_client_id("google");
        let google_status = match google {
            Some(id) if jerc::oauth::valid_google_desktop_client_id(id) => "embedded",
            Some(_) => "embedded, but not a valid Google desktop client ID",
            None => "not embedded",
        };
        println!("Google OAuth client ID: {google_status}");
        println!(
            "Microsoft OAuth client ID: {}",
            if jerc::oauth::embedded_client_id("microsoft").is_some() {
                "embedded"
            } else {
                "not embedded"
            }
        );
        println!(
            "Google desktop public client secret: {}",
            if jerc::oauth::embedded_google_client_secret().is_some() {
                "embedded"
            } else {
                "not embedded (optional for some registrations)"
            }
        );
        println!(
            "These are compile-time settings. Account-specific OAuth values can override them."
        );
        return Ok(());
    }
    if let Some(email) = &cli.discover_email {
        let mut provider = cli.discover_provider.as_deref().unwrap_or("auto");
        let mut oauth_settings = None;
        let mut configured_logins = (None, None);
        if let Some(name) = &cli.discover_oauth_account {
            let config = Config::load(cli.config.as_deref())?;
            let account = config
                .accounts
                .iter()
                .find(|account| &account.name == name)
                .with_context(|| format!("no configured account named {name:?}"))?;
            anyhow::ensure!(
                account
                    .email
                    .as_deref()
                    .is_some_and(|address| address.eq_ignore_ascii_case(email)),
                "--discover-oauth-account must refer to an account with the same visible email address"
            );
            let settings = jerc::oauth::OAuthSettings::for_account(account)?
                .with_context(|| format!("account {name:?} has no OAuth configuration"))?;
            anyhow::ensure!(
                settings.provider == "microsoft",
                "OAuth identity suggestions currently support Microsoft accounts only"
            );
            if provider == "auto" {
                provider = "microsoft";
            }
            anyhow::ensure!(
                provider == "microsoft",
                "--discover-provider must be microsoft when using a Microsoft OAuth account"
            );
            configured_logins = (account.imap_username.clone(), account.smtp_username.clone());
            oauth_settings = Some(settings);
        }
        let mut proposal = jerc::discovery::discover_with_provider(email, provider)?;
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
                println!(
                    "# Missing login username(s) suggested from Microsoft access-token claims (unverified)."
                );
                println!("# Confirm the UPN against your provider before applying this config.");
            } else if imap_login.is_none() || smtp_login.is_none() {
                println!(
                    "# Access token has no readable login hint; use --discover-username LOGIN_UPN if needed."
                );
            }
        }
        println!("{}", proposal.to_toml());
        return Ok(());
    }
    // CLI readers use a private temporary Maildir. Hold its guard until the
    // terminal exits so it is never accidentally treated as persistent mail.
    let mut temp_maildir: Option<tempfile::TempDir> = None;
    let mut launch = LaunchOptions { account: cli.account.clone(), offline: cli.offline, ..Default::default() };
    let mut config = if cli.check_config {
        anyhow::ensure!(cli.input.is_none() && cli.draft_patch.is_none() && !cli.offline && cli.attach.is_empty(),
            "--check-config cannot be combined with interactive options");
        let config = Config::load(cli.config.as_deref())?;
        let count = jerc::runtime::check_configuration(config)?;
        println!("Configuration OK ({count} accounts)");
        return Ok(());
    } else if cli.input.as_deref().is_some_and(|arg| !arg.starts_with("mailto:")) {
        // Only load UI/contacts preferences: local files must not initialize
        // configured remote accounts, but must retain the selected theme.
        Config::load_preferences_only(cli.config.as_deref())?
    } else {
        load_config(&cli)?
    };

    if let Some(argument) = cli.input.as_deref() {
        if argument.starts_with("mailto:") {
            launch.compose = Some(cli_input::parse_mailto(argument)?);
        } else {
            anyhow::ensure!(cli.attach.is_empty(), "--attach requires mailto: or --draft-patch");
            let (kind, path) = if argument == "-" {
                // Stdin has no suffix: recognize an mbox, Git patch, or RFC 5322
                // message by its content rather than guessing its filename.
                (None, PathBuf::from("-"))
            } else {
                let (kind, path) = cli_input::input_kind(argument)?;
                (Some(kind), path)
            };
            let bytes = cli_input::read_input(&path)?;
            let kind = kind.unwrap_or_else(|| cli_input::detect_stdin_kind(&bytes));
            let title = path.file_name().and_then(|name| name.to_str()).unwrap_or("stdin.patch");
            let temporary = cli_input::prepare_reader(kind, &bytes, title)?;
            // Replace accounts only; keep the user's theme, layout and config path.
            config.accounts = Config::single_maildir(
                "Local file".to_owned(),
                temporary.path().to_path_buf(),
            )
            .accounts;
            temp_maildir = Some(temporary);
            launch.account = None;
            launch.open_first_message = kind != cli_input::InputKind::Mbox;
        }
    }
    if let Some(path) = cli.draft_patch.as_deref() {
        let bytes = cli_input::read_input(path)?;
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("stdin.patch");
        launch.compose = Some(cli_input::patch_draft(&bytes, name)?);
    }
    if !cli.attach.is_empty() {
        anyhow::ensure!(launch.compose.is_some(), "--attach requires mailto: or --draft-patch");
        launch.compose.as_mut().expect("validated composer").attachments = cli_input::validate_attachments(&cli.attach)?;
    }
    if launch.compose.is_some() && config.accounts.is_empty() {
        let temporary = cli_input::empty_maildir()?;
        config.accounts = Config::single_maildir(
            "Draft".to_owned(),
            temporary.path().to_path_buf(),
        )
        .accounts;
        temp_maildir = Some(temporary);
    }
    if let Some(name) = launch.account.as_deref() {
        anyhow::ensure!(config.accounts.iter().any(|account| account.name == name),
            "no configured account named {name:?}");
    }
    if let Some(name) = &cli.oauth_login {
        return jerc::oauth::login_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_inspect {
        return jerc::oauth::inspect_account(&config.accounts, name);
    }
    if let Some(name) = &cli.oauth_probe {
        return oauth_probe(&config, name, cli.oauth_probe_smtp_challenge);
    }
    if cli.headless_check {
        return headless_check(&config);
    }
    // The TempDir guard outlives run_tui. Do not drop it before the reader
    // closes or its temporary Maildir would disappear during navigation.
    let result = run_tui_with_options(config, launch);
    drop(temp_maildir);
    result
}

fn load_config(cli: &Cli) -> Result<Config> {
    if let Some(maildir) = &cli.maildir {
        let mut config = Config::load_preferences_only(cli.config.as_deref())?;
        config.accounts = Config::single_maildir(cli.name.clone(), maildir.clone()).accounts;
        return Ok(config);
    }
    if cli.oauth_login.is_none()
        && cli.oauth_inspect.is_none()
        && cli.oauth_probe.is_none()
        && !cli.headless_check
    {
        Config::load_or_empty(cli.config.as_deref())
    } else {
        Config::load(cli.config.as_deref())
    }
}

fn oauth_probe(config: &Config, name: &str, smtp_challenge: bool) -> Result<()> {
    let account = config
        .accounts
        .iter()
        .find(|account| account.name == name)
        .with_context(|| format!("no account named {name:?} in config"))?;
    let settings = jerc::oauth::OAuthSettings::for_account(account)?
        .with_context(|| format!("account {name:?} has no [accounts.oauth] configuration"))?;
    println!("Testing OAuth for {name:?} (no email will be sent)");

    // Refresh the token and inspect the response's scope metadata without
    // exposing bearer tokens, refresh tokens, or the full response payload.
    let reported_scopes = settings
        .reported_protocol_scopes()
        .context("refreshing OAuth token / inspecting reported scopes")?;
    println!("  OAuth refresh: OK");
    if settings.provider == "microsoft" {
        match reported_scopes {
            Some((imap, smtp)) => println!(
                "  Token response reports IMAP permission: {} | SMTP.Send: {}",
                if imap { "yes" } else { "no" },
                if smtp { "yes" } else { "no" },
            ),
            None => println!("  Token response did not include scope metadata (inconclusive)"),
        }
    }

    let mut backend = backend::from_account(account)?;
    backend
        .connect()
        .context("connecting/authenticating to IMAP using XOAUTH2")?;
    println!("  IMAP XOAUTH2: OK");
    let mailboxes = backend.list_mailboxes().context("listing IMAP mailboxes")?;
    println!("  IMAP mailbox LIST: OK ({} folders)", mailboxes.len());

    if let Some(outgoing) = account.outgoing.as_deref() {
        if outgoing.starts_with("smtp://")
            || outgoing.starts_with("smtps://")
            || outgoing.starts_with("smtp+")
            || outgoing.starts_with("smtps+")
        {
            let source =
                jerc::outgoing::SmtpSource::parse(outgoing)?.with_account_oauth(account)?;
            let sender = jerc::outgoing::SmtpSender::new(source, account.outgoing_cred_cmd.clone());
            if smtp_challenge {
                println!("  SMTP OAuth mode: two-step XOAUTH2 challenge/response");
            }
            sender
                .probe_oauth_with_challenge(smtp_challenge)
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
            format!(
                " body={:.3} ms",
                body_start.elapsed().as_secs_f64() * 1000.0
            )
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

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn help_shows_all_options_with_common_flags_first() {
        for flag in ["-h", "--help"] {
            let err = Cli::try_parse_from(["jerc", flag]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::DisplayHelp);
            let help = err.to_string();
            for option in ["-c, --config", "-m, --maildir", "-n, --name", "-v, --version"] {
                assert!(help.contains(option), "missing {option}: {help}");
            }
            for option in [
                "--oauth-login", "--oauth-build-info", "--oauth-inspect",
                "--oauth-probe", "--oauth-probe-smtp-challenge",
                "--discover-email", "--discover-provider", "--discover-username",
                "--discover-oauth-account", "--headless-check",
            ] {
                assert!(help.contains(option), "missing {option}: {help}");
            }
            assert!(!help.contains("--help-all"));
            let order = [
                "-h, --help",
                "-v, --version",
                "-c, --config",
                "-m, --maildir",
                "-n, --name",
            ];
            for pair in order.windows(2) {
                let first = help.find(pair[0]).expect("missing common option");
                let second = help.find(pair[1]).expect("missing common option");
                assert!(
                    first < second,
                    "{} should appear before {}: {help}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn unix_style_entry_points_parse_without_shifting_existing_flags() {
        assert_eq!(Cli::try_parse_from(["jerc", "-a", "Personal"]).unwrap().account.as_deref(), Some("Personal"));
        assert!(Cli::try_parse_from(["jerc", "--offline"]).unwrap().offline);
        assert!(Cli::try_parse_from(["jerc", "--check-config"]).unwrap().check_config);
        assert_eq!(Cli::try_parse_from(["jerc", "a.patch"]).unwrap().input.as_deref(), Some("a.patch"));
        assert_eq!(Cli::try_parse_from(["jerc", "--draft-patch", "a.patch"]).unwrap().draft_patch, Some(PathBuf::from("a.patch")));
        assert_eq!(Cli::try_parse_from(["jerc", "mailto:a@example.org", "--attach", "a.txt"]).unwrap().attach, vec![PathBuf::from("a.txt")]);
        assert_eq!(Cli::try_parse_from(["jerc", "-"]).unwrap().input.as_deref(), Some("-"));
        assert!(Cli::try_parse_from(["jerc", "--draft-patch", "a.patch", "b.eml"]).is_err());
    }

    #[test]
    fn version_is_available_on_both_short_flags_and_long_flag() {
        for flag in ["-v", "-V", "--version"] {
            let err = Cli::try_parse_from(["jerc", flag]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::DisplayVersion, "{flag}");
            assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
        }
    }

    #[test]
    fn legacy_diagnostics_and_short_options_remain_accepted() {
        // Neither help nor version should be a required option.
        assert!(Cli::try_parse_from(["jerc"]).is_ok());
        assert_eq!(
            Cli::try_parse_from(["jerc", "-c", "custom.toml"])
                .unwrap()
                .config,
            Some(PathBuf::from("custom.toml")),
        );
        assert_eq!(
            Cli::try_parse_from(["jerc", "-m", "Maildir", "-n", "LocalMail"])
                .unwrap()
                .name,
            "LocalMail",
        );
        assert_eq!(
            Cli::try_parse_from(["jerc", "--oauth-probe", "Personal"])
                .unwrap()
                .oauth_probe
                .as_deref(),
            Some("Personal"),
        );
        assert_eq!(
            Cli::try_parse_from(["jerc", "--help-all"])
                .unwrap_err()
                .kind(),
            ErrorKind::UnknownArgument
        );
    }
}
