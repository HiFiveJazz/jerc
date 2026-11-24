//! Tests for the parent module; kept alongside the implementation.
use super::*;
fn source() -> JmapSource {
    JmapSource::parse("jmaps://hi%40example.com:secret@mail.example.com").unwrap()
}
#[test]
fn query_get_batch_uses_result_reference_and_distinct_call_ids() {
    let mut args = email_query_arguments("folder", 0, 100);
    args["accountId"] = json!("account");
    let request = query_get_request(args, "account");
    let calls = request["methodCalls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0][2], "q");
    assert_eq!(calls[1][2], "g");
    assert_eq!(
        calls[1][1]["#ids"],
        json!({"resultOf": "q", "name": "Email/query", "path": "/ids"})
    );
    assert_eq!(calls[0][1]["accountId"], calls[1][1]["accountId"]);
    assert!(calls[1][1].get("ids").is_none());
}

#[test]
fn batched_response_validates_correlation_and_supports_safe_get_fallback() {
    let query = json!({"ids": ["a"], "total": 1});
    let details = json!({"list": [fixture_email("a", "Hello")], "notFound": [], "state": "s1"});
    let reply =
        json!({"methodResponses": [["Email/query", query, "q"], ["Email/get", details, "g"]]});
    let (q, g) = extract_query_get(reply).unwrap();
    assert_eq!(q, query);
    assert_eq!(g, Some(details));
    let failed = json!({"methodResponses": [["Email/query", query, "q"], ["error", {"type": "invalidResultReference"}, "g"]]});
    assert_eq!(extract_query_get(failed).unwrap(), (query.clone(), None));
    let wrong = json!({"methodResponses": [["Email/query", query, "q"], ["Email/get", {}, "q"]]});
    assert!(extract_query_get(wrong).is_err());
}

fn prefix_snapshot() -> QuerySnapshot {
    query_snapshot(
        &json!({"ids": ["a", "b", "c"], "total": 100,
            "queryState": "q1", "canCalculateChanges": true}),
        3,
    )
    .unwrap()
}

#[test]
fn query_changes_reuses_unchanged_prefix_and_inserts_new_arrivals() {
    let old = prefix_snapshot();
    let unchanged = json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 100,
            "removed": [], "added": []});
    let next = apply_query_changes(&old, &unchanged).unwrap();
    assert_eq!(next.ids, old.ids);
    assert_eq!(next.state, "q2");
    let arrival = json!({"oldQueryState": "q1", "newQueryState": "q3", "total": 101,
            "removed": [], "added": [{"id": "new", "index": 0}]});
    let next = apply_query_changes(&old, &arrival).unwrap();
    assert_eq!(next.ids, ["new", "a", "b"]);
    assert_eq!(next.total, 101);
}

#[test]
fn query_changes_falls_back_for_unknown_tail_and_malformed_deltas() {
    let old = prefix_snapshot();
    for response in [
        json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 99, "removed": ["b"], "added": []}),
        json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 100, "removed": ["b"], "added": [{"id": "x", "index": 5}]}),
        json!({"oldQueryState": "wrong", "newQueryState": "q2", "total": 100, "removed": [], "added": []}),
        json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 101, "removed": [], "added": [{"id": "a", "index": 0}]}),
        json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 101, "removed": [], "added": [{"id": "d", "index": 101}]}),
    ] {
        assert!(apply_query_changes(&old, &response).is_none(), "{response}");
    }
    assert!(
        query_snapshot(
            &json!({"ids": ["a"], "total": 20, "canCalculateChanges": false, "queryState": "q"}),
            1
        )
        .is_none()
    );
}

#[test]
fn query_changes_handles_reordering_and_complete_small_mailboxes() {
    let old = query_snapshot(
        &json!({"ids": ["a", "b", "c"], "total": 3,
            "queryState": "q1", "canCalculateChanges": true}),
        10,
    )
    .unwrap();
    let response = json!({"oldQueryState": "q1", "newQueryState": "q2", "total": 2,
            "removed": ["b", "c"], "added": [{"id": "c", "index": 0}]});
    assert_eq!(
        apply_query_changes(&old, &response).unwrap().ids,
        ["c", "a"]
    );
}

#[test]
fn deletion_preserves_other_jmap_mailboxes_and_last_membership_destroys() {
    let ids = [MessageId("shared".into()), MessageId("solo".into())];
    let memberships = HashMap::from([
        (
            "shared".to_owned(),
            HashSet::from(["T/r~".to_owned(), "Sent".to_owned()]),
        ),
        ("solo".to_owned(), HashSet::from(["T/r~".to_owned()])),
    ]);
    let (updates, destroyed) = deletion_plan("T/r~", &ids, &memberships).unwrap();
    assert_eq!(destroyed, ["solo"]);
    assert_eq!(updates["shared"]["mailboxIds/T~1r~0"], Value::Null);
    let mut changed = memberships.clone();
    changed.get_mut("shared").unwrap().remove("T/r~");
    assert!(deletion_plan("T/r~", &ids, &changed).is_err());
}

#[test]
fn count_only_mailbox_changes_are_recognized() {
    let initial = json!({"state":"m1","list":[
        {"id":"a","name":"Inbox","role":"inbox","totalEmails":3,"unreadEmails":1}
    ]});
    let (old, _) = parse_mailboxes(initial).unwrap();
    let change = json!({"oldState":"m1","newState":"m2", "hasMoreChanges":false,
            "created":[],"updated":["a"],"destroyed":[],
            "updatedProperties":["unreadEmails","totalEmails"]});
    assert!(apply_mailbox_changes(&old, "m1", &change).unwrap().3);
    let mut mixed = change;
    mixed["updatedProperties"] = json!(["name", "unreadEmails"]);
    assert!(!apply_mailbox_changes(&old, "m1", &mixed).unwrap().3);
}

