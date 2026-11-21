//! Tests for the parent module; kept alongside the implementation.
use super::*;

fn queued() -> DeferredMutation {
    DeferredMutation {
        mailbox: "INBOX".into(),
        validity: 81,
        ids: vec![MessageId("imap-uid:81:77".into())],
        kind: DeferredMutationKind::Archive {
            destination: "Archive".into(),
        },
        state: DeferredMutationState::Queued,
    }
}

#[test]
fn offline_mutations_are_crash_durable_uid_scoped_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let store = MutationStore {
        path: dir.path().join("index.mutations.json"),
        readable: true,
    };
    let pending = queued();
    assert!(pending.has_valid_identity());
    store.save(std::slice::from_ref(&pending)).unwrap();
    assert_eq!(store.read().unwrap(), vec![pending.clone()]);
    let mut uncertain = pending;
    uncertain.state = DeferredMutationState::NeedsReview;
    store.save(std::slice::from_ref(&uncertain)).unwrap();
    let restarted = MutationStore {
        path: store.path.clone(),
        readable: true,
    };
    assert_eq!(restarted.read().unwrap(), vec![uncertain]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn malformed_uid_or_corrupt_journal_is_rejected_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = MutationStore {
        path: dir.path().join("mutations.json"),
        readable: true,
    };
    let mut record = queued();
    record.validity = 82;
    assert!(!record.has_valid_identity());
    assert!(store.save(&[record]).is_err());
    fs::write(&store.path, b"{invalid").unwrap();
    assert!(store.read().is_err());
    let locked = MutationStore {
        path: store.path.clone(),
        readable: false,
    };
    assert!(locked.save(&[queued()]).is_err());
    assert_eq!(fs::read(store.path).unwrap(), b"{invalid");
}
