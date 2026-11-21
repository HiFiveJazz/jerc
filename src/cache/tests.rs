//! Tests for the parent module; kept alongside the implementation.
use super::*;

#[test]
fn buffered_json_coalesces_small_serde_writes_without_changing_bytes() {
    #[derive(Default)]
    struct CountingSink {
        bytes: Vec<u8>,
        writes: usize,
    }

    impl Write for CountingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A realistic JSON collection with many tiny values; previously each
    // string/colon/comma could cause its own write(2) syscall.
    let headers = vec!["subject-and-sender"; 1_000];
    let expected = serde_json::to_vec(&headers).unwrap();
    assert!(expected.len() < CACHE_JSON_BUFFER_BYTES);
    let mut sink = CountingSink::default();
    write_json_buffered(&mut sink, &headers).unwrap();
    assert_eq!(sink.bytes, expected);
    assert_eq!(sink.writes, 1);
}

#[test]
fn raw_mime_store_is_private_uid_scoped_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore {
        dir: dir.path().join("raw"),
    };
    let id = MessageId("imap-uid:42:7".into());
    let raw = b"From: sender@example.org\r\nSubject: Patch\r\n\r\nHello";
    store
        .write("INBOX", &CacheIdentity::Imap(42), &id, raw)
        .unwrap();
    assert_eq!(
        store.read("INBOX", &CacheIdentity::Imap(42), &id),
        Some(raw.to_vec())
    );
    assert_eq!(store.read("INBOX", &CacheIdentity::Imap(43), &id), None);
    assert_eq!(store.read("Sent", &CacheIdentity::Imap(42), &id), None);
    assert_eq!(
        store.read(
            "INBOX",
            &CacheIdentity::Imap(42),
            &MessageId("imap-uid:42:8".into())
        ),
        None
    );
    assert_eq!(
        store.read(
            "INBOX",
            &CacheIdentity::Imap(42),
            &MessageId("imap-uid:7".into())
        ),
        None
    );
    assert!(
        store
            .write("INBOX", &CacheIdentity::Imap(43), &id, raw)
            .is_err()
    );
    // A later instance must find the same bytes without an IMAP session.
    let reopened = RawStore {
        dir: store.dir.clone(),
    };
    assert_eq!(
        reopened.read("INBOX", &CacheIdentity::Imap(42), &id),
        Some(raw.to_vec())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&store.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(store.raw_path("INBOX", &CacheIdentity::Imap(42), &id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn raw_mime_store_does_not_follow_cache_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore {
        dir: dir.path().join("raw"),
    };
    std::fs::create_dir_all(&store.dir).unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"untouched").unwrap();
    let id = MessageId("imap-uid:12:9".into());
    std::os::unix::fs::symlink(
        &victim,
        store.raw_path("INBOX", &CacheIdentity::Imap(12), &id),
    )
    .unwrap();
    assert!(store.read("INBOX", &CacheIdentity::Imap(12), &id).is_none());
    store
        .write("INBOX", &CacheIdentity::Imap(12), &id, b"safe")
        .unwrap();
    assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
    assert_eq!(
        store.read("INBOX", &CacheIdentity::Imap(12), &id),
        Some(b"safe".to_vec())
    );
}

#[test]
fn identical_index_updates_do_not_rewrite_cache_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut snapshot = Snapshot::default();
    let mut dirty = false;

    let inbox = folder(41, 3);
    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Mailboxes(vec![Mailbox::new(
            "INBOX",
            crate::model::MailboxRole::Inbox,
        )]));
        changes.insert(Change::Folder("INBOX".into(), inbox.clone()));
    }
    assert_eq!(
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
        0
    );
    assert!(path.exists());
    assert!(!dirty);
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&path).unwrap().ino()
    };

    // A fresh IMAP snapshot with equal values (not equal Arc identities)
    // must not replace the existing inode, even after another checkpoint.
    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Mailboxes(vec![Mailbox::new(
            "INBOX",
            crate::model::MailboxRole::Inbox,
        )]));
        changes.insert(Change::Folder("INBOX".into(), folder(41, 3)));
        changes.insert(Change::RemoveFolder("Unknown".into()));
        changes.insert(Change::DeferredSeen(vec![]));
    }
    assert_eq!(
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
        0
    );
    assert!(!dirty);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
    }

    // Flags really are mutable, and must cause a new checkpoint without
    // copying the shared immutable RFC 5322 header allocation.
    let original_headers = Arc::clone(&snapshot.folders["INBOX"].messages[0].headers);
    let mut changed = folder(41, 3);
    changed.messages[0]
        .flags
        .set(crate::model::MessageFlags::SEEN, true);
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("INBOX".into(), changed));
    flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    assert!(!snapshot.folders["INBOX"].messages[0].unread());
    assert_eq!(
        read_snapshot(&path).unwrap().folders["INBOX"].messages[0].flags,
        snapshot.folders["INBOX"].messages[0].flags
    );
    // Original in-memory snapshots remain unaffected by COW/flag updates.
    assert_eq!(original_headers.subject, "Subject 1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(fs::metadata(&path).unwrap().ino(), original_inode);
    }
}

#[test]
fn uidvalidity_and_changed_headers_trigger_checkpoints() {
    let mut snapshot = Snapshot::default();
    assert!(apply(
        &mut snapshot,
        Change::Folder("INBOX".into(), folder(1, 2))
    ));
    assert!(!apply(
        &mut snapshot,
        Change::Folder("INBOX".into(), folder(1, 2))
    ));
    let mut edited = folder(1, 2);
    edited.messages[0].subject = "Updated subject".into();
    assert!(apply(&mut snapshot, Change::Folder("INBOX".into(), edited)));
    assert!(!apply(
        &mut snapshot,
        Change::RemoveFolder("Unknown".into())
    ));
    assert!(apply(
        &mut snapshot,
        Change::Folder("INBOX".into(), folder(2, 2))
    ));
    assert_eq!(
        snapshot.folders["INBOX"].identity,
        Some(CacheIdentity::Imap(2))
    );
    assert!(apply(&mut snapshot, Change::RemoveFolder("INBOX".into())));
    assert!(!apply(&mut snapshot, Change::RemoveFolder("INBOX".into())));
}