#[test]
fn mailbox_delta_removes_destroyed_and_fetches_only_changed_ids() {
    let initial = json!({"state": "m1", "list": [
        {"id": "a", "name": "INBOX", "role": "inbox", "totalEmails": 2, "unreadEmails": 1},
        {"id": "b", "name": "Old", "totalEmails": 1, "unreadEmails": 0},
    ]});
    let (old, state) = parse_mailboxes(initial).unwrap();
    let response = json!({"oldState": "m1", "newState": "m2", "hasMoreChanges": false,
            "created": ["c"], "updated": ["a"], "destroyed": ["b"]});
    let (retained, state, ids, counts_only) =
        apply_mailbox_changes(&old, state.as_deref().unwrap(), &response).unwrap();
    assert_eq!(state, "m2");
    assert_eq!(ids, ["c", "a"]);
    assert!(!counts_only);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained["a"].unread, 1);
    let unchanged = json!({"oldState": "m1", "newState": "m1", "hasMoreChanges": false,
            "created": [], "updated": [], "destroyed": []});
    assert!(
        apply_mailbox_changes(&old, "m1", &unchanged)
            .unwrap()
            .2
            .is_empty()
    );
}

#[test]
fn mailbox_delta_rejects_partial_or_inconsistent_history() {
    let old = HashMap::new();
    let mut response = json!({"oldState": "m1", "newState": "m2", "hasMoreChanges": true,
            "created": [], "updated": [], "destroyed": []});
    assert!(apply_mailbox_changes(&old, "m1", &response).is_none());
    response["hasMoreChanges"] = json!(false);
    assert!(apply_mailbox_changes(&old, "wrong", &response).is_none());
    response["created"] = json!(["a"]);
    response["destroyed"] = json!(["a"]);
    assert!(apply_mailbox_changes(&old, "m1", &response).is_none());
}

#[test]
fn repeated_push_tokens_are_coalesced_per_object_type() {
    let event = |email: &str, mailbox: &str| SseFrame {
        event: "state".into(),
        data: json!({"@type": "StateChange", "changed": {"account": {
                "Email": email, "Mailbox": mailbox}}})
        .to_string(),
        ..SseFrame::default()
    };
    let mut states = PushStates::default();
    assert!(states.accept(&event("e1", "m1"), "account"));
    assert!(!states.accept(&event("e1", "m1"), "account"));
    assert!(states.accept(&event("e1", "m2"), "account"));
    assert!(states.accept(&event("e2", "m2"), "account"));
    assert!(!states.accept(&event("e3", "m3"), "other"));
    assert!(!states.accept(&event("e2", "m2"), "account"));
}

#[test]
fn request_gate_releases_permit_and_allows_next_request() {
    let gate = RequestGate::default();
    let first = gate.enter(2).unwrap();
    let second = gate.enter(2).unwrap();
    assert_eq!(*gate.active.lock().unwrap(), 2);
    drop(first);
    assert_eq!(*gate.active.lock().unwrap(), 1);
    let third = gate.enter(2).unwrap();
    drop(second);
    drop(third);
    assert_eq!(*gate.active.lock().unwrap(), 0);
}

