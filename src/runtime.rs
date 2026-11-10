use std::io::{self, Stdout, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;

use crate::app::{App, NewMailNotice};
use crate::backend;
use crate::config::Config;
use crate::outgoing;
use crate::ui;
use crate::worker::{RuntimeEvent, WorkerHandle};

pub fn run_tui(config: Config) -> Result<()> {
    let (config, configuration_warnings) = validate_account_configuration(config)?;
    let (runtime_tx, runtime_rx) = sync_channel::<RuntimeEvent>(4096);
    let workers = create_workers(&config, runtime_tx.clone())?;
    let mut app = App::new(&config, workers);
    app.bootstrap();
    app.attach_setup_sender(runtime_tx.clone());
    app.startup_warnings.extend(configuration_warnings);
    if !app.startup_warnings.is_empty() {
        app.status = crate::app::StatusLine::error(format!(
            "{} account configuration issue(s) — press ! for details",
            app.startup_warnings.len()
        ));
    }

    let _terminal_guard = TerminalGuard::enter()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Paint the application shell immediately. Terminal image capability
    // detection is one of the few startup operations that must synchronously
    // read stdio before our input thread starts, and some terminals take a few
    // hundred milliseconds to answer. First paint should not be held hostage by
    // that query; worker results can queue safely while it completes.
    render(&mut terminal, &mut app)?;

    // ratatui-image must query terminal graphics capabilities after entering
    // the alternate screen but before the input thread starts consuming escape
    // sequence responses. Fall back to portable half-block rendering when the
    // terminal does not answer the protocol query.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    app.configure_mail_images(picker, runtime_tx.clone());
    spawn_input_thread(runtime_tx.clone());

    // Redraw once with the resolved image protocol before entering the event loop.
    render(&mut terminal, &mut app)?;
    // Quit must not wait for a busy IMAP SELECT/FETCH or for a full worker
    // queue. Keep the normal graceful worker shutdown for account reloads,
    // but detach network workers on final process exit (including UI errors).
    let result = event_loop(&mut terminal, &mut app, runtime_rx, runtime_tx);
    app.detach_network_workers_for_exit();
    result
}

/// Invalid account URIs must not keep correctly configured accounts from
/// opening. In particular, a bad SMTP URL disables only sending on that
/// account; receiving remains available. Never include URL credentials or
/// command values in diagnostics rendered by the TUI.
fn validate_account_configuration(mut config: Config) -> Result<(Config, Vec<String>)> {
    let mut warnings = Vec::new();
    config.accounts.retain_mut(|account| {
        if backend::from_account(account).is_err() {
            if account.source.trim().is_empty() || matches!(account.source.as_str(), "imap" | "imaps") {
                warnings.push(format!(
                    "{}: incoming IMAP shorthand could not be configured. Set email and, \
                     for a custom provider, imap_host; or use a full imaps:// URI. Account skipped.",
                    account.name,
                ));
            } else {
                let scheme = safe_scheme(&account.source);
                warnings.push(format!(
                    "{}: incoming source uses invalid or unsupported scheme '{scheme}'. \
                     Use maildir://, imaps://, imaps+login://, imaps+oauthbearer://, or imaps+xoauth2://. Account skipped.",
                    account.name,
                ));
            }
            return false;
        }
        if outgoing::from_account(account).is_err() {
            if account.outgoing.as_deref().is_some_and(|outgoing| matches!(outgoing, "smtp" | "smtps" | "starttls")) {
                warnings.push(format!(
                    "{}: outgoing SMTP shorthand could not be configured. Set email and, \
                     for a custom provider, smtp_host; or use a full smtps:// or smtp:// URI. Receiving works; sending disabled until fixed.",
                    account.name,
                ));
            } else {
                let scheme = account.outgoing.as_deref().map(safe_scheme).unwrap_or("<missing>");
                warnings.push(format!(
                    "{}: outgoing transport '{scheme}' is invalid. Use smtp:// or smtps:// \
                     (optional +login, +plain, +xoauth2 or +oauthbearer), or an absolute sendmail path. Receiving works; sending disabled until fixed.",
                    account.name,
                ));
            }
            account.outgoing = None;
        }
        true
    });
    if config.accounts.is_empty() && !warnings.is_empty() {
        return Err(anyhow!(
            "no usable incoming accounts; check [[accounts]].source in config.toml ({} account(s) invalid)",
            warnings.len(),
        ));
    }
    Ok((config, warnings))
}

fn safe_scheme(uri: &str) -> &str {
    let scheme = uri
        .split_once("://")
        .map_or("<invalid>", |(scheme, _)| scheme);
    // Never display an entire malformed URI: it may include a password.
    if scheme.len() > 32
        || !scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "+-._".contains(ch))
    {
        "<invalid>"
    } else {
        scheme
    }
}