#[test]
fn identical_body_is_not_rewritten_but_enriched_body_is() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let id = MessageId("imap-uid:17:42".into());
    let original = body(id.clone(), "Hello");
    assert!(
        store
            .write("INBOX", &CacheIdentity::Imap(17), &original)
            .unwrap()
    );
    let path = store.body_path("INBOX", &CacheIdentity::Imap(17), &id);
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&path).unwrap().ino()
    };
    assert!(
        !store
            .write("INBOX", &CacheIdentity::Imap(17), &original)
            .unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
    }
    let mut enriched = original.clone();
    enriched.attachment_count = 1;
    assert!(
        store
            .write("INBOX", &CacheIdentity::Imap(17), &enriched)
            .unwrap()
    );
    assert_eq!(
        store
            .read("INBOX", &CacheIdentity::Imap(17), &id)
            .unwrap()
            .attachment_count,
        1
    );
    // Corrupted persisted entries are repaired rather than treated as
    // equivalent based on their filename or UID alone.
    fs::write(&path, b"not json").unwrap();
    assert!(
        store
            .write("INBOX", &CacheIdentity::Imap(17), &enriched)
            .unwrap()
    );
    assert_eq!(
        store.read("INBOX", &CacheIdentity::Imap(17), &id).unwrap(),
        enriched
    );
}

#[test]
fn failed_index_checkpoint_is_retried_on_shutdown_without_new_changes() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("not-a-dir");
    fs::write(&parent, b"blocking file").unwrap();
    let path = parent.join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    pending
        .lock()
        .unwrap()
        .insert(Change::Mailboxes(vec![Mailbox::new(
            "INBOX",
            crate::model::MailboxRole::Inbox,
        )]));
    let mut stored = Snapshot::default();
    let mut dirty = false;
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(dirty);
    fs::remove_file(&parent).unwrap();
    // No new event needed: final flush retries the still-dirty snapshot.
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    assert_eq!(read_snapshot(&path).unwrap().mailboxes[0].name, "INBOX");
}

#[test]
fn continuous_cache_events_have_a_maximum_batch_delay() {
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(()).unwrap();
    // A run of updates every ~2 ms must not postpone the checkpoint until
    // the producer stops. Use a generous CI time bound, not a precise timer.
    let sender = thread::spawn(move || {
        for _ in 0..100 {
            let _ = tx.try_send(());
            thread::sleep(Duration::from_millis(2));
        }
    });
    rx.recv().unwrap();
    let started = Instant::now();
    wait_for_cache_batch(&rx, Duration::from_millis(50), Duration::from_millis(60));
    assert!(started.elapsed() < Duration::from_millis(190));
    sender.join().unwrap();
}

#[test]
fn buffered_json_stays_bounded_across_multiple_64k_blocks() {
    #[derive(Default)]
    struct CountingSink {
        bytes: Vec<u8>,
        write_calls: usize,
    }

    impl Write for CountingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.write_calls += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Several hundred KiB of many small JSON fields used to issue tens
    // of thousands of write(2) calls before we buffered serialization.
    let values = (0..20_000)
        .map(|index| format!("Subject {index:05}: sender@example.test"))
        .collect::<Vec<_>>();
    let expected = serde_json::to_vec(&values).unwrap();
    assert!(expected.len() > CACHE_JSON_BUFFER_BYTES * 4);
    let mut sink = CountingSink::default();
    write_json_buffered(&mut sink, &values).unwrap();
    assert_eq!(sink.bytes, expected);
    assert!(sink.write_calls > 1);
    assert!(
        sink.write_calls <= expected.len() / CACHE_JSON_BUFFER_BYTES + 2,
        "buffered writer made {} calls for {} bytes",
        sink.write_calls,
        expected.len()
    );
}

#[test]
fn unchanged_multi_folder_refreshes_never_replace_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut snapshot = Snapshot::default();
    let mut dirty = false;
    let mailboxes = vec![
        Mailbox::new("INBOX", crate::model::MailboxRole::Inbox),
        Mailbox::new("Sent", crate::model::MailboxRole::Sent),
    ];

    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Mailboxes(mailboxes.clone()));
        changes.insert(Change::Folder("INBOX".into(), folder(81, 120)));
        changes.insert(Change::Folder("Sent".into(), folder(82, 40)));
    }
    flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    let original_bytes = fs::read(&path).unwrap();
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&path).unwrap().ino()
    };

    // Repeated independent IMAP STATUS results arrive in different orders
    // and reconstruct equal headers with fresh allocations.
    for _ in 0..5 {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Folder("Sent".into(), folder(82, 40)));
        changes.insert(Change::Mailboxes(mailboxes.clone()));
        changes.insert(Change::Folder("INBOX".into(), folder(81, 120)));
        drop(changes);
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            0
        );
        assert!(!dirty);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
        }
    }

    // A genuine flag change must still persist, without losing the other
    // previously warmed folder.
    let mut sent = folder(82, 40);
    sent.messages[0]
        .flags
        .set(crate::model::MessageFlags::SEEN, true);
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("Sent".into(), sent));
    flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
    let reloaded = read_snapshot(&path).unwrap();
    assert!(!reloaded.folders["Sent"].messages[0].unread());
    assert_eq!(reloaded.folders["INBOX"].messages.len(), 120);
    assert!(!dirty);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(fs::metadata(&path).unwrap().ino(), original_inode);
    }
}

#[test]
fn changing_only_terminal_html_layout_does_not_rewrite_disk_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut stored = Snapshot::default();
    let mut dirty = false;
    let mut initial = folder(41, 1);
    let id = initial.messages[0].id.clone();
    let mut rendered = body(id.clone(), "Visible text");
    rendered.html_source = Some("<p>Visible text</p>".into());
    rendered.html_text = Some("Visible text".into());
    rendered.html_lines = Some(Arc::new(vec![crate::model::HtmlLine::default()]));
    rendered.html_render_width = 80;
    initial.bodies.insert(id.clone(), rendered.clone());
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("INBOX".into(), initial));
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&path).unwrap().ino()
    };
    let old_bytes = fs::read(&path).unwrap();

    let mut reflowed = folder(41, 1);
    rendered.html_lines = Some(Arc::new(vec![
        crate::model::HtmlLine::default(),
        crate::model::HtmlLine::default(),
    ]));
    rendered.html_render_width = 120;
    reflowed.bodies.insert(id, rendered);
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("INBOX".into(), reflowed));
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    assert_eq!(fs::read(&path).unwrap(), old_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    }
}