#[test]
fn source_defaults_to_https_discovery_and_redacts_password() {
    let source = source();
    assert_eq!(
        source.session_url.as_str(),
        "https://mail.example.com/.well-known/jmap"
    );
    // url::Url does not automatically append '/' to every custom scheme.
    assert_eq!(
        JmapSource::parse("jmaps://user@mail.example.com/")
            .unwrap()
            .session_url
            .as_str(),
        "https://mail.example.com/.well-known/jmap"
    );
    assert_eq!(
        JmapSource::parse("jmaps://user@mail.example.com/jmap/session")
            .unwrap()
            .session_url
            .as_str(),
        "https://mail.example.com/jmap/session"
    );
    assert!(!source.redacted().contains("secret"));
    assert!(JmapSource::parse("jmap://a@example.com").is_err());
    assert!(JmapSource::parse("jmaps://bad%3Aname@mail.example.com").is_err());
    assert!(JmapSource::parse("jmaps://bad%0Aname@mail.example.com").is_err());
    let custom_port = JmapSource::parse("jmaps://hi%40example.com@mail.example.com:8443").unwrap();
    assert!(custom_port.redacted().contains(":8443"));
}
#[test]
fn rejects_cross_origin_session_endpoints() {
    let mut session: Value = serde_json::from_str(concat!(
        r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},"#,
        r#""primaryAccounts":{"urn:ietf:params:jmap:mail":"a1"},"#,
        r#""accounts":{"a1":{"accountCapabilities":{"urn:ietf:params:jmap:mail":{}}}},"#,
        r#""apiUrl":"https://mail.example.com/jmap/api","#,
        r#""downloadUrl":"https://other.example.net/{accountId}/{blobId}/{name}","#,
        r#""uploadUrl":"https://mail.example.com/upload/{accountId}"}"#
    ))
    .unwrap();
    assert!(parse_session(&session, &source()).is_err());
    session["downloadUrl"] = json!("https://mail.example.com/download/{accountId}/{blobId}/{name}");
    assert!(!parse_session(&session, &source()).unwrap().submission);
    session["capabilities"][JMAP_SUBMISSION] = json!({});
    assert!(!parse_session(&session, &source()).unwrap().submission);
    session["accounts"]["a1"]["accountCapabilities"][JMAP_SUBMISSION] = json!({});
    assert!(parse_session(&session, &source()).unwrap().submission);
    session["accounts"]["a1"]["isReadOnly"] = json!(true);
    assert!(!parse_session(&session, &source()).unwrap().submission);
}
#[test]
fn rejects_ambiguous_or_failed_api_responses() {
    assert!(
        extract_method_response(
            json!({"methodResponses": [["Email/get", {}, "wrong"]]}),
            "Email/get"
        )
        .is_err()
    );
    assert!(
        extract_method_response(
            json!({"methodResponses": [["error", {"type":"serverFail"}, "m1"]]}),
            "Email/get"
        )
        .is_err()
    );
    assert!(
        extract_method_response(
            json!({"methodResponses": [["Email/get", {"list": []}, "m1"]]}),
            "Email/get"
        )
        .is_ok()
    );
}
#[test]
fn rejected_or_partial_mutations_must_not_report_success() {
    let a = HashSet::from(["a".to_owned()]);
    let empty = HashSet::new();
    assert!(
        validate_set_result(json!({"updated": {"a": null}, "destroyed": []}), &a, &empty)
            .unwrap()
            .finish()
            .is_ok()
    );
    assert!(validate_set_result(json!({"updated": {}, "destroyed": []}), &a, &empty).is_err());
    assert!(
        validate_set_result(
            json!({"notUpdated": {"a": {"type": "forbidden"}}}),
            &a,
            &empty
        )
        .unwrap()
        .finish()
        .is_err()
    );
    assert!(
        validate_set_result(
            json!({"destroyed": ["a"]}),
            &empty,
            &HashSet::from(["a".to_owned(), "b".to_owned()])
        )
        .is_err()
    );
}
#[test]
fn blob_url_templates_are_escaped() {
    let expanded = expand_url(
        "https://mail.example.com/d/{accountId}/{blobId}/{name}",
        "a/one",
        "b?two",
        "message.eml",
    )
    .unwrap();
    assert!(expanded.contains("a%2Fone/b%3Ftwo/message%2Eeml"));
    assert!(expand_url("https://mail.example.com/{evil}", "a", "b", "c").is_err());
}
#[test]
fn stable_ids_and_flags_are_not_interpreted_as_imap_uids() {
    let row = summary_from_email(&json!({"id": "abc/def", "receivedAt": "2026-11-19T18:31:00Z",
            "keywords": {"$seen": true, "$flagged": true}, "subject": "Hi", "from": [{"email":"a@example.net"}]})).unwrap();
    assert_eq!(row.id.0, "abc/def");
    assert!(row.flags.contains(MessageFlags::SEEN));
    assert!(row.flags.contains(MessageFlags::FLAGGED));
    assert_eq!(pointer_segment("a/b~c"), "a~1b~0c");
}
#[test]
fn parent_cycles_are_rejected() {
    let map = HashMap::from([
        (
            "a".into(),
            JmapMailbox {
                id: "a".into(),
                name: "Inbox".into(),
                parent: Some("b".into()),
                role: MailboxRole::Inbox,
                total: 0,
                unread: 0,
            },
        ),
        (
            "b".into(),
            JmapMailbox {
                id: "b".into(),
                name: "Nested".into(),
                parent: Some("a".into()),
                role: MailboxRole::Other,
                total: 0,
                unread: 0,
            },
        ),
    ]);
    assert!(mailbox_path(&map, "a").is_err());
}
#[test]
fn event_source_url_expands_and_rejects_unsafe_endpoint_origins() {
    let template = "https://mail.example.com/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}";
    assert_eq!(
        event_source_endpoint(template).unwrap(),
        "https://mail.example.com/jmap/eventsource/?types=Email,Mailbox&closeafter=no&ping=30"
    );
    assert!(event_source_endpoint("https://mail.example.com/events?types={types}").is_err());
    assert!(event_source_endpoint(&format!("{template}&token={{untrusted}}")).is_err());
    let mut valid = json!({
        "capabilities": {},
        "primaryAccounts": {},
        "accounts": {"a1": {"accountCapabilities": {}}},
        "apiUrl": "https://mail.example.com/jmap/api",
        "downloadUrl": "https://mail.example.com/d/{accountId}/{blobId}/{name}",
        "uploadUrl": "https://mail.example.com/u/{accountId}",
        "eventSourceUrl": template,
    });
    valid["capabilities"][JMAP_CORE] = json!({});
    valid["capabilities"][JMAP_MAIL] = json!({});
    valid["primaryAccounts"][JMAP_MAIL] = json!("a1");
    valid["accounts"]["a1"]["accountCapabilities"][JMAP_MAIL] = json!({});
    assert!(
        parse_session(&valid, &source())
            .unwrap()
            .event_source_url
            .is_some()
    );
    let mut malicious = valid;
    malicious["eventSourceUrl"] =
        json!(template.replace("mail.example.com", "attacker.example.net"));
    assert!(parse_session(&malicious, &source()).is_err());
}

#[test]
fn sse_reader_rejects_unbounded_lines_without_consuming_unbounded_memory() {
    let mut reader = BufReader::new(&b"data: hello\n\n"[..]);
    let mut line = String::new();
    assert_eq!(read_bounded_sse_line(&mut reader, &mut line).unwrap(), 12);
    assert_eq!(line, "data: hello\n");
    let oversized = vec![b'x'; 128 * 1024];
    let mut reader = BufReader::new(oversized.as_slice());
    let mut unused = String::new();
    assert!(read_bounded_sse_line(&mut reader, &mut unused).is_err());
}

#[test]
fn sse_mail_and_mailbox_changes_trigger_only_for_matching_account() {
    let mut frame = SseFrame::default();
    for line in [
        ": heartbeat",
        "event: state",
        "id: 789",
        "data: {\"@type\":\"StateChange\",\"changed\":{\"a1\":{\"Email\":\"updated\"}}}",
    ] {
        assert!(!frame.push_line(line));
    }
    assert!(frame.push_line(""));
    assert_eq!(frame.id.as_deref(), Some("789"));
    assert!(frame.is_mail_state("a1"));
    assert!(!frame.is_mail_state("a2"));

    let mut mailbox = SseFrame::default();
    mailbox.push_line("event: state");
    mailbox
        .push_line("data: {\"@type\":\"StateChange\",\"changed\":{\"a1\":{\"Mailbox\":\"new\"}}}");
    assert!(mailbox.is_mail_state("a1"));

    let mut unrelated = SseFrame::default();
    unrelated.push_line("event: ping");
    unrelated.push_line("data: {\"interval\":30}");
    assert!(!unrelated.is_mail_state("a1"));
    let mut bad_json = SseFrame::default();
    bad_json.push_line("event: state");
    bad_json.push_line("data: {not json}");
    assert!(!bad_json.is_mail_state("a1"));
    let mut wrong_type = SseFrame::default();
    wrong_type.push_line("event: state");
    wrong_type.push_line("data: {\"@type\":\"Fake\",\"changed\":{\"a1\":{\"Email\":\"x\"}}}");
    assert!(!wrong_type.is_mail_state("a1"));
}