fn create_workers(
    config: &Config,
    runtime_tx: SyncSender<RuntimeEvent>,
) -> Result<Vec<WorkerHandle>> {
    config
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let backend = backend::from_account(account)?;
            // STATUS and preview preloading share a lazy background session.
            // Maildir uses a separate inexpensive local worker as well, so
            // disk reads cannot stall interactive navigation.
            let status_backend = Some(backend::from_account(account)?);
            let idle_backend = if backend::ImapSource::supports(&account.source) {
                Some(backend::from_account(account)?)
            } else {
                None
            };
            let sender = outgoing::from_account(account)?;
            Ok(WorkerHandle::spawn_with_idle(
                index,
                backend,
                status_backend,
                idle_backend,
                sender,
                runtime_tx.clone(),
            ))
        })
        .collect()
}

fn spawn_input_thread(runtime_tx: SyncSender<RuntimeEvent>) {
    thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || {
            loop {
                match event::read() {
                    Ok(event) => {
                        if runtime_tx.send(RuntimeEvent::Input(event)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = runtime_tx.send(RuntimeEvent::InputStopped);
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn terminal input thread");
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    runtime_rx: Receiver<RuntimeEvent>,
    runtime_tx: SyncSender<RuntimeEvent>,
) -> Result<()> {
    // Animation is deadline-driven instead of timeout-driven.  The old loop only
    // advanced smooth scrolling when recv_timeout() *timed out*, so a steady
    // stream of worker events (mailbox STATUS refreshes are a common example)
    // could starve Ctrl-j/Ctrl-k for hundreds of milliseconds.
    let mut next_animation_frame = Instant::now();
    let mut next_setup_spinner_frame = Instant::now();
    let mut next_inbox_poll = Instant::now() + app.inbox_poll_interval();

    while !app.should_quit {
        app.flush_contacts(false);
        let now = Instant::now();
        if now >= next_inbox_poll {
            app.poll_inboxes();
            next_inbox_poll = now + app.inbox_poll_interval();
        }
        if app.animation_active() {
            let now = Instant::now();
            if now >= next_animation_frame {
                let tick = Instant::now();
                app.advance_animation();
                next_animation_frame = now + app.animation_frame_interval();
                render(terminal, app)?;
                // Includes animation-state updates and terminal drawing. The
                // separate render marker isolates the paint/layout portion.
                crate::perf::record("ui_animation_tick", None, tick.elapsed(), true);
                continue;
            }
        } else {
            next_animation_frame = Instant::now();
        }

        // Browser OAuth and provider discovery happen on worker threads. They
        // may generate no terminal events for many seconds, so animate their
        // progress indicator on its own deadline (without blocking input).
        let setup_busy = app
            .setup
            .as_ref()
            .is_some_and(|wizard| wizard.authenticating || wizard.discovering);
        if setup_busy {
            if Instant::now() >= next_setup_spinner_frame {
                if let Some(wizard) = app.setup.as_mut() {
                    wizard.spin();
                }
                next_setup_spinner_frame = Instant::now() + Duration::from_millis(120);
                render(terminal, app)?;
                continue;
            }
        } else {
            next_setup_spinner_frame = Instant::now();
        }

        let deadline = if app.animation_active() {
            next_animation_frame.min(next_inbox_poll)
        } else {
            next_inbox_poll
        };
        let deadline = if setup_busy {
            deadline.min(next_setup_spinner_frame)
        } else {
            deadline
        };
        let deadline = app
            .contact_save_deadline()
            .map_or(deadline, |due| deadline.min(due));
        let received =
            match runtime_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("runtime event channel closed"));
                }
            };

        let Some(event) = received else {
            // Do not advance here. The deadline check at the top of the next
            // iteration owns animation progress, which also prevents event load
            // from postponing a due frame.
            continue;
        };

        let background_update =
            matches!(&event, RuntimeEvent::Worker(_) | RuntimeEvent::MailImage(_));
        match event {
            RuntimeEvent::Input(event::Event::Key(key))
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                app.handle_key(key);
            }
            RuntimeEvent::Input(event::Event::Key(_)) => continue,
            RuntimeEvent::Input(event::Event::Resize(_, _)) => {
                app.invalidate_attachment_preview_layout();
            }
            RuntimeEvent::Input(event::Event::Mouse(mouse)) => {
                let size = terminal.size()?;
                app.handle_mouse(mouse, size.width, size.height);
            }
            RuntimeEvent::Input(_) => continue,
            RuntimeEvent::Worker(event) => app.handle_worker_event(event),
            RuntimeEvent::MailImage(event) => app.handle_mail_image_event(event),
            RuntimeEvent::SetupDiscovered { email, result } => app.setup_discovered(email, result),
            RuntimeEvent::SetupLoginFinished(result) => app.setup_login_finished(result),
            RuntimeEvent::InputStopped => break,
        }

        if std::mem::take(&mut app.setup_reload_requested) {
            let result = Config::load_or_empty(Some(&app.setup_config_path))
                .and_then(validate_account_configuration)
                .and_then(|(config, warnings)| {
                    let workers = create_workers(&config, runtime_tx.clone())?;
                    Ok((config, workers, warnings))
                });
            match result {
                Ok((config, workers, warnings)) => {
                    // Old mail workers finish before we accept events from new
                    // account indices, preventing stale updates after reload.
                    app.flush_contacts(true);
                    let mut fresh = App::new(&config, workers);
                    fresh.attach_setup_sender(runtime_tx.clone());
                    fresh.account_index = fresh.accounts.len().saturating_sub(1);
                    fresh.startup_warnings.extend(warnings);
                    let mut old = std::mem::replace(app, fresh);
                    app.inherit_image_runtime(&mut old);
                    drop(old);
                    while runtime_rx.try_recv().is_ok() {}
                    app.bootstrap();
                    render(terminal, app)?;
                    continue;
                }
                Err(error) => {
                    if let Some(wizard) = app.setup.as_mut() {
                        wizard.error = true;
                        wizard.message = format!("Saved, but could not load account: {error:#}");
                    }
                }
            }
        }

        // The desktop notification server owns the themed sound, not the
        // terminal. Never invoke a shell or block the TUI on a notification.
        while let Some(notice) = app.take_new_mail_notice() {
            notify_new_mail(
                &notice,
                app.new_mail_sound_name(),
                app.new_mail_sound_file(),
            );
        }
        if app.take_new_mail_bell() {
            // Terminal bell: honors the user's terminal bell settings and
            // requires no external audio process or extra dependencies.
            io::stdout().write_all(b"\x07")?;
            io::stdout().flush()?;
        }

        // A key can start or reverse an animation. Draw its state immediately,
        // then make its first row eligible without waiting on unrelated traffic.
        if app.animation_active() && next_animation_frame <= Instant::now() {
            app.advance_animation();
            next_animation_frame = Instant::now() + app.animation_frame_interval();
        }
        // Worker completions may arrive in bursts while preloading bodies.
        // The animation deadline will paint them on the next ~100 Hz frame;
        // rendering once *per* completion can overwhelm a terminal and make
        // Ctrl-j/k stutter even though the mailbox worker is asynchronous.
        if background_update && app.animation_active() {
            continue;
        }
        render(terminal, app)?;
    }
    app.flush_contacts(true);
    Ok(())
}

/// Fire-and-forget freedesktop notification. `notify-send` is the Wayland/X11
/// transport; the sound name is resolved by the user's active sound theme.
/// Never print GTK warnings over the alternate screen.
fn notification_excerpt(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut excerpt = normalized.chars().take(limit).collect::<String>();
    if normalized.chars().count() > limit {
        excerpt.push('…');
    }
    excerpt
}

fn notification_text(notice: &NewMailNotice) -> (String, String) {
    let subject = notification_excerpt(&notice.subject, 100);
    let title = if subject.is_empty() {
        format!("New email · {}", notice.account)
    } else {
        subject
    };
    let mut lines = Vec::new();
    if !notice.sender.is_empty() {
        lines.push(format!(
            "From: {}",
            notification_excerpt(&notice.sender, 80)
        ));
    }
    let excerpt = notification_excerpt(&notice.excerpt, 180);
    if !excerpt.is_empty() {
        lines.push(excerpt);
    }
    if notice.count > 1 {
        lines.push(format!(
            "+{} more new messages · {}",
            notice.count - 1,
            notice.account
        ));
    }
    if lines.is_empty() {
        lines.push(format!(
            "{} new message(s) in {}",
            notice.count, notice.account
        ));
    }
    (title, lines.join("\n"))
}

fn notify_new_mail(notice: &NewMailNotice, sound: &str, sound_file: Option<&str>) {
    let (title, body) = notification_text(notice);
    let sound_file = sound_file.filter(|file| std::path::Path::new(file).is_file());
    let mut command = ProcessCommand::new("notify-send");
    command.args(["--app-name=jerc", "--icon=mail-unread"]);
    // Explicit file playback and notification-server playback are mutually
    // exclusive to avoid playing the same alert twice.
    if sound_file.is_none() && !sound.trim().is_empty() {
        command
            .arg("--hint")
            .arg(format!("string:sound-name:{sound}"));
    }
    // Prevent untrusted subjects beginning with '-' from becoming CLI options.
    command
        .arg("--")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = command.spawn();
    if let Some(file) = sound_file {
        // Never shell out. If PipeWire's pw-play is absent, try PulseAudio's
        // compatible paplay command. Both run detached from terminal redraw.
        for player in ["pw-play", "paplay"] {
            if ProcessCommand::new(player)
                .arg(file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
}

fn render(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let started = Instant::now();
    let result = terminal.draw(|frame| ui::draw(frame, app));
    // Opt-in frame timing distinguishes terminal paint/layout jank from slow
    // network fetches. No message content or account information is logged.
    crate::perf::record("ui_render_frame", None, started.elapsed(), result.is_ok());
    result?;
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            SetCursorStyle::SteadyBar
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
    }
}

#[cfg(test)]
mod multi_account_tests {
    use super::{safe_scheme, validate_account_configuration};
    use crate::config::Config;

    #[test]
    fn malformed_smtp_disables_sending_without_disabling_receiving() {
        let config: Config = toml::from_str(
            r#"
            [[accounts]]
            name = "Primary"
            source = "maildir:///tmp/primary"
            outgoing = "smtps://user:password@smtp.example.net:465"
            [[accounts]]
            name = "Secondary"
            source = "maildir:///tmp/secondary"
            outgoing = "imaps://sensitive:secret@imap.example.net:993"
        "#,
        )
        .unwrap();
        let (validated, errors) = validate_account_configuration(config).unwrap();
        assert_eq!(validated.accounts.len(), 2);
        assert!(validated.accounts[0].outgoing.is_some());
        assert!(validated.accounts[1].outgoing.is_none());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Secondary"));
        assert!(errors[0].contains("receiving works") || errors[0].contains("Receiving works"));
        assert!(!errors.join(" ").contains("secret"));
    }

    #[test]
    fn invalid_incoming_skips_only_that_account() {
        let config: Config = toml::from_str(
            r#"
            [[accounts]]
            name = "Broken"
            source = "pop3s://user:secret@server.example.net"
            [[accounts]]
            name = "Working"
            source = "maildir:///tmp/working"
        "#,
        )
        .unwrap();
        let (validated, warnings) = validate_account_configuration(config).unwrap();
        assert_eq!(validated.accounts.len(), 1);
        assert_eq!(validated.accounts[0].name, "Working");
        assert!(warnings[0].contains("Broken"));
        assert!(!warnings[0].contains("secret"));
    }

    #[test]
    fn invalid_shorthand_does_not_block_other_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            [[accounts]]
            name = "Broken"
            email = "person@custom.example"
            incoming = "imap"
            outgoing = "smtp"
            [[accounts]]
            name = "Working"
            email = "worker@gmail.com"
            incoming = "imap"
            outgoing = "smtp"
        "#,
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        let (validated, warnings) = validate_account_configuration(config).unwrap();
        assert_eq!(validated.accounts.len(), 1);
        assert_eq!(validated.accounts[0].name, "Working");
        assert!(warnings[0].contains("imap_host"));
    }

    #[test]
    fn no_valid_incoming_accounts_returns_safe_error() {
        let config: Config = toml::from_str(
            r#"
            [[accounts]]
            name = "Broken"
            source = "pop3s://user:secret@server.example.net"
        "#,
        )
        .unwrap();
        let error = match validate_account_configuration(config) {
            Ok(_) => panic!("expected invalid config"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no usable incoming accounts"));
        assert!(!error.contains("secret"));
        assert_eq!(safe_scheme("imaps://username:secret@host"), "imaps");
        assert_eq!(safe_scheme("username:secret@host"), "<invalid>");
    }
}

#[cfg(test)]
mod new_mail_notification_tests {
    use super::{notification_excerpt, notification_text};
    use crate::app::NewMailNotice;

    #[test]
    fn includes_sender_subject_and_body_without_newlines_from_content() {
        let notice = NewMailNotice {
            account: "University".into(),
            count: 2,
            sender: "Professor\nSmith".into(),
            subject: "Office hours".into(),
            excerpt: "Please   review\n the notes.".into(),
        };
        let (title, body) = notification_text(&notice);
        assert_eq!(title, "Office hours");
        assert!(body.contains("From: Professor Smith"));
        assert!(body.contains("Please review the notes."));
        assert!(body.contains("+1 more"));
    }

    #[test]
    fn long_preview_is_bounded_by_chars() {
        assert!(notification_excerpt(&"界".repeat(240), 180).chars().count() <= 181);
    }
}