#[test]
fn mailbox_burst_keeps_only_the_final_folder_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut snapshot = Snapshot::default();
    let mut dirty = false;
    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Folder("INBOX".into(), folder(14, 1)));
        changes.insert(Change::Folder("Sent".into(), folder(15, 2)));
    }
    flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Folder("INBOX".into(), folder(14, 2)));
        changes.insert(Change::RemoveFolder("INBOX".into()));
        changes.insert(Change::Folder("INBOX".into(), folder(14, 5)));
        changes.insert(Change::Folder("Sent".into(), folder(15, 4)));
        changes.insert(Change::RemoveFolder("Sent".into()));
        changes.insert(Change::RemoveFolder("NeverLoaded".into()));
        assert_eq!(changes.folders.len(), 3);
    }
    flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    let persisted = read_snapshot(&path).unwrap();
    assert_eq!(persisted.folders.len(), 1);
    assert_eq!(persisted.folders["INBOX"].messages.len(), 5);
    assert_eq!(
        persisted.folders["INBOX"].identity,
        Some(CacheIdentity::Imap(14))
    );
    assert!(pending.lock().unwrap().folders.is_empty());
}

#[test]
fn queued_body_updates_write_only_the_latest_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut snapshot = Snapshot::default();
    let mut dirty = false;
    let id = MessageId("imap-uid:19:4".into());
    let latest = body(id.clone(), "Latest fetched text");
    {
        let mut changes = pending.lock().unwrap();
        changes.insert(Change::Body(
            "INBOX".into(),
            CacheIdentity::Imap(19),
            body(id.clone(), "Old"),
        ));
        changes.insert(Change::Body(
            "INBOX".into(),
            CacheIdentity::Imap(19),
            latest.clone(),
        ));
    }
    assert_eq!(
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
        1
    );
    assert!(
        !path.exists(),
        "body persistence must not rewrite the mailbox index"
    );
    assert_eq!(
        body_store.read("INBOX", &CacheIdentity::Imap(19), &id),
        Some(latest.clone())
    );
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(body_store.body_path("INBOX", &CacheIdentity::Imap(19), &id))
            .unwrap()
            .ino()
    };
    pending.lock().unwrap().insert(Change::Body(
        "INBOX".into(),
        CacheIdentity::Imap(19),
        latest.clone(),
    ));
    assert_eq!(
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
        0
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(body_store.body_path("INBOX", &CacheIdentity::Imap(19), &id))
                .unwrap()
                .ino(),
            original_inode
        );
    }
    let mut enriched = latest;
    enriched.attachment_count = 2;
    pending.lock().unwrap().insert(Change::Body(
        "INBOX".into(),
        CacheIdentity::Imap(19),
        enriched.clone(),
    ));
    assert_eq!(
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
        1
    );
    assert_eq!(
        body_store.read("INBOX", &CacheIdentity::Imap(19), &id),
        Some(enriched)
    );
}

#[test]
fn failed_checkpoint_retries_the_latest_coalesced_data() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocked-parent");
    fs::write(&blocker, b"not a directory").unwrap();
    let path = blocker.join("index.json");
    let body_store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let mut stored = Snapshot::default();
    let mut dirty = false;
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("INBOX".into(), folder(31, 1)));
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(dirty);

    // More updates can arrive while the filesystem is unavailable; when
    // persistence recovers it must save the newest state, not the first.
    pending
        .lock()
        .unwrap()
        .insert(Change::Folder("INBOX".into(), folder(31, 3)));
    fs::remove_file(&blocker).unwrap();
    flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
    assert!(!dirty);
    assert_eq!(
        read_snapshot(&path).unwrap().folders["INBOX"]
            .messages
            .len(),
        3
    );
}

fn test_cache_writer(path: PathBuf) -> Writer {
    let (wake, rx) = mpsc::sync_channel::<()>(1);
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let body_store = BodyStore {
        dir: path.with_extension("bodies"),
    };
    let worker_pending = Arc::clone(&pending);
    let worker_store = body_store.clone();
    let worker_path = path.clone();
    let join = thread::spawn(move || {
        run_cache_writer(
            rx,
            worker_path,
            worker_pending,
            worker_store,
            Snapshot::default(),
        );
    });
    Writer {
        wake: Some(wake),
        pending,
        join: Some(join),
        body_store,
        mutation_store: MutationStore {
            path: path.with_extension("mutations.json"),
            readable: true,
        },
    }
}

#[test]
fn remote_mailbox_counts_survive_shutdown_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::Config::single_maildir("Test".into(), dir.path().into());
    for source in ["imaps://user@example.test", "jmaps://user@example.test"] {
        let mut account = config.accounts[0].clone();
        account.source = source.into();
        assert!(supports_mailbox_cache(&account.source));
        let (snapshot, writer) = open_in(&account, dir.path());
        assert!(snapshot.mailboxes.is_empty());
        let mailboxes = vec![
            Mailbox {
                name: "INBOX".into(),
                role: crate::model::MailboxRole::Inbox,
                total: 100,
                unread: 42,
                counts_known: true,
            },
            Mailbox {
                name: "Sent".into(),
                role: crate::model::MailboxRole::Sent,
                total: 5,
                unread: 0,
                counts_known: true,
            },
            Mailbox::new("New", crate::model::MailboxRole::Other),
        ];
        let writer = writer.unwrap();
        writer.send(Change::Mailboxes(mailboxes.clone()));
        drop(writer);
        let (reopened, writer) = open_in(&account, dir.path());
        assert_eq!(reopened.mailboxes, mailboxes);
        assert!(reopened.folders.is_empty());
        drop(writer.unwrap());
    }
}

#[test]
fn jmap_mailbox_cache_keys_isolate_endpoints_and_ignore_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::Config::single_maildir("Test".into(), dir.path().into());
    let mut account = config.accounts[0].clone();
    account.source = "jmaps://user:old@example.test".into();
    let first = account_cache_stem(&account);
    account.source = "jmaps://user:new@example.test/.well-known/jmap".into();
    assert_eq!(account_cache_stem(&account), first);
    account.source = "jmaps://user:new@example.test/other/jmap".into();
    assert_ne!(account_cache_stem(&account), first);
    account.source = "jmaps://other:new@example.test/.well-known/jmap".into();
    assert_ne!(account_cache_stem(&account), first);
}

