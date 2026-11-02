use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::backend::MailBackend;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessageSummary};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub enum ActionKind {
    Connect,
    ListMailboxes,
    OpenMailbox(String),
    FetchBody { mailbox: String, id: MessageId },
    SetSeen { mailbox: String, id: MessageId, seen: bool },
    Delete { mailbox: String, id: MessageId },
    Move { source: String, destination: String, id: MessageId },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub id: u64,
    pub kind: ActionKind,
}

impl Action {
    #[must_use]
    pub fn new(kind: ActionKind) -> Self {
        Self {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            kind,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WorkerEventKind {
    Connected { capabilities: Capabilities },
    Mailboxes(Vec<Mailbox>),
    MailboxOpened {
        name: String,
        messages: Vec<MessageSummary>,
        elapsed_micros: u128,
    },
    BodyLoaded(MessageBody),
    MutationComplete,
    Error(String),
    Stopped,
}

#[derive(Debug, Clone)]
pub struct WorkerEvent {
    pub account: usize,
    pub request_id: u64,
    pub kind: WorkerEventKind,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Input(crossterm::event::Event),
    Worker(WorkerEvent),
    InputStopped,
}

pub struct WorkerHandle {
    sender: SyncSender<Action>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn(
        account: usize,
        mut backend: Box<dyn MailBackend>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Action>(64);
        let join = thread::Builder::new()
            .name(format!("mail-worker-{account}"))
            .spawn(move || run_worker(account, &mut *backend, receiver, runtime_tx))
            .expect("failed to spawn mail worker");
        Self {
            sender,
            join: Some(join),
        }
    }

    pub fn post(&self, kind: ActionKind) -> Result<u64, TrySendError<Action>> {
        let action = Action::new(kind);
        let id = action.id;
        self.sender.try_send(action)?;
        Ok(id)
    }

    pub fn shutdown(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // Shutdown is allowed to wait for the bounded queue to drain. Ordinary
        // UI actions use try_send() so backend pressure can never freeze input.
        let _ = self.sender.send(Action::new(ActionKind::Shutdown));
        let _ = join.join();
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    account: usize,
    backend: &mut dyn MailBackend,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    while let Ok(action) = receiver.recv() {
        let request_id = action.id;
        if matches!(&action.kind, ActionKind::Shutdown) {
            break;
        }

        let event = match action.kind {
            ActionKind::Connect => backend
                .connect()
                .map(|()| WorkerEventKind::Connected {
                    capabilities: backend.capabilities(),
                }),
            ActionKind::ListMailboxes => backend.list_mailboxes().map(WorkerEventKind::Mailboxes),
            ActionKind::OpenMailbox(name) => {
                let start = Instant::now();
                backend.open_mailbox(&name).map(|messages| WorkerEventKind::MailboxOpened {
                    name,
                    messages,
                    elapsed_micros: start.elapsed().as_micros(),
                })
            }
            ActionKind::FetchBody { mailbox, id } => {
                backend.fetch_body(&mailbox, &id).map(WorkerEventKind::BodyLoaded)
            }
            ActionKind::SetSeen { mailbox, id, seen } => backend
                .set_seen(&mailbox, &id, seen)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Delete { mailbox, id } => backend
                .delete(&mailbox, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Move { source, destination, id } => backend
                .move_to(&source, &destination, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Shutdown => unreachable!(),
        }
        .unwrap_or_else(|err| WorkerEventKind::Error(format!("{err:#}")));

        if runtime_tx
            .send(RuntimeEvent::Worker(WorkerEvent {
                account,
                request_id,
                kind: event,
            }))
            .is_err()
        {
            break;
        }
    }
}
