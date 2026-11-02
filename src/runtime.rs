use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{self, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, command_for_key};
use crate::backend;
use crate::config::Config;
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
            let backend = backend::from_source(&account.source)?;
            Ok(WorkerHandle::spawn(index, backend, runtime_tx.clone()))
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
        let event = runtime_rx.recv().context("runtime event channel closed")?;
        match event {
            RuntimeEvent::Input(event::Event::Key(key))
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                app.handle_command(command_for_key(key));
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
