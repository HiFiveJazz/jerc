use std::io::{self, Stdout, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, KeyEventKind};
use crossterm::cursor::SetCursorStyle;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;

use crate::app::App;
use crate::backend;
use crate::config::Config;
use crate::outgoing;
use crate::ui;
use crate::worker::{RuntimeEvent, WorkerHandle};


pub fn run_tui(config: Config) -> Result<()> {
    let (runtime_tx, runtime_rx) = sync_channel::<RuntimeEvent>(4096);
    let workers = create_workers(&config, runtime_tx.clone())?;
    let mut app = App::new(&config, workers);
    app.bootstrap();

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
    spawn_input_thread(runtime_tx);

    // Redraw once with the resolved image protocol before entering the event loop.
    render(&mut terminal, &mut app)?;
    event_loop(&mut terminal, &mut app, runtime_rx)?;
    Ok(())
}

fn create_workers(config: &Config, runtime_tx: SyncSender<RuntimeEvent>) -> Result<Vec<WorkerHandle>> {
    config
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let backend = backend::from_account(account)?;
            let sender = outgoing::from_account(account)?;
            Ok(WorkerHandle::spawn(
                index,
                backend,
                sender,
                runtime_tx.clone(),
            ))
        })
        .collect()
}

fn spawn_input_thread(runtime_tx: SyncSender<RuntimeEvent>) {
    thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || loop {
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
        })
        .expect("failed to spawn terminal input thread");
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    runtime_rx: Receiver<RuntimeEvent>,
) -> Result<()> {
    // Animation is deadline-driven instead of timeout-driven.  The old loop only
    // advanced smooth scrolling when recv_timeout() *timed out*, so a steady
    // stream of worker events (mailbox STATUS refreshes are a common example)
    // could starve Ctrl-j/Ctrl-k for hundreds of milliseconds.
    let mut next_animation_frame = Instant::now();
    let mut next_inbox_poll = Instant::now() + app.inbox_poll_interval();

    while !app.should_quit {
        let now = Instant::now();
        if now >= next_inbox_poll {
            app.poll_inboxes();
            next_inbox_poll = now + app.inbox_poll_interval();
        }
        if app.animation_active() {
            let now = Instant::now();
            if now >= next_animation_frame {
                app.advance_animation();
                next_animation_frame = now + app.animation_frame_interval();
                render(terminal, app)?;
                continue;
            }
        } else {
            next_animation_frame = Instant::now();
        }

        let deadline = if app.animation_active() {
            next_animation_frame.min(next_inbox_poll)
        } else {
            next_inbox_poll
        };
        let received = match runtime_rx.recv_timeout(
            deadline.saturating_duration_since(Instant::now())
        ) {
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

        match event {
            RuntimeEvent::Input(event::Event::Key(key))
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                app.handle_key(key);
            }
            RuntimeEvent::Input(event::Event::Key(_)) => continue,
            RuntimeEvent::Input(event::Event::Resize(_, _)) => {}
            RuntimeEvent::Input(_) => continue,
            RuntimeEvent::Worker(event) => app.handle_worker_event(event),
            RuntimeEvent::MailImage(event) => app.handle_mail_image_event(event),
            RuntimeEvent::InputStopped => break,
        }

        // The desktop notification server owns the themed sound, not the
        // terminal. Never invoke a shell or block the TUI on a notification.
        while let Some((account, count)) = app.take_new_mail_notice() {
            notify_new_mail(&account, count, app.new_mail_sound_name());
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
        render(terminal, app)?;
    }
    Ok(())
}

/// Fire-and-forget freedesktop notification. `notify-send` is the Wayland/X11
/// transport; the sound name is resolved by the user's active sound theme.
/// Never print GTK warnings over the alternate screen.
fn notify_new_mail(account: &str, count: usize, sound: &str) {
    let title = if count == 1 { "New email" } else { "New emails" };
    let body = format!("{count} new message(s) in {account}");
    let mut command = ProcessCommand::new("notify-send");
    command.args(["--app-name=aerc-rs", "--icon=mail-unread"])
        .arg("--hint")
        .arg(format!("string:sound-name:{sound}"))
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = command.spawn();
}

fn render(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, SetCursorStyle::SteadyBar)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape, LeaveAlternateScreen);
    }
}