#[test]
fn dropping_writer_flushes_batched_index_and_body_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let writer = test_cache_writer(path.clone());
    let id = MessageId("imap-uid:31:1".into());
    writer.send(Change::Folder("INBOX".into(), folder(31, 1)));
    writer.send(Change::Body(
        "INBOX".into(),
        CacheIdentity::Imap(31),
        body(id.clone(), "Saved body"),
    ));
    writer.send(Change::Mailboxes(vec![Mailbox::new(
        "INBOX",
        crate::model::MailboxRole::Inbox,
    )]));
    // No sleeps: an immediate quit must flush every pending cache change.
    drop(writer);
    let loaded = read_snapshot(&path).unwrap();
    assert_eq!(loaded.mailboxes[0].name, "INBOX");
    assert_eq!(loaded.folders["INBOX"].messages.len(), 1);
    assert_eq!(
        BodyStore {
            dir: path.with_extension("bodies"),
        }
        .read("INBOX", &CacheIdentity::Imap(31), &id),
        Some(body(id, "Saved body"))
    );
}

#[test]
fn destructive_mutation_journal_is_saved_before_cache_batch_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let writer = test_cache_writer(path.clone());
    writer.send(Change::Folder("INBOX".into(), folder(52, 1)));
    let operation = DeferredMutation {
        mailbox: "INBOX".into(),
        validity: 52,
        ids: vec![MessageId("imap-uid:52:1".into())],
        kind: DeferredMutationKind::Delete,
        state: DeferredMutationState::Queued,
    };
    writer
        .save_mutations(std::slice::from_ref(&operation))
        .unwrap();
    // Crash-durable operations do not wait for the 500 ms display-cache
    // debounce or for Writer::drop to join the background worker.
    let journal = MutationStore {
        path: path.with_extension("mutations.json"),
        readable: true,
    };
    assert_eq!(journal.read().unwrap(), vec![operation.clone()]);
    drop(writer);
    assert_eq!(
        read_snapshot(&path).unwrap().folders["INBOX"]
            .messages
            .len(),
        1
    );
    assert_eq!(journal.read().unwrap(), vec![operation]);
}

#[test]
fn pending_writer_coalesces_latest_folder_and_removal() {
    let mut pending = super::PendingChanges::default();
    pending.insert(super::Change::Mailboxes(Vec::new()));
    pending.insert(super::Change::RemoveFolder("INBOX".into()));
    pending.insert(super::Change::RemoveFolder("INBOX".into()));
    assert_eq!(pending.folders.len(), 1);
    let mut stored = super::Snapshot::default();
    pending.drain_into(&mut stored);
    assert!(pending.folders.is_empty());
}

#[test]
fn deferred_seen_journal_roundtrip_and_coalescing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let id = MessageId("imap-uid:17:4".into());
    let entry = DeferredSeen {
        mailbox: "INBOX".into(),
        validity: 17,
        id: id.clone(),
        seen: true,
    };
    let mut pending = PendingChanges::default();
    pending.insert(Change::DeferredSeen(vec![entry.clone()]));
    pending.insert(Change::DeferredSeen(vec![DeferredSeen {
        seen: false,
        ..entry
    }]));
    let mut snapshot = Snapshot::default();
    assert!(pending.drain_into(&mut snapshot));
    assert_eq!(snapshot.deferred_seen.len(), 1);
    assert!(!snapshot.deferred_seen[0].seen);
    write_snapshot(&path, &snapshot).unwrap();
    assert_eq!(
        read_snapshot(&path).unwrap().deferred_seen,
        snapshot.deferred_seen
    );
    pending.insert(Change::DeferredSeen(vec![]));
    assert!(pending.drain_into(&mut snapshot));
    assert!(snapshot.deferred_seen.is_empty());
}

#[test]
fn invalid_deferred_uid_is_rejected_from_the_index() {
    let mut snapshot = Snapshot::default();
    snapshot.deferred_seen.push(DeferredSeen {
        mailbox: "INBOX".into(),
        validity: 10,
        id: MessageId("imap-uid:11:3".into()),
        seen: true,
    });
    let invalid = snapshot.deferred_seen.clone();
    apply(&mut snapshot, Change::DeferredSeen(invalid));
    assert!(snapshot.deferred_seen.is_empty());
}

#[test]
fn body_only_changes_do_not_rewrite_mailbox_index() {
    let mut pending = PendingChanges::default();
    let id = MessageId("imap-uid:10:4".into());
    pending.insert(Change::Body(
        "INBOX".into(),
        CacheIdentity::Imap(10),
        body(id.clone(), "hello"),
    ));
    let mut snapshot = Snapshot::default();
    assert!(!pending.drain_into(&mut snapshot));
    assert!(snapshot.folders.is_empty());
    let bodies = pending.drain_bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].2.id, id);
}

#[test]
fn coalesced_writer_queue_never_exceeds_folder_limit() {
    let mut pending = PendingChanges::default();
    for index in 0..1_000 {
        pending.insert(Change::RemoveFolder(format!("F{index}")));
    }
    assert!(pending.folders.len() <= MAX_FOLDERS);
    pending.insert(Change::RemoveFolder("latest".into()));
    assert!(pending.folders["latest"].is_none());
}

#[test]
fn snapshot_roundtrip_and_private_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let data = Snapshot {
        mailboxes: vec![Mailbox::new("INBOX", crate::model::MailboxRole::Inbox)],
        folders: HashMap::new(),
        deferred_seen: Vec::new(),
        deferred_mutations: Vec::new(),
    };
    write_snapshot(&path, &data).unwrap();
    let loaded = read_snapshot(&path).unwrap();
    assert_eq!(loaded.mailboxes[0].name, "INBOX");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn cloned_cache_indexes_share_headers_but_not_flags() {
    let mut original = Snapshot::default();
    original.folders.insert("INBOX".into(), folder(23, 2));
    let mut writer_index = original.clone();
    let original_row = &original.folders["INBOX"].messages[0];
    let writer_row = &writer_index.folders["INBOX"].messages[0];
    assert!(std::sync::Arc::ptr_eq(
        &original_row.headers,
        &writer_row.headers
    ));

    writer_index.folders.get_mut("INBOX").unwrap().messages[0]
        .flags
        .set(crate::model::MessageFlags::SEEN, true);
    assert!(std::sync::Arc::ptr_eq(
        &original.folders["INBOX"].messages[0].headers,
        &writer_index.folders["INBOX"].messages[0].headers,
    ));
    assert!(original.folders["INBOX"].messages[0].unread());
    assert!(!writer_index.folders["INBOX"].messages[0].unread());
}