// Protocol-contract fixtures intentionally exercise the same query/get
// helpers used by the live backend. They need no credentials or server.
fn fixture_email(id: &str, subject: &str) -> Value {
    json!({
        "id": id, "blobId": format!("blob-{id}"), "threadId": format!("thread-{id}"), "receivedAt": "2026-11-20T08:00:00Z",
        "subject": subject, "from": [{"name": "Sender", "email": "sender@example.net"}],
        "to": [{"email": "recipient@example.net"}], "cc": null, "bcc": null,
        "size": 123, "hasAttachment": false, "keywords": {"$seen": true},
        "messageId": ["msg@example.net"], "inReplyTo": ["root@example.net"],
        "references": ["grandparent@example.net", "root@example.net"],
    })
}

fn cached_hint() -> MailboxSyncHint {
    MailboxSyncHint {
        identity: CacheIdentity::Jmap {
            account_id: "account".into(),
            mailbox_id: "folder".into(),
            email_state: Some("s1".into()),
            query_state: None,
        },
        uid_next: None,
        messages: vec![
            summary_from_email(&fixture_email("old", "Cached")).unwrap(),
            summary_from_email(&fixture_email("changed", "Old subject")).unwrap(),
        ],
    }
}

fn changes(state: &str, updated: Value) -> Value {
    json!({"oldState": "s1", "newState": state, "hasMoreChanges": false,
            "created": [], "updated": updated, "destroyed": []})
}

#[test]
fn unchanged_jmap_page_skips_email_get_entirely() {
    let hint = cached_hint();
    let mut calls = Vec::new();
    let (page, state) = load_cached_email_page(
        "account",
        "folder",
        0,
        20,
        50,
        Some(&hint),
        |method, args| {
            calls.push(method.to_owned());
            match method {
                "Email/query" => Ok(json!({"ids": ["old", "changed"], "total": 2})),
                "Email/changes" => {
                    assert_eq!(args["sinceState"], "s1");
                    Ok(changes("s2", json!([])))
                }
                _ => bail!("unexpected download of cached rows"),
            }
        },
    )
    .unwrap();
    assert_eq!(calls, ["Email/query", "Email/changes"]);
    assert_eq!(state.as_deref(), Some("s2"));
    assert!(Arc::ptr_eq(
        &page.messages[0].headers,
        &hint.messages[0].headers
    ));
}

#[test]
fn jmap_delta_only_fetches_new_or_changed_ids_in_query_order() {
    let hint = cached_hint();
    let mut requested = Vec::new();
    let (page, state) = load_cached_email_page("account", "folder", 0, 20, 50, Some(&hint), |method, args| {
            match method {
                "Email/query" => Ok(json!({"ids": ["new", "old", "changed"], "total": 3})),
                "Email/changes" => Ok(changes("s2", json!(["changed"]))),
                "Email/get" => {
                    requested.push(args["ids"].clone());
                    Ok(json!({"state": "s2", "list": [fixture_email("changed", "Updated"), fixture_email("new", "Arrival")], "notFound": []}))
                }
                _ => bail!("unexpected method"),
            }
        }).unwrap();
    assert_eq!(requested, [json!(["new", "changed"])]);
    assert_eq!(state.as_deref(), Some("s2"));
    assert_eq!(
        page.messages
            .iter()
            .map(|m| m.subject.as_str())
            .collect::<Vec<_>>(),
        ["Arrival", "Cached", "Updated"]
    );
    assert!(Arc::ptr_eq(
        &page.messages[1].headers,
        &hint.messages[0].headers
    ));
}

#[test]
fn expired_or_incomplete_checkpoint_refetches_full_page() {
    let hint = cached_hint();
    for expired in [true, false] {
        let (page, state) = load_cached_email_page("account", "folder", 0, 20, 50, Some(&hint), |method, args| {
                match method {
                    "Email/query" => Ok(json!({"ids": ["old"], "total": 1})),
                    "Email/changes" if expired => Err(method_failure("Email/changes", MethodErrorType::CannotCalculateChanges)),
                    "Email/changes" => Ok(json!({"oldState": "s1", "newState": "s2", "hasMoreChanges": true})),
                    "Email/get" => { assert_eq!(args["ids"], json!(["old"]));
                        Ok(json!({"state": "s3", "list": [fixture_email("old", "Fresh")], "notFound": []})) },
                    _ => bail!("unexpected method"),
                }
            }).unwrap();
        assert_eq!(state.as_deref(), Some("s3"));
        assert_eq!(page.messages[0].subject, "Fresh");
    }
}

#[test]
fn racing_state_change_never_labels_old_rows_with_new_token() {
    let hint = cached_hint();
    let mut batches = Vec::new();
    let (page, state) = load_cached_email_page("account", "folder", 0, 20, 50, Some(&hint), |method, args| {
            match method {
                "Email/query" => Ok(json!({"ids": ["old", "new"], "total": 2})),
                "Email/changes" => Ok(changes("s2", json!([]))),
                "Email/get" => {
                    batches.push(args["ids"].clone());
                    if batches.len() == 1 { Ok(json!({"state": "s3", "list": [fixture_email("new", "New")], "notFound": []})) }
                    else { Ok(json!({"state": "s3", "list": [fixture_email("old", "Reconciled"), fixture_email("new", "New")], "notFound": []})) }
                }
                _ => bail!("unexpected method"),
            }
        }).unwrap();
    assert_eq!(batches, [json!(["new"]), json!(["old", "new"])]);
    assert_eq!(page.messages[0].subject, "Reconciled");
    assert_eq!(state.as_deref(), Some("s3"));
}

