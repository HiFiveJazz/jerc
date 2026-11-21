//! Native JMAP delivery. Before submission, the MIME message is uploaded and
//! imported into Drafts. Acceptance and subsequent filing are distinct results.
use super::*;
use crate::backend::NativeSendReceipt;
use crate::model::OutgoingMessage;

#[derive(Deserialize)]
struct Identity {
    id: String,
    email: String,
}

fn select_identity(value: Value, from: &str) -> Result<String> {
    let response: GetResponse<Identity> = decode_response(value, "Identity/get")?;
    ensure!(
        response.not_found.is_empty(),
        "JMAP identity list is incomplete"
    );
    let mut ids = HashSet::new();
    for row in &response.list {
        ensure!(
            !row.id.is_empty() && ids.insert(&row.id),
            "invalid JMAP identity IDs"
        );
    }
    // Prefer an exact address. RFC 8621 also allows identities with a
    // wildcard local part; only a matching domain is eligible in that case.
    if let Some(row) = response
        .list
        .iter()
        .find(|row| row.email.eq_ignore_ascii_case(from))
    {
        return Ok(row.id.clone());
    }
    if let Some((_, domain)) = from.rsplit_once('@')
        && let Some(row) = response.list.iter().find(|row| {
            row.email
                .strip_prefix("*@")
                .is_some_and(|allowed| allowed.eq_ignore_ascii_case(domain))
        })
    {
        return Ok(row.id.clone());
    }
    bail!("JMAP has no sending identity for the configured From address")
}

/// Build the explicit SMTP envelope from lettre (which includes Bcc), not
/// from the formatted MIME bytes (which intentionally omit the Bcc header).
fn submission_envelope(email: &lettre::Message) -> Result<Value> {
    let envelope = email.envelope();
    let from = envelope
        .from()
        .ok_or_else(|| anyhow!("missing submission envelope sender"))?;
    ensure!(
        !envelope.to().is_empty(),
        "missing submission envelope recipients"
    );
    Ok(
        json!({"mailFrom": {"email": from.to_string(), "parameters": null},
        "rcptTo": envelope.to().iter().map(|address| json!({"email": address.to_string(), "parameters": null})).collect::<Vec<_>>() }),
    )
}

fn submission_request(
    account: &str,
    identity: &str,
    email: &str,
    envelope: Value,
    drafts: &str,
    targets: &HashSet<String>,
) -> Value {
    let mut args = json!({"accountId": account, "create": {"m1": {
        "identityId": identity, "emailId": email, "envelope": envelope
    }}});
    if targets.is_empty() {
        args["onSuccessDestroyEmail"] = json!(["#m1"]);
    } else {
        let mut patch = serde_json::Map::new();
        patch.insert("keywords/$draft".into(), Value::Null);
        if !targets.contains(drafts) {
            patch.insert(
                format!("mailboxIds/{}", pointer_segment(drafts)),
                Value::Null,
            );
        }
        for target in targets {
            patch.insert(
                format!("mailboxIds/{}", pointer_segment(target)),
                Value::Bool(true),
            );
        }
        args["onSuccessUpdateEmail"] = json!({"#m1": patch});
    }
    method_request("EmailSubmission/set", args)
}

/// JMAP method invocations are three-element arrays, unlike the object
/// arguments decoded by decode_response. Keep object validation strict there.
fn decode_submission_invocation(value: &Value, label: &str) -> Result<(String, Value, String)> {
    let invalid = || {
        BackendError::new(
            ErrorKind::InvalidResponse,
            format!("invalid JMAP {label} invocation"),
        )
    };
    if !value.as_array().is_some_and(|parts| parts.len() == 3) {
        return Err(invalid().into());
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid().into())
}