#[test]
fn older_cache_without_uidnext_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    snapshot.folders.insert("INBOX".into(), folder(55, 2));
    write_snapshot(&path, &snapshot).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut value = value;
    value["snapshot"]["folders"]["INBOX"]
        .as_object_mut()
        .unwrap()
        .remove("uid_next");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let loaded = read_snapshot(&path).unwrap();
    assert_eq!(
        loaded.folders["INBOX"].identity,
        Some(CacheIdentity::Imap(55))
    );
    assert_eq!(loaded.folders["INBOX"].uid_next, None);
}

#[test]
fn corrupted_or_old_cache_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    std::fs::write(&path, b"{not json").unwrap();
    assert!(read_snapshot(&path).is_none());
    std::fs::write(&path, br#"{"version":0,"snapshot":{}}"#).unwrap();
    assert!(read_snapshot(&path).is_none());
}

fn jmap_identity(account: &str, mailbox: &str, state: &str) -> CacheIdentity {
    CacheIdentity::Jmap {
        account_id: account.into(),
        mailbox_id: mailbox.into(),
        email_state: Some(state.into()),
        query_state: None,
    }
}

#[test]
fn jmap_summaries_and_state_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    let mut cached = folder(42, 1);
    cached.messages[0].id = MessageId("opaque-id".into());
    cached.identity = Some(jmap_identity("a", "inbox", "s1"));
    assert!(apply(
        &mut snapshot,
        Change::Folder("INBOX".into(), cached.clone())
    ));
    write_snapshot(&path, &snapshot).unwrap();
    assert_eq!(read_snapshot(&path).unwrap().folders["INBOX"], cached);
    assert_eq!(cached.identity.unwrap().uid_validity(), None);
}

#[test]
fn jmap_body_and_raw_mime_keys_share_account_but_isolate_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let parsed = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let raw = RawStore {
        dir: dir.path().join("raw"),
    };
    let id = MessageId("opaque-id".into());
    let first = jmap_identity("a", "inbox", "s1");
    let content = body(id.clone(), "JMAP cached body");
    parsed.write("INBOX", &first, &content).unwrap();
    raw.write("INBOX", &first, &id, b"hello").unwrap();

    // The same account Email ID has one immutable body, regardless of
    // synchronization checkpoints, mailbox membership, or mailbox recreation.
    for (mailbox, identity) in [
        ("INBOX", jmap_identity("a", "inbox", "s2")),
        ("Sent", jmap_identity("a", "sent", "s3")),
        ("INBOX", jmap_identity("a", "recreated", "s4")),
    ] {
        assert_eq!(parsed.read(mailbox, &identity, &id), Some(content.clone()));
        assert_eq!(raw.read(mailbox, &identity, &id), Some(b"hello".to_vec()));
    }

    // Other accounts and IMAP must never reuse this account's cached data.
    for wrong in [jmap_identity("b", "inbox", "s2"), CacheIdentity::Imap(42)] {
        assert!(parsed.read("INBOX", &wrong, &id).is_none());
        assert!(raw.read("INBOX", &wrong, &id).is_none());
    }
    let other_id = MessageId("another-email".into());
    assert!(parsed.read("INBOX", &first, &other_id).is_none());
    assert!(raw.read("INBOX", &first, &other_id).is_none());
}

#[test]
fn legacy_numeric_cache_keys_and_snapshot_fields_remain_readable() {
    let cached = folder(42, 1);
    let value = serde_json::to_value(&cached).unwrap();
    assert_eq!(value["uid_validity"], 42);
    let restored: CachedMailbox = serde_json::from_value(value).unwrap();
    assert_eq!(restored, cached);
    let id = &cached.messages[0].id;
    use ring::digest::{SHA256, digest};
    let expected = digest(&SHA256, format!("INBOX\0{}\0{}", 42, id.0).as_bytes());
    let expected: String = expected.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        cache_message_key("INBOX", &CacheIdentity::Imap(42), id),
        expected
    );
}

#[test]
fn cache_requires_uidvalidity() {
    let mut snapshot = Snapshot::default();
    let folder = CachedMailbox {
        thread_messages: Vec::new(),
        messages: vec![],
        bodies: HashMap::new(),
        message_index: 0,
        message_view_offset: 0,
        next_page_offset: 0,
        has_more: false,
        identity: None,
        uid_next: None,
    };
    apply(
        &mut snapshot,
        Change::Folder("INBOX".into(), folder.clone()),
    );
    assert!(!snapshot.folders.contains_key("INBOX"));
    apply(
        &mut snapshot,
        Change::Folder(
            "INBOX".into(),
            CachedMailbox {
                identity: Some(CacheIdentity::Imap(42)),
                ..folder
            },
        ),
    );
    assert!(snapshot.folders.contains_key("INBOX"));
}

fn summary(uid_validity: u32, uid: u32) -> MessageSummary {
    MessageSummary {
        headers: std::sync::Arc::new(crate::model::MessageHeaders {
            id: MessageId(format!("imap-uid:{uid_validity}:{uid}")),
            thread_id: None,
            message_id: None,
            in_reply_to: None,
            references: Vec::new().into(),
            from_name: "Sender".into(),
            from_address: "sender@example.test".into(),
            to: "recipient@example.test".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: format!("Subject {uid}"),
            timestamp: i64::from(uid),
            date_label: "Today".into(),
            size: 25,
            has_attachment_hint: false,
            attachment_names_hint: vec![].into(),
        }),
        flags: crate::model::MessageFlags::empty(),
    }
}

fn body(id: MessageId, text: &str) -> MessageBody {
    MessageBody {
        id,
        plain_text: Some(text.into()),
        plain_fallback: None,
        html_source: None,
        html_text: None,
        html_lines: None,
        html_renderer: None,
        html_render_width: 0,
        attachment_count: 0,
        attachment_names: vec![],
    }
}

fn folder(validity: u32, count: u32) -> CachedMailbox {
    CachedMailbox {
        thread_messages: Vec::new(),
        messages: (1..=count).map(|uid| summary(validity, uid)).collect(),
        bodies: HashMap::new(),
        message_index: 0,
        message_view_offset: 0,
        next_page_offset: count as usize,
        has_more: false,
        identity: Some(CacheIdentity::Imap(validity)),
        uid_next: None,
    }
}

