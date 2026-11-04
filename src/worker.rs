use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::backend::MailBackend;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage, OutgoingMessage, OutgoingAttachment};
use crate::outgoing::MailSender;
use crate::mail_image::MailImageEvent;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub enum ActionKind {
    Connect,
    ListMailboxes,
    MailboxStatus(String),
    OpenMailbox { name: String, offset: usize, limit: usize },
    FetchBody { mailbox: String, id: MessageId },
    FetchRawForCompose { mailbox: String, id: MessageId },
    FetchAttachments { mailbox: String, id: MessageId },
    ExportRaw { mailbox: String, id: MessageId, path: PathBuf },
    SetSeen { mailbox: String, id: MessageId, seen: bool },
    Delete { mailbox: String, id: MessageId },
    DeleteMany { mailbox: String, ids: Vec<MessageId> },
    Move { source: String, destination: String, id: MessageId },
    MoveMany { source: String, destination: String, ids: Vec<MessageId> },
    ArchiveMany { source: String, destination: String, ids: Vec<MessageId> },
    UnarchiveMany { source: String, destination: String, ids: Vec<MessageId> },
    Send {
        message: OutgoingMessage,
        sent_mailbox: Option<String>,
    },
    SaveDraft { message: OutgoingMessage, mailbox: String },
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
    MailboxStatus {
        name: String,
        total: usize,
        unread: usize,
    },
    MailboxOpened {
        name: String,
        page: MessagePage,
        elapsed_micros: u128,
    },
    BodyLoaded(MessageBody),
    RawLoaded { id: MessageId, raw: Vec<u8> },
    AttachmentsLoaded { id: MessageId, attachments: Vec<OutgoingAttachment> },
    MessageExported { path: PathBuf },
    SeenChanged { id: MessageId, seen: bool },
    MutationComplete,
    DraftSaved { mailbox: String },
    DraftSaveFailed { mailbox: String, error: String },
    MessageSent {
        smtp_status: String,
        sent_mailbox: Option<String>,
        sent_copy_saved: bool,
        sent_copy_warning: Option<String>,
    },
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
    MailImage(MailImageEvent),
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
        mut outgoing: Option<Box<dyn MailSender>>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Action>(64);
        let join = thread::Builder::new()
            .name(format!("mail-worker-{account}"))
            .spawn(move || {
                run_worker(
                    account,
                    &mut *backend,
                    &mut outgoing,
                    receiver,
                    runtime_tx,
                );
            })
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
    outgoing: &mut Option<Box<dyn MailSender>>,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut pending = VecDeque::<Action>::new();

    loop {
        let mut action = if let Some(action) = pending.pop_front() {
            action
        } else {
            match receiver.recv() {
                Ok(action) => action,
                Err(_) => break,
            }
        };

        if matches!(&action.kind, ActionKind::Shutdown) {
            break;
        }

        // Body loads are the expensive operation users most often make stale by
        // moving rapidly through a list. Keep only the newest queued body load.
        // A mailbox/page open has higher priority and invalidates every queued
        // body load: otherwise an old preview can SELECT the mailbox we just left
        // immediately after the new folder opens. The UI requests the current
        // preview again after MailboxOpened, so dropping those stale fetches is
        // both faster and correct.
        if matches!(&action.kind, ActionKind::FetchBody { .. } | ActionKind::OpenMailbox { .. }) {
            let mut opening_mailbox = matches!(&action.kind, ActionKind::OpenMailbox { .. });
            loop {
                match receiver.try_recv() {
                    Ok(next) if matches!(&next.kind, ActionKind::OpenMailbox { .. }) => {
                        action = next;
                        opening_mailbox = true;
                        pending.retain(|queued| !matches!(&queued.kind, ActionKind::FetchBody { .. }));
                    }
                    Ok(next) if matches!(&next.kind, ActionKind::FetchBody { .. }) => {
                        if !opening_mailbox {
                            action = next;
                        }
                    }
                    Ok(next) => pending.push_back(next),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }

        let request_id = action.id;
        let event = match action.kind {
            ActionKind::Connect => backend
                .connect()
                .map(|()| WorkerEventKind::Connected {
                    capabilities: backend.capabilities(),
                }),
            ActionKind::ListMailboxes => backend.list_mailboxes().map(WorkerEventKind::Mailboxes),
            ActionKind::MailboxStatus(name) => backend.mailbox_status(&name).map(|(total, unread)| {
                WorkerEventKind::MailboxStatus {
                    name,
                    total,
                    unread,
                }
            }),
            ActionKind::OpenMailbox { name, offset, limit } => {
                let start = Instant::now();
                backend
                    .open_mailbox(&name, offset, limit)
                    .map(|page| WorkerEventKind::MailboxOpened {
                        name,
                        page,
                        elapsed_micros: start.elapsed().as_micros(),
                    })
            }
            ActionKind::FetchBody { mailbox, id } => backend
                .fetch_body(&mailbox, &id)
                .map(WorkerEventKind::BodyLoaded),
            ActionKind::FetchRawForCompose { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .map(|raw| WorkerEventKind::RawLoaded { id, raw }),
            ActionKind::FetchAttachments { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .and_then(|raw| crate::mail::extract_attachments(&raw))
                .map(|attachments| WorkerEventKind::AttachmentsLoaded { id, attachments }),
            ActionKind::ExportRaw { mailbox, id, path } => backend
                .fetch_raw(&mailbox, &id)
                .and_then(|raw| write_export(&path, &raw).map(|()| path))
                .map(|path| WorkerEventKind::MessageExported { path }),
            ActionKind::SetSeen { mailbox, id, seen } => backend
                .set_seen(&mailbox, &id, seen)
                .map(|()| WorkerEventKind::SeenChanged { id, seen }),
            ActionKind::Delete { mailbox, id } => backend
                .delete(&mailbox, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::DeleteMany { mailbox, ids } => backend
                .delete_many(&mailbox, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Move { source, destination, id } => backend
                .move_to(&source, &destination, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::MoveMany { source, destination, ids } => backend
                .move_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::ArchiveMany { source, destination, ids } => backend
                .archive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::UnarchiveMany { source, destination, ids } => backend
                .unarchive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::SaveDraft { message, mailbox } => {
                // Save failures must not invalidate unrelated mailbox state.
                // A local recovery copy is already on disk at this point.
                Ok(match crate::outgoing::build_draft(&message, &message.from)
                    .and_then(|raw| backend.append_draft(&mailbox, &raw)) {
                    Ok(()) => WorkerEventKind::DraftSaved { mailbox },
                    Err(error) => WorkerEventKind::DraftSaveFailed {
                        mailbox, error: format!("{error:#}"),
                    },
                })
            }
            ActionKind::Send {
                message,
                sent_mailbox,
            } => match outgoing.as_deref_mut() {
                Some(sender) => sender.send(&message).map(|receipt| {
                    let mut sent_copy_saved = false;
                    let mut sent_copy_warning = None;
                    if !receipt.server_files_sent_copy {
                        if let Some(mailbox) = sent_mailbox.as_deref() {
                            match backend.append_message(mailbox, &receipt.raw_message, true) {
                                Ok(()) => sent_copy_saved = true,
                                Err(error) => {
                                    sent_copy_warning = Some(format!(
                                        "SMTP accepted the message, but saving a Sent copy to {mailbox:?} failed: {error:#}"
                                    ));
                                }
                            }
                        } else {
                            sent_copy_warning = Some(
                                "SMTP accepted the message, but the account advertises no Sent mailbox"
                                    .to_owned(),
                            );
                        }
                    }
                    WorkerEventKind::MessageSent {
                        smtp_status: receipt.smtp_status,
                        sent_mailbox,
                        sent_copy_saved,
                        sent_copy_warning,
                    }
                }),
                None => Err(anyhow::anyhow!(
                    "no outgoing transport configured for this account"
                )),
            },
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

fn write_export(path: &std::path::Path, raw: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_export;

    #[test]
    fn raw_export_writes_exact_message_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("message.eml");
        let raw = b"From: test@example.com\r\nSubject: debug\r\n\r\nbody\r\n";
        write_export(&path, raw).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