/// Once the first response confirms submission, nothing in the implicit
/// Email/set result can turn it into a send error (RFC 8621 §7.5).
fn submission_result(reply: Value, account: &str, email: &str, keep: bool) -> Result<bool> {
    // Parse the first response separately: a malformed *second* response
    // must not erase a confirmed send.
    let mut responses = reply
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("JMAP submission response missing method results"))?
        .iter();
    let first = decode_submission_invocation(
        responses
            .next()
            .ok_or_else(|| anyhow!("empty JMAP submission response"))?,
        "EmailSubmission/set",
    )?;
    let created = extract_correlated(first, "EmailSubmission/set", "m1")?;
    validate_account(&created, account)?;
    let result: ImportResponse = decode_response(created, "EmailSubmission/set")?;
    let successes = result.created.unwrap_or_default();
    let failures = result.not_created.unwrap_or_default();
    if successes.is_empty()
        && failures.len() == 1
        && failures
            .get("m1")
            .is_some_and(|failure| !failure.kind.is_empty())
    {
        return Err(BackendError::new(
            ErrorKind::Other,
            "JMAP submission rejected; server draft retained",
        )
        .into());
    }
    ensure!(
        successes.len() == 1
            && failures.is_empty()
            && successes.get("m1").is_some_and(|row| !row.id.is_empty()),
        "JMAP submission result uncertain; verify Sent before retrying"
    );
    let filing = (|| -> Result<()> {
        let response = responses
            .next()
            .ok_or_else(|| anyhow!("missing implicit Email/set result"))?;
        ensure!(
            responses.next().is_none(),
            "unexpected implicit Email/set results"
        );
        let response = extract_correlated(
            decode_submission_invocation(response, "Email/set")?,
            "Email/set",
            "m1",
        )?;
        validate_account(&response, account)?;
        let expected = HashSet::from([email.to_owned()]);
        let empty = HashSet::new();
        let (updates, destroys) = if keep {
            (&expected, &empty)
        } else {
            (&empty, &expected)
        };
        validate_set_result(response, updates, destroys)?.finish()
    })();
    Ok(filing.is_ok())
}

struct PreparedSubmission {
    email: lettre::Message,
    identity: String,
    drafts: String,
    targets: HashSet<String>,
    sent: Option<String>,
    affected: Vec<String>,
}

impl JmapBackend {
    pub(crate) fn supports_submission(&self) -> Result<bool> {
        Ok(self.session()?.submission)
    }

    pub(crate) fn probe_submission(&mut self, from: &str) -> Result<()> {
        self.connect()?;
        ensure!(
            self.session()?.submission,
            "JMAP server does not advertise submission for this mail account"
        );
        let from: lettre::message::Mailbox = from.parse().context("invalid From mailbox")?;
        self.identity_for(from.email.as_ref()).map(|_| ())
    }