#[test]
fn supplemental_rows_roundtrip_without_changing_page_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.json");
    let mut snapshot = Snapshot::default();
    let mut cached = folder(42, 2);
    cached.has_more = true;
    cached.thread_messages = vec![summary(42, 1), summary(42, 7), summary(42, 7)];
    assert!(apply(&mut snapshot, Change::Folder("INBOX".into(), cached)));
    write_snapshot(&path, &snapshot).unwrap();
    let restored = read_snapshot(&path).unwrap();
    let cached = &restored.folders["INBOX"];
    assert_eq!(cached.next_page_offset, 2);
    assert_eq!(cached.messages.len(), 2);
    assert_eq!(cached.thread_messages, vec![summary(42, 7)]);
    let mut stale = cached.clone();
    stale.thread_messages.push(summary(43, 7));
    assert!(!apply(&mut snapshot, Change::Folder("INBOX".into(), stale)));
}

#[test]
fn cached_body_clones_share_large_text_and_rendered_html_allocations() {
    use std::sync::Arc;

    let mut folder = folder(42, 1);
    let id = folder.messages[0].id.clone();
    let mut rich = body(id.clone(), &"plain".repeat(4096));
    rich.plain_fallback = Some("fallback".repeat(4096).into());
    rich.html_source = Some(format!("<p>{}</p>", "markup".repeat(4096)).into());
    rich.html_text = Some("rendered".repeat(4096).into());
    rich.html_lines = Some(Arc::new(vec![crate::model::HtmlLine {
        spans: vec![crate::model::HtmlSpan {
            text: "styled span".repeat(4096),
            ..Default::default()
        }],
    }]));
    folder.bodies.insert(id.clone(), rich);

    let live = folder.clone();
    let source = &folder.bodies[&id];
    let copy = &live.bodies[&id];
    for (old, new) in [
        (&source.plain_text, &copy.plain_text),
        (&source.plain_fallback, &copy.plain_fallback),
        (&source.html_source, &copy.html_source),
        (&source.html_text, &copy.html_text),
    ] {
        assert!(Arc::ptr_eq(old.as_ref().unwrap(), new.as_ref().unwrap()));
    }
    assert!(Arc::ptr_eq(
        source.html_lines.as_ref().unwrap(),
        copy.html_lines.as_ref().unwrap()
    ));

    // Writer snapshots omit derived rows while borrowing the same source
    // allocations. Cloning for persistence must not consume the UI cache.
    let disk = folder.for_persistence();
    let saved = &disk.bodies[&id];
    assert!(saved.html_lines.is_none());
    assert!(Arc::ptr_eq(
        source.html_source.as_ref().unwrap(),
        saved.html_source.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(
        source.plain_text.as_ref().unwrap(),
        saved.plain_text.as_ref().unwrap()
    ));
    assert!(source.html_lines.is_some());

    // Shared allocations serialize as ordinary JSON strings, compatible
    // with existing on-disk cache files and older jerc versions.
    let bytes = serde_json::to_vec(&disk).unwrap();
    let restored: CachedMailbox = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.bodies[&id].html_source, source.html_source);
    assert_eq!(restored.bodies[&id].plain_text, source.plain_text);
    assert!(restored.bodies[&id].html_lines.is_none());
}

#[test]
fn html_reflow_replaces_only_the_current_body_layout() {
    use std::sync::Arc;

    let id = MessageId("imap-uid:42:1".into());
    let mut before = body(id, "base text");
    before.html_source = Some("<p>source</p>".into());
    before.html_text = Some("first render".into());
    before.html_lines = Some(Arc::new(vec![crate::model::HtmlLine::default()]));

    let mut after = before.clone();
    after.html_text = Some("reflowed".into());
    after.html_lines = Some(Arc::new(vec![
        crate::model::HtmlLine::default(),
        crate::model::HtmlLine::default(),
    ]));

    assert_eq!(before.html_text.as_deref(), Some("first render"));
    assert_eq!(after.html_text.as_deref(), Some("reflowed"));
    assert_eq!(before.html_lines.as_ref().unwrap().len(), 1);
    assert_eq!(after.html_lines.as_ref().unwrap().len(), 2);
    assert!(Arc::ptr_eq(
        before.html_source.as_ref().unwrap(),
        after.html_source.as_ref().unwrap()
    ));
}

#[test]
fn persistent_folder_snapshot_drops_derived_html_without_touching_live_body() {
    let mut folder = folder(42, 1);
    folder.message_index = 0;
    folder.message_view_offset = 3;
    folder.uid_next = Some(17);
    let id = folder.messages[0].id.clone();
    let mut rich = body(id.clone(), "Cached text");
    rich.html_source = Some("<p>Cached text</p>".into());
    rich.html_text = Some("Cached text".into());
    rich.html_lines = Some(std::sync::Arc::new(vec![crate::model::HtmlLine {
        spans: vec![crate::model::HtmlSpan {
            text: "Rendered layout".repeat(4096),
            ..Default::default()
        }],
    }]));
    rich.html_renderer = Some("rich".into());
    rich.html_render_width = 72;
    folder.bodies.insert(id.clone(), rich);

    let persistent = folder.for_persistence();
    assert_eq!(persistent.messages[0].id, id);
    assert_eq!(persistent.message_view_offset, 3);
    assert_eq!(persistent.identity, Some(CacheIdentity::Imap(42)));
    assert_eq!(persistent.uid_next, Some(17));
    let body = &persistent.bodies[&id];
    assert_eq!(body.html_source.as_deref(), Some("<p>Cached text</p>"));
    assert_eq!(body.html_text.as_deref(), Some("Cached text"));
    assert!(body.html_lines.is_none());
    assert!(body.html_renderer.is_none());
    assert_eq!(body.html_render_width, 0);
    // Live folder switching keeps the expensive layout, by design.
    assert!(folder.bodies[&id].html_lines.is_some());
    assert_eq!(folder.bodies[&id].html_render_width, 72);

    // Even a direct writer change from an older caller must not retain
    // derived rich spans inside the persistent snapshot.
    let mut snapshot = Snapshot::default();
    apply(&mut snapshot, Change::Folder("INBOX".into(), folder));
    assert!(snapshot.folders["INBOX"].bodies[&id].html_lines.is_none());
}