#[test]
fn checkpoint_from_another_mailbox_is_never_reused() {
    let hint = cached_hint();
    let (page, _) = load_cached_email_page(
        "account",
        "recreated-folder",
        0,
        20,
        50,
        Some(&hint),
        |method, _| match method {
            "Email/query" => Ok(json!({"ids": ["old"], "total": 1})),
            "Email/get" => {
                Ok(json!({"state": "s3", "list": [fixture_email("old", "Fresh")], "notFound": []}))
            }
            _ => bail!("must not reuse state from a different mailbox"),
        },
    )
    .unwrap();
    assert_eq!(page.messages[0].subject, "Fresh");
}

#[test]
fn blob_cache_is_bounded_and_account_scoped() {
    let mut shared = Shared::default();
    for i in 0..1025 {
        shared.remember_blob("account", &i.to_string(), "blob");
    }
    assert_eq!(shared.blobs.len(), 1024);
    assert!(!shared.blobs.contains_key(&("account".into(), "0".into())));
    shared.remember_blob("another", "1024", "other-blob");
    assert_eq!(shared.blobs[&("account".into(), "1024".into())], "blob");
    assert_eq!(
        shared.blobs[&("another".into(), "1024".into())],
        "other-blob"
    );
}

#[test]
fn email_get_uses_only_rfc8621_supported_header_properties() {
    let args = email_get_arguments(&["id-one".into()]);
    assert_eq!(args["ids"], json!(["id-one"]));
    let properties = args["properties"].as_array().unwrap();
    for field in [
        "messageId",
        "inReplyTo",
        "references",
        "blobId",
        "keywords",
        "receivedAt",
    ] {
        assert!(properties.contains(&json!(field)), "missing {field}");
    }
    assert!(
        properties
            .iter()
            .all(|property| !property.as_str().unwrap().contains(":asText"))
    );
    // These are structured message-id fields; :asText was the exact
    // invalidArguments request that broke nonempty Sent and Trash.
    for forbidden in [
        "header:Message-ID:asText",
        "header:In-Reply-To:asText",
        "header:References:asText",
    ] {
        assert!(!properties.contains(&json!(forbidden)));
    }
}

#[test]
fn jmap_structured_thread_ids_have_the_same_shape_as_imap_headers() {
    let row = summary_from_email(&fixture_email("opaque-id", "Sent message")).unwrap();
    assert_eq!(row.message_id.as_deref(), Some("msg@example.net"));
    assert_eq!(row.in_reply_to.as_deref(), Some("root@example.net"));
    assert_eq!(
        &*row.references,
        &["grandparent@example.net", "root@example.net"]
    );
    assert_eq!(row.subject, "Sent message");
    assert_eq!(row.from_address, "sender@example.net");
    assert_eq!(row.to, "recipient@example.net");
    assert_eq!(row.size, 123);
    assert!(row.flags.contains(MessageFlags::SEEN));
}

#[test]
fn null_missing_and_empty_message_id_arrays_are_accepted() {
    let mut email = fixture_email("id", "subject");
    email["messageId"] = Value::Null;
    email["inReplyTo"] = json!([]);
    email.as_object_mut().unwrap().remove("references");
    let row = summary_from_email(&email).unwrap();
    assert!(row.message_id.is_none());
    assert!(row.in_reply_to.is_none());
    assert!(row.references.is_empty());
}

#[test]
fn malformed_message_id_values_are_not_silently_converted() {
    for value in [
        json!("not-an-array"),
        json!([12]),
        json!({"id":"wrong-type"}),
    ] {
        let mut email = fixture_email("id", "subject");
        email["references"] = value;
        assert!(summary_from_email(&email).is_err());
    }
}

#[test]
fn sent_and_deleted_items_load_complete_nonempty_pages() {
    for (mailbox_id, subject) in [("sent-id", "Sent Items"), ("deleted-id", "Deleted Items")] {
        let mut methods = Vec::new();
        let page = load_email_page(mailbox_id, 0, 25, 10, |method, args| {
            methods.push(method.to_owned());
            match method {
                "Email/query" => {
                    assert_eq!(args["filter"]["inMailbox"], mailbox_id);
                    assert_eq!(args["sort"][0]["property"], "receivedAt");
                    assert_eq!(args["sort"][0]["isAscending"], false);
                    Ok(json!({"ids": ["opaque-sent-1"], "total": 1}))
                }
                "Email/get" => {
                    assert_eq!(args["properties"], json!(EMAIL_SUMMARY_PROPERTIES));
                    assert_eq!(args["ids"], json!(["opaque-sent-1"]));
                    Ok(json!({"list": [fixture_email("opaque-sent-1", subject)], "notFound": []}))
                }
                _ => bail!("unexpected method"),
            }
        })
        .unwrap();
        assert_eq!(methods, ["Email/query", "Email/get"]);
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].subject, subject);
        assert_eq!(page.next_offset, 1);
        assert!(!page.has_more);
    }
}

#[test]
fn empty_mailbox_does_not_issue_email_get() {
    let mut calls = Vec::new();
    let page = load_email_page("empty-trash", 0, 100, 50, |method, args| {
        calls.push(method.to_owned());
        assert_eq!(method, "Email/query");
        assert_eq!(args["filter"]["inMailbox"], "empty-trash");
        Ok(json!({"ids": [], "total": 0}))
    })
    .unwrap();
    assert_eq!(calls, ["Email/query"]);
    assert!(page.messages.is_empty());
    assert_eq!(page.next_offset, 0);
    assert!(!page.has_more);
}