    fn identity_for(&mut self, from: &str) -> Result<String> {
        if let Some(cached) = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.identities.clone())
        {
            return select_identity(cached, from);
        }
        let response = self.call(
            "Identity/get",
            json!({"ids": null, "properties": ["id", "email"]}),
        )?;
        // Validate even if the configured identity is unavailable, so a
        // malformed provider response is not retained for future sends.
        let parsed: GetResponse<Identity> = decode_response(response.clone(), "Identity/get")?;
        ensure!(
            parsed.not_found.is_empty() && !parsed.list.is_empty(),
            "JMAP has no usable sending identities"
        );
        ensure!(
            parsed
                .list
                .iter()
                .all(|identity| !identity.id.is_empty() && !identity.email.is_empty()),
            "invalid JMAP sender identity"
        );
        if let Ok(mut shared) = self.shared.lock() {
            shared.identities = Some(response.clone());
        }
        select_identity(response, from)
    }

    fn prepare_submission(
        &mut self,
        message: &OutgoingMessage,
        sent_mailbox: Option<&str>,
        copy_to: Option<&[String]>,
    ) -> Result<PreparedSubmission> {
        let email = crate::outgoing::build_outgoing(message, self.source.username())?;
        let from = email
            .envelope()
            .from()
            .ok_or_else(|| anyhow!("missing submission sender"))?
            .to_string();
        self.load_mailboxes()?;
        ensure!(
            self.session()?.submission,
            "JMAP submission capability changed; retry after refreshing"
        );
        let identity = self.identity_for(&from)?;
        let role_name = |role| {
            self.mailbox_names
                .iter()
                .find(|(_, id)| {
                    self.mailboxes
                        .get(*id)
                        .is_some_and(|mailbox| mailbox.role == role)
                })
                .map(|(name, _)| name.clone())
        };
        let drafts_name = role_name(MailboxRole::Drafts)
            .ok_or_else(|| anyhow!("JMAP submission requires a Drafts mailbox"))?;
        let drafts = self.mailbox_names[&drafts_name].clone();
        let sent = role_name(MailboxRole::Sent).or_else(|| sent_mailbox.map(str::to_owned));
        let names = match copy_to {
            Some(names) => names.to_vec(),
            None => vec![sent.clone().ok_or_else(|| {
                anyhow!("JMAP has no Sent mailbox; configure copy_to or disable Sent copies")
            })?],
        };
        // Resolve all targets before importing or sending; invalid explicit
        // folders cannot silently cause lost copies or a fallback submission.
        let targets = names
            .iter()
            .map(|name| {
                self.mailbox_names
                    .get(name)
                    .cloned()
                    .ok_or_else(|| anyhow!("JMAP Sent-copy mailbox not found: {name}"))
            })
            .collect::<Result<HashSet<_>>>()?;
        let mut affected = names;
        if !affected.contains(&drafts_name) {
            affected.push(drafts_name);
        }
        Ok(PreparedSubmission {
            email,
            identity,
            drafts,
            targets,
            sent,
            affected,
        })
    }

    pub(super) fn submit_native(
        &mut self,
        message: &OutgoingMessage,
        sent_mailbox: Option<&str>,
        copy_to: Option<&[String]>,
    ) -> Result<Option<NativeSendReceipt>> {
        let Some(required) = self.native_submission else {
            return Ok(None);
        };
        self.connect().map_err(before_submission)?;
        if !self.session()?.submission {
            ensure!(
                !required,
                "JMAP server does not advertise submission; configure an SMTP transport if available"
            );
            return Ok(None);
        }
        let PreparedSubmission {
            email,
            identity,
            drafts,
            targets,
            sent,
            affected,
        } = self
            .prepare_submission(message, sent_mailbox, copy_to)
            .map_err(before_submission)?;
        let envelope = submission_envelope(&email)?;
        let raw = email.formatted();
        // No existing server draft is ever submitted/deleted speculatively.
        // The composer retires the original draft only after acceptance.
        let email_id = self
            .import_message(&drafts, &raw, json!({"$draft": true, "$seen": true}))
            .map_err(after_import_started)?;
        let account = self.session()?.account_id.clone();
        let request =
            submission_request(&account, &identity, &email_id, envelope, &drafts, &targets);
        // Never automatically retry or fall back after this point, even on a
        // transport failure or a malformed response: delivery is uncertain.
        let response = self.send_request(&request).map_err(after_import_started)?;
        let filed = submission_result(response, &account, &email_id, !targets.is_empty())?;
        Ok(Some(NativeSendReceipt {
            sent_mailbox: sent,
            affected_mailboxes: affected,
            sent_copy_saved: filed && !targets.is_empty(),
            sent_copy_warning: (!filed).then(|| if targets.is_empty() {
                "JMAP submission accepted, but removing the temporary draft could not be confirmed".to_owned()
            } else {
                "JMAP submission accepted, but filing the Sent copy could not be confirmed; check Drafts and Sent".to_owned()
            }),
        }))
    }
}

// Only pre-import connection failures can safely requeue without even
// creating a duplicate server draft. Preserve all other error categories.
fn before_submission(error: anyhow::Error) -> anyhow::Error {
    if super::super::classify(&error).is_connection_failure() {
        BackendError::new(
            ErrorKind::PreJmapSubmission,
            "network unavailable before JMAP submission",
        )
        .into()
    } else {
        error
    }
}

