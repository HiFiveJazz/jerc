//! Selective body retrieval uses the same renderer as MIME-based backends.
//! Complex MIME trees deliberately fall back to the original message.
use super::*;
use crate::backend::RemoteAttachment;

const MAX_PART_BYTES: usize = 2 * 1024 * 1024;
const MAX_PARTS: usize = 128;

#[derive(Debug, PartialEq, Eq)]
struct AttachmentRef {
    blob: String,
    name: String,
    size: usize,
    content_type: String,
}

// Be conservative: preserve MIME parser semantics for signed/encrypted mail,
// embedded messages, inline content and calendar/reaction quirks.
fn simple_tree(part: &Value, depth: usize) -> bool {
    if depth > 12 {
        return false;
    }
    let Some(kind) = part["type"].as_str() else {
        return false;
    };
    if matches!(kind, "multipart/mixed" | "multipart/alternative") {
        return part["subParts"].as_array().is_some_and(|parts| {
            !parts.is_empty()
                && parts.len() <= MAX_PARTS
                && parts.iter().all(|p| simple_tree(p, depth + 1))
        });
    }
    !kind.starts_with("multipart/")
        && !kind.starts_with("message/")
        && !matches!(
            kind,
            "application/pkcs7-mime"
                | "application/x-pkcs7-mime"
                | "application/pgp-encrypted"
                | "text/calendar"
                | "text/vnd.google.email-reaction+json"
        )
        && part["cid"].is_null()
        && part["location"].is_null()
        && part["disposition"].as_str() != Some("inline")
}

fn attachment_refs(email: &Value) -> Option<Vec<AttachmentRef>> {
    if !simple_tree(&email["bodyStructure"], 0) {
        return None;
    }
    let parts = email["attachments"].as_array()?;
    if parts.len() > MAX_PARTS {
        return None;
    }
    let mut refs = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if !simple_tree(part, 0) {
            return None;
        }
        let blob = part["blobId"].as_str()?.to_owned();
        if blob.is_empty() {
            return None;
        }
        let name = part["name"]
            .as_str()
            .unwrap_or("")
            .rsplit(['/', '\\'])
            .next()?
            .trim();
        let name = if name.is_empty() || name == "." || name == ".." {
            format!("attachment-{}", index + 1)
        } else {
            name.to_owned()
        };
        refs.push(AttachmentRef {
            blob,
            name,
            size: usize::try_from(part["size"].as_u64()?).ok()?,
            content_type: part["type"].as_str()?.to_owned(),
        });
    }
    Some(refs)
}

fn body_part(email: &Value, key: &str, expected_type: &str) -> Option<Option<String>> {
    let parts = email[key].as_array()?;
    // The existing reader uses one plain and one HTML alternative. Fall back
    // for composite or media bodies, rather than silently dropping content.
    if parts.len() > 1 {
        return None;
    }
    let Some(part) = parts.first() else {
        return Some(None);
    };
    let kind = part["type"].as_str()?;
    if !matches!(kind, "text/plain" | "text/html") {
        return None;
    }
    let id = part["partId"].as_str()?;
    let value = email["bodyValues"].get(id)?;
    if value["isTruncated"].as_bool()? || value["isEncodingProblem"].as_bool()? {
        return None;
    }
    let text = value["value"].as_str()?;
    if text.len() > MAX_PART_BYTES
        || (kind == "text/html" && text.to_ascii_lowercase().contains("cid:"))
    {
        return None;
    }
    Some((kind == expected_type).then(|| text.to_owned()))
}

fn selective_body(id: MessageId, email: &Value) -> Result<Option<MessageBody>> {
    let Some(refs) = attachment_refs(email) else {
        return Ok(None);
    };
    let Some(plain) = body_part(email, "textBody", "text/plain") else {
        return Ok(None);
    };
    let Some(html) = body_part(email, "htmlBody", "text/html") else {
        return Ok(None);
    };
    if plain.is_none() && html.is_none() {
        return Ok(None);
    }
    crate::mail::compact_message_body(id, plain, html, refs.into_iter().map(|a| a.name).collect())
        .map(Some)
}

