//! Temporary mailbox-scoped searches and conversation views. Their results
//! must never be committed as a normal mailbox synchronization checkpoint.
use super::*;
use crate::backend::MailboxView;

fn search_filter(mailbox: &str, text: &str) -> Result<Value> {
    let mut conditions = vec![json!({"inMailbox": mailbox})];
    use crate::backend::search::Filter;
    for filter in crate::backend::search::parse(text)? {
        conditions.push(match filter {
            Filter::Text(value) => json!({"text": value}),
            Filter::Field(key, value) => json!({(key): value}),
            Filter::Attachment => json!({"hasAttachment": true}),
            Filter::Read(true) => json!({"hasKeyword": "$seen"}),
            Filter::Read(false) => json!({"notKeyword": "$seen"}),
            Filter::Starred => json!({"hasKeyword": "$flagged"}),
            Filter::Date(key, date) => json!({(key): format!("{date}T00:00:00Z")}),
            Filter::GmailRaw(_) => bail!("gmail: is only supported on Gmail IMAP"),
        });
    }
    Ok(if conditions.len() == 1 {
        conditions.remove(0)
    } else {
        json!({"operator": "AND", "conditions": conditions})
    })
}

impl JmapBackend {
    pub(super) fn native_query(
        &mut self,
        name: &str,
        view: &MailboxView,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        self.checkpoint = None;
        let mailbox = self.mailbox_id(name)?;
        let session = self.session()?;
        let account = session.account_id.clone();
        let max_get = session.max_get;
        let limit = if limit == 0 {
            MAX_PAGE
        } else {
            limit.min(MAX_PAGE)
        };
        if let MailboxView::Thread(thread_id) = view {
            let page = self.thread_page(&mailbox, thread_id, offset, limit)?;
            self.checkpoint = Some(CacheIdentity::Jmap {
                account_id: account,
                mailbox_id: mailbox,
                email_state: None,
                query_state: None,
            });
            return Ok(page);
        }
        let filter = match view {
            MailboxView::Search(text) => search_filter(&mailbox, text)?,
            _ => json!({"inMailbox": mailbox}),
        };
        let mut prefetched = None;
        let (mut page, _) = load_cached_email_page(
            &account,
            &mailbox,
            offset,
            limit,
            max_get,
            None,
            |method, mut args| {
                if method == "Email/query" {
                    args["filter"] = filter.clone();
                    args["collapseThreads"] = json!(matches!(view, MailboxView::Threads));
                    // A separate query namespace: never use canonical queryChanges snapshots.
                    if let Some((query, details)) = self.query_and_get(args.clone())? {
                        if let Some(details) = details {
                            self.remember_response_blobs(&query["ids"], &details);
                            prefetched = Some((query["ids"].clone(), details));
                        }
                        return Ok(query);
                    }
                } else if method == "Email/get"
                    && let Some((ids, details)) = prefetched.take()
                    && ids == args["ids"]
                {
                    return Ok(details);
                }
                self.call_with_blobs(method, args)
            },
        )?;
        self.share_account_headers(&mut page.messages);
        self.checkpoint = Some(CacheIdentity::Jmap {
            account_id: account,
            mailbox_id: mailbox,
            email_state: None,
            query_state: None,
        });
        Ok(page)
    }

    fn thread_page(
        &mut self,
        mailbox: &str,
        thread_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        let response = self.call(
            "Thread/get",
            json!({"ids": [thread_id], "properties": ["id", "emailIds"]}),
        )?;
        let response: GetResponse<Value> = decode_response(response, "Thread/get")?;
        ensure!(
            response.not_found.is_empty() && response.list.len() == 1,
            "JMAP thread no longer exists; refresh the mailbox"
        );
        let thread = &response.list[0];
        ensure!(
            thread["id"].as_str() == Some(thread_id),
            "JMAP Thread/get returned an unexpected ID"
        );
        let ids =
            id_array(&thread["emailIds"]).ok_or_else(|| anyhow!("invalid JMAP thread emailIds"))?;
        ensure!(
            ids.iter().collect::<HashSet<_>>().len() == ids.len(),
            "duplicate JMAP thread IDs"
        );
        let max_get = self.session()?.max_get;
        // Thread/get returns chronological IDs. Restrict to the current mailbox
        // so existing move/delete commands can never target another mailbox.
        let window: Vec<_> = ids.iter().rev().skip(offset).take(limit).cloned().collect();
        let mut details = request_email_details(&window, max_get, |method, mut args| {
            args["properties"]
                .as_array_mut()
                .expect("Email/get properties")
                .push(json!("mailboxIds"));
            self.call_with_blobs(method, args)
        })?;
        let mut messages = Vec::new();
        for id in &window {
            if let Some(email) = details.remove(id) {
                ensure!(
                    email["threadId"].as_str() == Some(thread_id),
                    "JMAP thread membership changed; refresh"
                );
                let memberships = email["mailboxIds"]
                    .as_object()
                    .ok_or_else(|| anyhow!("missing thread mailbox membership"))?;
                if memberships.get(mailbox).and_then(Value::as_bool) == Some(true) {
                    messages.push(summary_from_email(&email)?);
                }
            }
        }
        let next_offset = offset.saturating_add(window.len());
        Ok(MessagePage {
            messages,
            offset,
            next_offset,
            has_more: next_offset < ids.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_is_mailbox_scoped_and_combines_filters() {
        let filter = search_filter(
            "inbox",
            "from:alice subject:\"project update\" is:unread has:attachment hello world",
        )
        .unwrap();
        assert_eq!(
            filter,
            json!({"operator":"AND","conditions":[{"inMailbox":"inbox"},{"from":"alice"},{"subject":"project update"},{"notKeyword":"$seen"},{"hasAttachment":true},{"text":"hello world"}]})
        );
    }
    #[test]
    fn malformed_filters_are_rejected() {
        assert!(search_filter("inbox", "subject:\"open").is_err());
        assert!(search_filter("inbox", "before:2026-02-30").is_err());
        assert!(search_filter("inbox", "from:").is_err());
    }
}