#[test]
fn cached_flags_subject_and_body_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    let mut inbox = folder(12, 2);
    inbox.messages[0].subject = "Unicode: café ☃".into();
    inbox.messages[0]
        .flags
        .set(crate::model::MessageFlags::SEEN, true);
    let first_id = inbox.messages[0].id.clone();
    inbox
        .bodies
        .insert(first_id.clone(), body(first_id, "cached offline text"));
    apply(&mut snapshot, Change::Folder("INBOX".into(), inbox));
    write_snapshot(&path, &snapshot).unwrap();

    let loaded = read_snapshot(&path).unwrap();
    let inbox = loaded.folders.get("INBOX").unwrap();
    assert_eq!(inbox.messages.len(), 2);
    assert_eq!(inbox.messages[0].subject, "Unicode: café ☃");
    assert!(!inbox.messages[0].unread());
    let text = inbox.bodies.get(&inbox.messages[0].id).unwrap();
    assert_eq!(text.plain_text.as_deref(), Some("cached offline text"));
}

#[test]
fn mismatched_uidvalidity_is_rejected_on_disk_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    let mut inbox = folder(100, 1);
    inbox.messages[0].id = MessageId("imap-uid:99:1".into());
    snapshot.folders.insert("INBOX".into(), inbox);
    snapshot.folders.insert("Archive".into(), folder(100, 1));
    write_snapshot(&path, &snapshot).unwrap();

    let loaded = read_snapshot(&path).unwrap();
    assert!(!loaded.folders.contains_key("INBOX"));
    assert!(loaded.folders.contains_key("Archive"));
}

#[test]
fn mismatched_uidvalidity_in_cached_body_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    let mut inbox = folder(100, 1);
    let bad_id = MessageId("imap-uid:99:1".into());
    inbox.bodies.insert(bad_id.clone(), body(bad_id, "stale"));
    snapshot.folders.insert("INBOX".into(), inbox);
    write_snapshot(&path, &snapshot).unwrap();

    assert!(!read_snapshot(&path).unwrap().folders.contains_key("INBOX"));
}

#[test]
fn folder_updates_limit_messages_bodies_and_size() {
    let mut snapshot = Snapshot::default();
    let mut inbox = folder(7, MAX_MESSAGES_PER_FOLDER as u32 + 2);
    let first_id = inbox.messages[0].id.clone();
    let removed_id = inbox.messages[MAX_MESSAGES_PER_FOLDER].id.clone();
    inbox
        .bodies
        .insert(first_id.clone(), body(first_id.clone(), "keep"));
    inbox
        .bodies
        .insert(removed_id.clone(), body(removed_id, "drop"));
    let too_large_id = inbox.messages[1].id.clone();
    inbox.bodies.insert(
        too_large_id.clone(),
        body(too_large_id, &"x".repeat(MAX_BODY_BYTES + 1)),
    );
    apply(&mut snapshot, Change::Folder("INBOX".into(), inbox));

    let cached = snapshot.folders.get("INBOX").unwrap();
    assert_eq!(cached.messages.len(), MAX_MESSAGES_PER_FOLDER);
    assert_eq!(cached.next_page_offset, MAX_MESSAGES_PER_FOLDER);
    assert!(cached.has_more);
    assert_eq!(cached.bodies.len(), 1);
    assert_eq!(cached.bodies[&first_id].plain_text.as_deref(), Some("keep"));
}

#[test]
fn persistence_clones_only_bodies_that_survive_disk_limits() {
    let messages = (1..=MAX_MESSAGES_PER_FOLDER as u32 + 2)
        .map(|uid| summary(7, uid))
        .collect::<Vec<_>>();
    let mut live_bodies = HashMap::new();
    for message in &messages {
        let id = message.id.clone();
        live_bodies.insert(id.clone(), body(id, "cached"));
    }
    // This oversized body is skipped, allowing the next eligible body in.
    let large_id = messages[0].id.clone();
    live_bodies.insert(
        large_id.clone(),
        body(large_id.clone(), &"x".repeat(MAX_BODY_BYTES + 1)),
    );
    let disk_bodies = persistable_bodies(&messages, &live_bodies);
    assert_eq!(disk_bodies.len(), MAX_BODIES_PER_FOLDER);
    assert!(!disk_bodies.contains_key(&large_id));
    assert!(disk_bodies.contains_key(&messages[1].id));
    assert!(disk_bodies.contains_key(&messages[MAX_BODIES_PER_FOLDER].id));
    assert!(!disk_bodies.contains_key(&messages[MAX_MESSAGES_PER_FOLDER].id));
    // Producing the snapshot must never consume or trim the warm reader cache.
    assert_eq!(live_bodies.len(), messages.len());

    // The existing writer's validation should produce exactly the same
    // persisted set as cloning the entire working set before filtering.
    let mut expected = Snapshot::default();
    apply(
        &mut expected,
        Change::Folder(
            "INBOX".into(),
            CachedMailbox {
                thread_messages: Vec::new(),
                messages: messages.clone(),
                bodies: live_bodies,
                message_index: 0,
                message_view_offset: 0,
                next_page_offset: messages.len(),
                has_more: false,
                identity: Some(CacheIdentity::Imap(7)),
                uid_next: None,
            },
        ),
    );
    let mut reduced = Snapshot::default();
    apply(
        &mut reduced,
        Change::Folder(
            "INBOX".into(),
            CachedMailbox {
                thread_messages: Vec::new(),
                messages: messages
                    .iter()
                    .take(MAX_MESSAGES_PER_FOLDER)
                    .cloned()
                    .collect(),
                bodies: disk_bodies,
                message_index: 0,
                message_view_offset: 0,
                next_page_offset: MAX_MESSAGES_PER_FOLDER,
                has_more: true,
                identity: Some(CacheIdentity::Imap(7)),
                uid_next: None,
            },
        ),
    );
    assert_eq!(
        serde_json::to_value(&expected).unwrap(),
        serde_json::to_value(&reduced).unwrap()
    );
}

#[test]
fn folder_removal_survives_cache_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let mut snapshot = Snapshot::default();
    apply(&mut snapshot, Change::Folder("INBOX".into(), folder(8, 1)));
    apply(
        &mut snapshot,
        Change::Folder("Archive".into(), folder(8, 1)),
    );
    apply(&mut snapshot, Change::RemoveFolder("Archive".into()));
    write_snapshot(&path, &snapshot).unwrap();

    let loaded = read_snapshot(&path).unwrap();
    assert!(loaded.folders.contains_key("INBOX"));
    assert!(!loaded.folders.contains_key("Archive"));
}

#[test]
fn cache_over_size_limit_is_ignored_without_loading_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized.json");
    let file = File::create(&path).unwrap();
    file.set_len(MAX_CACHE_BYTES + 1).unwrap();
    assert!(read_snapshot(&path).is_none());
}