impl JmapBackend {
    fn body_data(&mut self, id: &MessageId, values: bool) -> Result<Option<Value>> {
        let result = self.call_with_blobs("Email/get", json!({
            "ids": [&id.0], "properties": ["id", "blobId", "bodyStructure", "textBody", "htmlBody", "bodyValues", "attachments"],
            "bodyProperties": ["partId", "blobId", "size", "name", "type", "disposition", "cid", "location", "subParts"],
            "fetchTextBodyValues": values, "fetchHTMLBodyValues": values, "maxBodyValueBytes": MAX_PART_BYTES,
        }));
        // Compatibility fallback only for method/argument rejection, not for
        // transport errors (which must reach the connection recovery path).
        let response = match result {
            Ok(response) => response,
            Err(error)
                if matches!(
                    error.downcast_ref::<MethodErrorType>(),
                    Some(
                        MethodErrorType::UnknownMethod
                            | MethodErrorType::InvalidArguments
                            | MethodErrorType::RequestTooLarge
                    )
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let response: GetResponse<Value> = decode_response(response, "Email/get")?;
        ensure!(
            response.list.len() == 1 && response.not_found.is_empty(),
            "JMAP message no longer exists; refresh the folder"
        );
        let email = response
            .list
            .into_iter()
            .next()
            .expect("one message checked");
        ensure!(
            email["id"].as_str() == Some(id.0.as_str()),
            "JMAP body response returned an unexpected ID"
        );
        Ok(Some(email))
    }

    pub(super) fn native_body(&mut self, id: &MessageId) -> Result<Option<MessageBody>> {
        match self.body_data(id, true)? {
            Some(email) => selective_body(id.clone(), &email),
            None => Ok(None),
        }
    }

    pub(super) fn native_attachments(
        &mut self,
        id: &MessageId,
        mailbox_id: String,
    ) -> Result<Option<Vec<RemoteAttachment>>> {
        if let Some(mut cached) = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.attachments.get(&id.0).cloned())
        {
            let account_id = self.session()?.account_id.clone();
            for entry in &mut cached {
                entry.identity = CacheIdentity::Jmap {
                    account_id: account_id.clone(),
                    mailbox_id: mailbox_id.clone(),
                    email_state: None,
                    query_state: None,
                };
            }
            return Ok(Some(cached));
        }
        let Some(email) = self.body_data(id, false)? else {
            return Ok(None);
        };
        let Some(refs) = attachment_refs(&email) else {
            return Ok(None);
        };
        let identity = CacheIdentity::Jmap {
            account_id: self.session()?.account_id.clone(),
            mailbox_id,
            email_state: None,
            query_state: None,
        };
        let entries: Vec<_> = refs
            .into_iter()
            .map(|part| RemoteAttachment {
                identity: identity.clone(),
                blob_id: part.blob,
                filename: part.name,
                content_type: part.content_type,
                size: part.size,
            })
            .collect();
        if let Ok(mut shared) = self.shared.lock() {
            shared.remember_attachments(&id.0, entries.clone());
        }
        Ok(Some(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message() -> Value {
        json!({"bodyStructure":{"type":"multipart/mixed","subParts":[{"type":"text/plain"},{"type":"application/pdf"}]},
            "textBody":[{"type":"text/plain","partId":"1"}], "htmlBody":[{"type":"text/plain","partId":"1"}],
            "bodyValues":{"1":{"value":"hello","isTruncated":false,"isEncodingProblem":false}},
            "attachments":[{"type":"application/pdf","name":"report.pdf","blobId":"b1","size":15000000}]})
    }
    #[test]
    fn large_attachment_does_not_require_mime_to_render() {
        let body = selective_body(MessageId("id".into()), &message())
            .unwrap()
            .unwrap();
        assert_eq!(body.plain_text.as_deref(), Some("hello"));
        assert_eq!(body.attachment_names, vec!["report.pdf"]);
        assert!(body.html_source.is_none());
    }
    #[test]
    fn truncated_or_complex_bodies_fall_back() {
        let mut email = message();
        email["bodyValues"]["1"]["isTruncated"] = json!(true);
        assert!(
            selective_body(MessageId("id".into()), &email)
                .unwrap()
                .is_none()
        );
        email = message();
        email["bodyStructure"]["type"] = json!("multipart/signed");
        assert!(attachment_refs(&email).is_none());
        email = message();
        email["attachments"][0]["cid"] = json!("inline");
        assert!(attachment_refs(&email).is_none());
    }
}
