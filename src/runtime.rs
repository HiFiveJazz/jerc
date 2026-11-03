use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, KeyEventKind};
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

const ANIMATION_FRAME: Duration = Duration::from_millis(10);

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
    // ratatui-image must query terminal graphics capabilities after entering
    // the alternate screen but before the input thread starts consuming escape
    // sequence responses. Fall back to portable half-block rendering when the
    // terminal does not answer the protocol query.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    app.configure_mail_images(picker, runtime_tx.clone());
    spawn_input_thread(runtime_tx);

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

    while !app.should_quit {
        if app.animation_active() {
            let now = Instant::now();
            if now >= next_animation_frame {
                app.advance_animation();
                next_animation_frame = now + ANIMATION_FRAME;
                render(terminal, app)?;
                continue;
            }
        } else {
            next_animation_frame = Instant::now();
        }

        let received = if app.animation_active() {
            let timeout = next_animation_frame.saturating_duration_since(Instant::now());
            match runtime_rx.recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("runtime event channel closed"));
                }
            }
        } else {
            Some(runtime_rx.recv().context("runtime event channel closed")?)
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

        // A key can start or reverse an animation. Draw its state immediately,
        // then make its first row eligible without waiting on unrelated traffic.
        if app.animation_active() && next_animation_frame <= Instant::now() {
            app.advance_animation();
            next_animation_frame = Instant::now() + ANIMATION_FRAME;
        }
        render(terminal, app)?;
    }
    Ok(())
}

fn render(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