// Once importing starts, keep ambiguous outcomes out of automatic replay.
fn after_import_started(error: anyhow::Error) -> anyhow::Error {
    if matches!(
        super::super::classify(&error),
        ErrorKind::PreSmtpOAuth | ErrorKind::PreJmapSubmission
    ) {
        BackendError::new(
            ErrorKind::Offline,
            "network unavailable after JMAP import started; verify Drafts and Sent before retrying",
        )
        .into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outgoing() -> OutgoingMessage {
        OutgoingMessage {
            from: "Sender <sender@example.org>".into(),
            to: "to@example.net".into(),
            cc: "cc@example.net".into(),
            bcc: "hidden@example.net".into(),
            subject: "Native send".into(),
            body: "hello".into(),
            html_body: None,
            attachments: Vec::new(),
            custom_headers: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
        }
    }

    #[test]
    fn envelope_keeps_bcc_but_mime_does_not_disclose_it() {
        let mut message = outgoing();
        message.attachments.push(crate::model::OutgoingAttachment {
            filename: "test.txt".into(),
            content_type: "text/plain".into(),
            data: b"attachment".to_vec(),
        });
        let email = crate::outgoing::build_outgoing(&message, "").unwrap();
        let envelope = submission_envelope(&email).unwrap();
        assert_eq!(envelope["mailFrom"]["email"], "sender@example.org");
        let recipients: HashSet<_> = envelope["rcptTo"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["email"].as_str().unwrap())
            .collect();
        assert_eq!(
            recipients,
            HashSet::from(["to@example.net", "cc@example.net", "hidden@example.net"])
        );
        let raw = String::from_utf8(email.formatted()).unwrap();
        assert!(!raw.contains("hidden@example.net"));
        assert!(raw.contains("test.txt"));
    }

    #[test]
    fn identity_exact_match_precedes_domain_wildcard() {
        let identities = json!({"list": [
            {"id":"wild", "email":"*@example.org"}, {"id":"exact", "email":"sender@example.org"}
        ]});
        assert_eq!(
            select_identity(identities.clone(), "sender@example.org").unwrap(),
            "exact"
        );
        assert_eq!(
            select_identity(identities.clone(), "another@example.org").unwrap(),
            "wild"
        );
        assert!(select_identity(identities, "another@example.net").is_err());
        assert!(
            select_identity(
                json!({"list":[{"id":"x","email":"a@b"},{"id":"x","email":"a@b"}]}),
                "a@b"
            )
            .is_err()
        );
    }

    #[test]
    fn submission_uses_capability_and_moves_one_email_to_multiple_targets() {
        let req = submission_request(
            "a",
            "i",
            "e",
            json!({}),
            "draft/1",
            &HashSet::from(["sent~1".into(), "archive".into()]),
        );
        assert!(
            req["using"]
                .as_array()
                .unwrap()
                .contains(&json!(JMAP_SUBMISSION))
        );
        let args = &req["methodCalls"][0][1];
        assert_eq!(args["create"]["m1"]["emailId"], "e");
        let patch = &args["onSuccessUpdateEmail"]["#m1"];
        assert_eq!(patch["mailboxIds/sent~01"], true);
        assert_eq!(patch["mailboxIds/archive"], true);
        assert_eq!(patch.get("mailboxIds/draft~11"), Some(&Value::Null));
        assert_eq!(patch.get("keywords/$draft"), Some(&Value::Null));
        assert!(args.get("onSuccessDestroyEmail").is_none());
    }

    #[test]
    fn no_copy_destinations_destroy_only_after_acceptance() {
        let req = submission_request("a", "i", "e", json!({}), "draft", &HashSet::new());
        let args = &req["methodCalls"][0][1];
        assert_eq!(args["onSuccessDestroyEmail"], json!(["#m1"]));
        assert!(args.get("onSuccessUpdateEmail").is_none());
    }

    fn accepted() -> Value {
        json!(["EmailSubmission/set", {"accountId":"a", "created":{"m1":{"id":"s"}}}, "m1"])
    }

    #[test]
    fn validates_implicit_email_set_and_preserves_confirmed_acceptance() {
        let updated = json!(["Email/set", {"accountId":"a", "updated":{"e":null}}, "m1"]);
        assert!(
            submission_result(
                json!({"methodResponses":[accepted(),updated]}),
                "a",
                "e",
                true
            )
            .unwrap()
        );
        let destroyed = json!(["Email/set", {"accountId":"a", "destroyed":["e"]}, "m1"]);
        assert!(
            submission_result(
                json!({"methodResponses":[accepted(),destroyed]}),
                "a",
                "e",
                false
            )
            .unwrap()
        );
        for bad in [
            json!(null),
            json!(["error", {"type":"serverFail"}, "m1"]),
            json!(["Email/set", {"accountId":"a", "updated":{"wrong":null}}, "m1"]),
            json!(["Email/set", {"accountId":"b", "updated":{"e":null}}, "m1"]),
            json!(["Email/set", {"accountId":"a", "notUpdated":{"e":{"type":"forbidden"}}}, "m1"]),
        ] {
            assert!(
                !submission_result(json!({"methodResponses":[accepted(),bad]}), "a", "e", true)
                    .unwrap()
            );
        }
        assert!(
            !submission_result(json!({"methodResponses":[accepted()]}), "a", "e", true).unwrap()
        );
    }

    #[test]
    fn rejected_or_uncorrelated_submission_is_not_accepted() {
        for first in [
            json!(["EmailSubmission/set", {"accountId":"a", "notCreated":{"m1":{"type":"forbiddenToSend"}}}, "m1"]),
            json!(["EmailSubmission/set", {"accountId":"a", "created":{"wrong":{"id":"s"}}}, "m1"]),
            json!(["EmailSubmission/set", {"accountId":"b", "created":{"m1":{"id":"s"}}}, "m1"]),
            json!(["EmailSubmission/set", {"accountId":"a", "created":{"m1":{"id":"s"}}}, "wrong"]),
        ] {
            assert!(submission_result(json!({"methodResponses":[first]}), "a", "e", true).is_err());
        }
    }

    #[test]
    fn malformed_submission_invocation_is_rejected_but_filing_failure_preserves_acceptance() {
        for malformed in [
            json!({"method":"EmailSubmission/set", "arguments":{}, "callId":"m1"}),
            json!(["EmailSubmission/set", {}, "m1", "extra"]),
            json!(["EmailSubmission/set", "not an object", "m1"]),
            json!(["EmailSubmission/set", {}, 123]),
        ] {
            assert!(
                submission_result(json!({"methodResponses":[malformed]}), "a", "e", true).is_err()
            );
        }
        let wrong_call = json!(["Email/set", {"accountId":"a", "updated":{"e":null}}, "wrong"]);
        assert!(
            !submission_result(
                json!({"methodResponses":[accepted(),wrong_call]}),
                "a",
                "e",
                true
            )
            .unwrap()
        );
        let updated = json!(["Email/set", {"accountId":"a", "updated":{"e":null}}, "m1"]);
        assert!(
            !submission_result(
                json!({"methodResponses":[accepted(),updated.clone(),updated]}),
                "a",
                "e",
                true
            )
            .unwrap()
        );
    }

    #[test]
    fn only_pre_import_network_failures_are_requeueable() {
        let offline = || BackendError::new(ErrorKind::Offline, "offline").into();
        assert_eq!(
            super::super::super::classify(&before_submission(offline())),
            ErrorKind::PreJmapSubmission
        );
        assert_eq!(
            super::super::super::classify(&after_import_started(offline())),
            ErrorKind::Offline
        );
        let oauth = BackendError::new(ErrorKind::PreSmtpOAuth, "oauth").into();
        assert_eq!(
            super::super::super::classify(&after_import_started(oauth)),
            ErrorKind::Offline
        );
    }
}