#[test]
fn get_batches_and_reorders_server_results_to_query_order() {
    let ids: Vec<String> = (0..5).map(|n| format!("id-{n}")).collect();
    let mut sizes = Vec::new();
    let results = request_email_details(&ids, 2, |method, args| {
        assert_eq!(method, "Email/get");
        let chunk: Vec<String> = args["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect();
        sizes.push(chunk.len());
        Ok(json!({"list": chunk.iter().rev()
                .map(|id| fixture_email(id, id)).collect::<Vec<_>>(), "notFound": []}))
    })
    .unwrap();
    assert_eq!(sizes, [2, 2, 1]);
    assert_eq!(results.len(), 5);
    assert_eq!(results["id-0"]["subject"], "id-0");

    let page = load_email_page("sent", 10, 5, 2, |method, _args| {
        if method == "Email/query" {
            Ok(json!({"ids": ids, "total": 15}))
        } else {
            Ok(json!({"list": [fixture_email("id-0", "subject")], "notFound": []}))
        }
    });
    assert!(page.is_err(), "every queried ID must be accounted for");
}

#[test]
fn message_deleted_between_query_and_get_skips_row_without_replaying_page() {
    let page = load_email_page("trash", 20, 3, 10, |method, args| match method {
        "Email/query" => Ok(json!({"ids": ["gone", "alive", "also-alive"], "total": 25})),
        "Email/get" => {
            assert_eq!(args["ids"], json!(["gone", "alive", "also-alive"]));
            Ok(json!({"list": [fixture_email("also-alive", "Second"),
                        fixture_email("alive", "First")], "notFound": ["gone"]}))
        }
        _ => bail!("unexpected method"),
    })
    .unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].id.0, "alive");
    assert_eq!(page.messages[1].id.0, "also-alive");
    assert_eq!(page.offset, 20);
    assert_eq!(page.next_offset, 23);
    assert!(page.has_more);
}

#[test]
fn multiple_missing_messages_and_all_deleted_page_are_safe() {
    let page = load_email_page("trash", 0, 2, 10, |method, _args| match method {
        "Email/query" => Ok(json!({"ids": ["gone-a", "gone-b"], "total": 2})),
        "Email/get" => Ok(json!({"list": [], "notFound": ["gone-a", "gone-b"]})),
        _ => bail!("unexpected method"),
    })
    .unwrap();
    assert!(page.messages.is_empty());
    assert_eq!(page.next_offset, 2);
    assert!(!page.has_more);
}

#[test]
fn email_get_rejects_incomplete_duplicate_and_unsolicited_results() {
    let ids = vec!["wanted".to_owned()];
    for bad_response in [
        json!({"list": []}),
        json!({"list": [fixture_email("other", "Unsolicited")], "notFound": []}),
        json!({"list": [fixture_email("wanted", "Once"), fixture_email("wanted", "Twice")], "notFound": []}),
        json!({"list": [fixture_email("wanted", "Found")], "notFound": ["wanted"]}),
        json!({"list": [], "notFound": ["other"]}),
        json!({"list": [], "notFound": [123]}),
        json!({"list": [], "notFound": "wrong"}),
        json!({"list": "wrong", "notFound": ["wanted"]}),
        json!({"list": [{}], "notFound": []}),
    ] {
        assert!(
            request_email_details(&ids, 10, |_, _| Ok(bad_response.clone())).is_err(),
            "malformed Email/get response was accepted: {bad_response}"
        );
    }
}