#[test]
fn body_store_roundtrip_is_lazy_and_scoped_to_uid_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let id = MessageId("imap-uid:17:42".into());
    let mut message = body(id.clone(), "Offline message");
    message.html_lines = Some(std::sync::Arc::new(vec![crate::model::HtmlLine::default()]));
    let compact = compact_body(&message).unwrap();
    assert!(compact.html_lines.is_none());
    store
        .write("INBOX", &CacheIdentity::Imap(17), &compact)
        .unwrap();
    assert!(store.has_candidate("INBOX", &CacheIdentity::Imap(17), &id));
    assert!(!store.has_candidate("INBOX", &CacheIdentity::Imap(18), &id));
    assert_eq!(
        store
            .read("INBOX", &CacheIdentity::Imap(17), &id)
            .unwrap()
            .plain_text
            .as_deref(),
        Some("Offline message")
    );
    assert!(store.read("INBOX", &CacheIdentity::Imap(18), &id).is_none());
    assert!(
        store
            .read("Archive", &CacheIdentity::Imap(17), &id)
            .is_none()
    );
    assert!(
        store
            .read(
                "INBOX",
                &CacheIdentity::Imap(17),
                &MessageId("imap-uid:17:43".into())
            )
            .is_none()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = store.body_path("INBOX", &CacheIdentity::Imap(17), &id);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn jmap_query_checkpoint_survives_serde_and_is_invalidated_with_generation() {
    let identity = CacheIdentity::Jmap {
        account_id: "a".into(),
        mailbox_id: "inbox".into(),
        email_state: Some("email-state".into()),
        query_state: Some("query-state".into()),
    };
    let serialized = serde_json::to_vec(&identity).unwrap();
    let restored: CacheIdentity = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(identity, restored);
    assert_eq!(
        identity.generation(),
        CacheIdentity::Jmap {
            account_id: "a".into(),
            mailbox_id: "inbox".into(),
            email_state: None,
            query_state: None
        }
    );
    let prior: CacheIdentity = serde_json::from_value(serde_json::json!({
            "account_id":"a", "mailbox_id":"inbox", "email_state":"old" }))
    .unwrap();
    assert!(matches!(
        prior,
        CacheIdentity::Jmap {
            query_state: None,
            ..
        }
    ));
}

#[test]
fn jmap_body_and_raw_cache_are_account_scoped_across_mailboxes() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let id = MessageId("jmap-opaque-id".into());
    let inbox = CacheIdentity::Jmap {
        account_id: "a".into(),
        mailbox_id: "inbox".into(),
        email_state: None,
        query_state: None,
    };
    let sent = CacheIdentity::Jmap {
        account_id: "a".into(),
        mailbox_id: "sent".into(),
        email_state: Some("s2".into()),
        query_state: None,
    };
    let other = CacheIdentity::Jmap {
        account_id: "other".into(),
        mailbox_id: "inbox".into(),
        email_state: None,
        query_state: None,
    };
    assert_eq!(
        store.body_path("Inbox", &inbox, &id),
        store.body_path("Sent", &sent, &id)
    );
    assert_ne!(
        store.body_path("Inbox", &inbox, &id),
        store.body_path("Inbox", &other, &id)
    );
    store
        .write("Inbox", &inbox, &body(id.clone(), "shared across folders"))
        .unwrap();
    assert!(store.has_candidate("Sent", &sent, &id));
    assert_eq!(
        store
            .read("Sent", &sent, &id)
            .unwrap()
            .plain_text
            .as_deref(),
        Some("shared across folders")
    );
    assert!(store.read("Inbox", &other, &id).is_none());
    let raw = RawStore {
        dir: dir.path().join("raw"),
    };
    raw.write("Inbox", &inbox, &id, b"From: test\r\n\r\nbody")
        .unwrap();
    assert_eq!(
        raw.read("Sent", &sent, &id),
        Some(b"From: test\r\n\r\nbody".to_vec())
    );
    assert!(raw.read("Inbox", &other, &id).is_none());
}

#[test]
fn legacy_jmap_body_remains_readable_after_shared_cache_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let id = MessageId("old".into());
    let identity = CacheIdentity::Jmap {
        account_id: "a".into(),
        mailbox_id: "inbox".into(),
        email_state: None,
        query_state: None,
    };
    std::fs::create_dir_all(&store.dir).unwrap();
    let key = legacy_jmap_message_key("Inbox", &identity, &id).unwrap();
    let record = serde_json::json!({ "version": VERSION, "mailbox": "Inbox",
            "validity": identity, "body": body(id.clone(), "from old cache") });
    std::fs::write(
        store.dir.join(format!("{key}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .read("Inbox", &identity, &id)
            .unwrap()
            .plain_text
            .as_deref(),
        Some("from old cache")
    );
    assert!(store.read("Sent", &identity, &id).is_none());
}

#[test]
fn oversized_body_and_corrupt_file_are_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    let id = MessageId("imap-uid:4:9".into());
    assert!(compact_body(&body(id.clone(), &"a".repeat(MAX_BODY_FILE_BYTES as usize))).is_none());
    store
        .write("INBOX", &CacheIdentity::Imap(4), &body(id.clone(), "okay"))
        .unwrap();
    std::fs::write(
        store.body_path("INBOX", &CacheIdentity::Imap(4), &id),
        b"not json",
    )
    .unwrap();
    assert!(store.read("INBOX", &CacheIdentity::Imap(4), &id).is_none());
}

#[test]
fn store_prunes_to_disk_budget_without_reading_body_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = BodyStore {
        dir: dir.path().join("bodies"),
    };
    std::fs::create_dir_all(&store.dir).unwrap();
    for index in 0..2 {
        let file = File::create(store.dir.join(format!("{index}.json"))).unwrap();
        file.set_len(MAX_BODY_STORE_BYTES / 2 + 1).unwrap();
    }
    store.prune().unwrap();
    let remaining = std::fs::read_dir(&store.dir).unwrap().count();
    assert_eq!(remaining, 1);
}

#[test]
fn cache_identity_keys_are_stable_and_distinct() {
    assert_eq!(stable_key("account\0imap-a"), stable_key("account\0imap-a"));
    assert_ne!(stable_key("account\0imap-a"), stable_key("account\0imap-b"));
    assert_ne!(stable_key("account\0imap-a"), stable_key("other\0imap-a"));
}
