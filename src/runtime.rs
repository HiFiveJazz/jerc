use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

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
    spawn_input_thread(runtime_tx);

    render(&mut terminal, &app)?;
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
    while !app.should_quit {
        let received = if app.animation_active() {
            match runtime_rx.recv_timeout(ANIMATION_FRAME) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => {
                    app.advance_animation();
                    render(terminal, app)?;
                    None
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("runtime event channel closed"));
                }
            }
        } else {
            Some(runtime_rx.recv().context("runtime event channel closed")?)
        };

        let Some(event) = received else {
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
            RuntimeEvent::InputStopped => break,
        }
        render(terminal, app)?;
    }
    Ok(())
}

fn render(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &App) -> Result<()> {
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