#[test]
fn email_get_rejects_invalid_batch_sizes_and_duplicate_query_ids() {
    let ids = ["x".to_owned()];
    assert!(request_email_details(&ids, 0, |_, _| unreachable!()).is_err());
    assert!(request_email_details(&["x".into(), "x".into()], 2, |_, _| unreachable!()).is_err());
    assert!(
        request_email_details(
            &["x".into(), "y".into(), "x".into()],
            2,
            |_, _| unreachable!()
        )
        .is_err()
    );
    let empty: [String; 0] = [];
    assert!(
        request_email_details(&empty, 1, |_, _| unreachable!())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn malformed_queries_and_oversized_pages_are_rejected() {
    for invalid in [
        json!({}),
        json!({"ids": "wrong"}),
        json!({"ids": [null]}),
        json!({"ids": [""]}),
        json!({"ids": ["a", "b", "c"]}),
    ] {
        let page = load_email_page("sent", 0, 2, 50, |method, _| {
            assert_eq!(method, "Email/query");
            Ok(invalid.clone())
        });
        assert!(page.is_err(), "accepted: {invalid}");
    }
}

#[test]
fn query_limits_are_bounded_and_nonzero() {
    for (asked, expected) in [(0, MAX_PAGE), (1, 1), (1000, MAX_PAGE)] {
        let page = load_email_page("inbox", 12, asked, 10, |method, args| {
            assert_eq!(method, "Email/query");
            assert_eq!(args["position"], 12);
            assert_eq!(args["limit"], expected);
            Ok(json!({"ids": [], "total": 12}))
        })
        .unwrap();
        assert_eq!(page.offset, 12);
        assert_eq!(page.next_offset, 12);
        assert!(!page.has_more);
    }
}

#[test]
fn all_standard_flags_and_missing_flags_behave_correctly() {
    let mut email = fixture_email("flags", "test");
    email["keywords"] = json!({
        "$seen": true, "$answered": true, "$flagged": true,
        "$draft": true, "$forwarded": true,
    });
    let row = summary_from_email(&email).unwrap();
    for flag in [
        MessageFlags::SEEN,
        MessageFlags::ANSWERED,
        MessageFlags::FLAGGED,
        MessageFlags::DRAFT,
        MessageFlags::FORWARDED,
    ] {
        assert!(row.flags.contains(flag));
    }
    email["keywords"] = json!({});
    let row = summary_from_email(&email).unwrap();
    assert_eq!(row.flags, MessageFlags::empty());
    email["receivedAt"] = json!("malformed-date");
    let row = summary_from_email(&email).unwrap();
    assert_eq!(row.timestamp, 0);
}

#[test]
fn method_call_and_method_response_keep_correlation_and_error_safety() {
    let request = method_request("Email/get", email_get_arguments(&["opaque".into()]));
    assert_eq!(request["using"], json!([JMAP_CORE, JMAP_MAIL]));
    assert_eq!(request["methodCalls"][0][0], "Email/get");
    assert_eq!(request["methodCalls"][0][2], "m1");
    let good = json!({"methodResponses": [["Email/get", {"list": [], "notFound": []}, "m1"]]});
    assert!(extract_method_response(good, "Email/get").is_ok());
    for bad in [
        json!({"methodResponses": [["error", {"type": "invalidArguments", "description": "secret=do-not-show"}, "m1"]]}),
        json!({"methodResponses": [["Email/get", {}, "wrong-call"]]}),
        json!({"methodResponses": [["Mailbox/get", {}, "m1"]]}),
        json!({"methodResponses": []}),
        json!({"methodResponses": [["Email/get", {}, "m1"], ["Email/get", {}, "m2"]]}),
    ] {
        let error = extract_method_response(bad, "Email/get")
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("do-not-show"),
            "error leaked server-controlled description"
        );
    }
}

#[test]
fn partial_mutations_fail_instead_of_reporting_success() {
    let updates = HashSet::from(["one".to_owned(), "two".to_owned()]);
    let empty = HashSet::new();
    let report = validate_set_result(
        json!({"updated": {"one": null},
            "notUpdated": {"two": {"type":"forbidden", "description":"secret"}}}),
        &updates,
        &empty,
    )
    .unwrap();
    assert_eq!((report.confirmed, report.rejected), (1, 1));
    let error = report.finish().unwrap_err();
    assert_eq!(super::super::classify(&error), ErrorKind::PartialMutation);
    assert!(!error.to_string().contains("secret"));
    for bad in [
        json!({"updated": {"one":null, "wrong":null}}),
        json!({"updated": {"one":null, "two":null}, "notUpdated": {"one":{"type":"forbidden"}}}),
        json!({"updated": {"one":null, "two":null}, "notUpdated": []}),
        json!({"updated": {"one":null, "two":false}}),
    ] {
        assert!(validate_set_result(bad, &updates, &empty).is_err());
    }
    assert!(validate_set_result(json!({"destroyed": ["one", "one"]}), &empty, &updates).is_err());
}

#[test]
fn mutation_batches_report_progress_without_replaying_uncertain_writes() {
    let ids = [
        MessageId("one".into()),
        MessageId("two".into()),
        MessageId("three".into()),
    ];
    let mut calls = 0;
    let error = run_mutation_batches(&ids, 1, |_| {
        calls += 1;
        if calls == 1 {
            Ok(MutationReport {
                confirmed: 1,
                rejected: 0,
            })
        } else {
            Err(BackendError::new(ErrorKind::Timeout, "JMAP timed out").into())
        }
    })
    .unwrap_err();
    assert_eq!(calls, 2);
    assert_eq!(super::super::classify(&error), ErrorKind::Timeout);
    assert!(error.to_string().contains("1 confirmed"));
    let mut calls = 0;
    let error = run_mutation_batches(&ids, 1, |_| {
        calls += 1;
        Ok(MutationReport {
            confirmed: 0,
            rejected: 1,
        })
    })
    .unwrap_err();
    assert_eq!(calls, 3);
    assert!(error.to_string().contains("3 rejected"));
}

#[test]
fn method_errors_are_typed_and_account_correlation_is_checked() {
    let error = extract_method_response(
        json!({"methodResponses": [["error",
            {"type":"cannotCalculateChanges", "description":"connection reset secret"}, "m1"]]}),
        "Email/changes",
    )
    .unwrap_err();
    assert!(changes_unavailable(&error));
    assert_eq!(super::super::classify(&error), ErrorKind::StaleState);
    assert!(!format!("{error:#}").contains("secret"));
    assert!(!changes_unavailable(&anyhow!("(cannotCalculateChanges)")));
    assert!(validate_account(&json!({"accountId":"a"}), "a").is_ok());
    assert!(validate_account(&json!({"accountId":"b"}), "a").is_err());
    assert!(validate_account(&json!({}), "a").is_err());
}

#[test]
fn import_result_requires_exact_creation_id() {
    assert!(validate_import_result(json!({"created":{"m1":{"id":"server-id"}}})).is_ok());
    for bad in [
        json!({"created":{"wrong":{"id":"server-id"}}}),
        json!({"created":{"m1":{"id":""}}}),
        json!({"created":{"m1":{"id":"server-id"}}, "notCreated":{"m1":{"type":"forbidden"}}}),
    ] {
        assert!(validate_import_result(bad).is_err());
    }
}

#[test]
fn typed_mailbox_response_rejects_invalid_counts() {
    for row in [
        json!({"id":"a", "name":"Inbox", "totalEmails":1}),
        json!({"id":"a", "name":"Inbox", "totalEmails":1, "unreadEmails":2}),
        json!({"id":"a", "name":"Inbox", "totalEmails":1, "unreadEmails":"1"}),
    ] {
        assert!(parse_mailboxes(json!({"list":[row]})).is_err());
    }
}

#[test]
fn session_rejects_missing_mail_capability_and_honors_get_batch_limit() {
    let mut session = json!({
        "capabilities": {},
        "primaryAccounts": {},
        "accounts": {"a1": {"accountCapabilities": {}}},
        "apiUrl": "https://mail.example.com/jmap/api",
        "downloadUrl": "https://mail.example.com/jmap/download/{accountId}/{blobId}/{name}",
        "uploadUrl": "https://mail.example.com/jmap/upload/{accountId}",
    });
    session["capabilities"][JMAP_CORE] = json!({"maxObjectsInGet": 3});
    session["capabilities"][JMAP_MAIL] = json!({});
    session["primaryAccounts"][JMAP_MAIL] = json!("a1");
    session["accounts"]["a1"]["accountCapabilities"][JMAP_MAIL] = json!({});
    assert_eq!(parse_session(&session, &source()).unwrap().max_get, 3);
    session["capabilities"][JMAP_CORE]["maxObjectsInGet"] = json!(0);
    assert_eq!(parse_session(&session, &source()).unwrap().max_get, 1);
    session["capabilities"][JMAP_CORE]["maxObjectsInGet"] = json!(10000);
    assert_eq!(
        parse_session(&session, &source()).unwrap().max_get,
        MAX_PAGE
    );
    session["capabilities"][JMAP_CORE] = json!({"maxObjectsInGet": 100, "maxObjectsInSet": 7,
            "maxCallsInRequest": 2, "maxSizeRequest": 4096, "maxSizeUpload": 1024,
            "maxConcurrentRequests": 1, "maxConcurrentUpload": 3});
    session["state"] = json!("session-1");
    let limits = parse_session(&session, &source()).unwrap();
    assert_eq!(
        (limits.max_get, limits.max_set, limits.max_calls),
        (100, 7, 2)
    );
    assert_eq!((limits.max_request, limits.max_upload), (4096, 1024));
    assert_eq!(
        (
            limits.max_concurrent_requests,
            limits.max_concurrent_uploads
        ),
        (1, 3)
    );
    assert_eq!(limits.state.as_deref(), Some("session-1"));
    session["accounts"]["a1"]["accountCapabilities"][JMAP_MAIL] = Value::Null;
    assert!(parse_session(&session, &source()).is_err());
}

#[test]
fn mailbox_roles_and_nested_paths_preserve_sent_and_trash() {
    let map = HashMap::from([
        (
            "root".into(),
            JmapMailbox {
                id: "root".into(),
                name: "Project".into(),
                parent: None,
                role: MailboxRole::Other,
                total: 0,
                unread: 0,
            },
        ),
        (
            "sent".into(),
            JmapMailbox {
                id: "sent".into(),
                name: "Sent Items".into(),
                parent: Some("root".into()),
                role: role(Some("sent")),
                total: 3,
                unread: 0,
            },
        ),
        (
            "trash".into(),
            JmapMailbox {
                id: "trash".into(),
                name: "Deleted Items".into(),
                parent: None,
                role: role(Some("trash")),
                total: 2,
                unread: 0,
            },
        ),
    ]);
    assert_eq!(mailbox_path(&map, "sent").unwrap(), "Project/Sent Items");
    assert_eq!(mailbox_path(&map, "trash").unwrap(), "Deleted Items");
    assert_eq!(map["sent"].role, MailboxRole::Sent);
    assert_eq!(map["trash"].role, MailboxRole::Trash);
    assert_eq!(pointer_segment("id/a~b"), "id~1a~0b");
    assert!(mailbox_path(&map, "missing").is_err());
}

/// Opt-in, read-only interoperability check against the user's own JMAP
/// account. Does not send, delete, move, or mark anything read.
/// Use `JERC_JMAP_TEST_ACCOUNT=name` to choose among configured accounts.
#[test]
#[ignore = "requires a configured live JMAP server and explicit opt-in"]
fn live_jmap_mailbox_read_only_smoke() -> Result<()> {
    let config_file = std::env::var_os("JERC_JMAP_TEST_CONFIG");
    let config = crate::config::Config::load(config_file.as_deref().map(std::path::Path::new))?;
    let selected = std::env::var("JERC_JMAP_TEST_ACCOUNT").ok();
    let account = config
        .accounts
        .iter()
        .find(|account| {
            JmapSource::supports(&account.source)
                && selected.as_deref().is_none_or(|name| name == account.name)
        })
        .ok_or_else(|| anyhow!("no matching configured JMAP account"))?;
    let mut backend = JmapBackend::new(
        JmapSource::parse(&account.source)?,
        account.source_cred_cmd.clone(),
        account,
    )?;
    backend.connect()?;
    let folders = backend.list_mailboxes()?;
    ensure!(!folders.is_empty(), "JMAP server returned no mailboxes");
    // This is where the earlier invalidArguments bug occurred: nonempty
    // Sent and Deleted Items caused Email/get to reject three :asText
    // properties. Exercise those roles plus Inbox, without mutations.
    let mut inspected = 0;
    for folder in &folders {
        if !matches!(
            &folder.role,
            MailboxRole::Inbox | MailboxRole::Sent | MailboxRole::Trash
        ) {
            continue;
        }
        let _status = backend.mailbox_status(&folder.name)?;
        let page = backend.open_mailbox(&folder.name, 0, 3)?;
        ensure!(
            page.messages.len() <= 3,
            "JMAP page exceeded requested size"
        );
        println!(
            "JMAP {:?} folder: {} ({} message rows)",
            folder.role,
            folder.name,
            page.messages.len()
        );
        if let Some(row) = page.messages.first() {
            // Verify blob-ID resolution and the raw MIME download path on
            // a bounded small sample; do not fetch large attachments.
            if row.size < 256 * 1024 {
                let raw = backend.fetch_raw(&folder.name, &row.id)?;
                ensure!(!raw.is_empty(), "JMAP raw download was empty");
            }
        }
        inspected += 1;
    }
    ensure!(
        inspected > 0,
        "server has no Inbox/Sent/Trash mailboxes to test"
    );
    Ok(())
}

#[test]
fn inbox_query_delta_counts_membership_additions_not_reordering() {
    let delta = json!({"oldQueryState":"a", "newQueryState":"b", "removed":["reordered", "gone"],
        "added":[{"id":"new", "index":0}, {"id":"reordered", "index":1}]});
    assert_eq!(
        inbox_query_delta("a", &delta),
        Some(("b".into(), 1, Some(MessageId("new".into()))))
    );
    assert!(inbox_query_delta("other", &delta).is_none());
    let duplicate = json!({"oldQueryState":"a", "newQueryState":"b", "removed":[],
        "added":[{"id":"new", "index":0}, {"id":"new", "index":1}]});
    assert!(inbox_query_delta("a", &duplicate).is_none());
}
