//! Tests for the parent module; kept alongside the implementation.
use super::*;
#[test]
fn a_completed_write_must_be_acknowledged_before_a_worker_can_be_retired() {
    let root = tempfile::tempdir().unwrap();
    for name in ["cur", "new", "tmp"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let (tx, rx) = sync_channel(16);
    let mut worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx.clone(),
    );
    let config = crate::config::Config::single_maildir("Local".into(), root.path().into());
    worker.enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    // A delete may have reached the server even if its acknowledgment has
    // not yet been consumed from the UI event queue.
    let request_id = worker
        .post(ActionKind::Delete {
            mailbox: "INBOX".into(),
            id: MessageId("missing".into()),
        })
        .unwrap();
    let RuntimeEvent::ForegroundWorker { event, .. } =
        rx.recv_timeout(Duration::from_secs(1)).unwrap()
    else {
        panic!("expected foreground delete result")
    };
    assert_eq!(event.request_id, request_id);
    assert!(!worker.recover_stalled_foreground().unwrap());
    worker.acknowledge_foreground_event(request_id);
    assert!(worker.recover_stalled_foreground().unwrap());
    worker.shutdown();
}
