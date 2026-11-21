//! Validated RFC 8620 session discovery, capability limits and same-origin checks.
//! Kept separate from transport I/O and push event handling.
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use url::Url;

use super::{
    DEFAULT_GET_BATCH, JMAP_CORE, JMAP_MAIL, JMAP_SUBMISSION, JmapSession, JmapSource,
    MAX_JSON_RESPONSE, MAX_PAGE, MAX_RAW_MESSAGE_BYTES, MAX_SET_BATCH, event_source_endpoint,
    expand_url, value_string,
};

pub(super) fn parse_session(session: &Value, source: &JmapSource) -> Result<JmapSession> {
    ensure!(
        session["capabilities"][JMAP_CORE].is_object()
            && session["capabilities"][JMAP_MAIL].is_object(),
        "JMAP server does not advertise core and mail capabilities"
    );
    let account_id = session["primaryAccounts"][JMAP_MAIL]
        .as_str()
        .ok_or_else(|| anyhow!("JMAP session has no primary mail account"))?
        .to_owned();
    ensure!(
        session["accounts"][&account_id]["accountCapabilities"][JMAP_MAIL].is_object(),
        "JMAP account does not support Mail"
    );
    let api_url = value_string(session, "apiUrl")?;
    let download_url = value_string(session, "downloadUrl")?;
    let upload_url = value_string(session, "uploadUrl")?;
    for candidate in [&api_url, &download_url, &upload_url] {
        // Templates are invalid URLs until substituted, so validate after
        // replacing only the standard JMAP placeholder variables.
        let expanded = expand_url(candidate, &account_id, "example", "message.eml")?;
        let url = Url::parse(&expanded)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == source.origin.host_str()
                && url.port_or_known_default() == source.origin.port_or_known_default()
                && url.username().is_empty()
                && url.password().is_none(),
            "JMAP endpoint origin mismatch; refusing to expose credentials"
        );
    }
    let event_source_url = session["eventSourceUrl"].as_str().map(str::to_owned);
    if let Some(template) = &event_source_url {
        let expanded = event_source_endpoint(template)?;
        let url = Url::parse(&expanded)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == source.origin.host_str()
                && url.port_or_known_default() == source.origin.port_or_known_default()
                && url.username().is_empty()
                && url.password().is_none(),
            "JMAP event source origin mismatch; refusing to expose credentials"
        );
    }
    let core = &session["capabilities"][JMAP_CORE];
    let limit = |key: &str, default: usize, ceiling: usize| {
        core[key].as_u64().map_or(default, |n| {
            usize::try_from(n).unwrap_or(usize::MAX).clamp(1, ceiling)
        })
    };
    let max_get = limit("maxObjectsInGet", DEFAULT_GET_BATCH, MAX_PAGE);
    let max_set = limit("maxObjectsInSet", DEFAULT_GET_BATCH, MAX_SET_BATCH);
    let max_calls = limit("maxCallsInRequest", 1, 16);
    let max_request = limit("maxSizeRequest", MAX_JSON_RESPONSE, MAX_JSON_RESPONSE);
    let max_upload = limit(
        "maxSizeUpload",
        MAX_RAW_MESSAGE_BYTES,
        MAX_RAW_MESSAGE_BYTES,
    );
    let max_concurrent_requests = limit("maxConcurrentRequests", 2, 16);
    let max_concurrent_uploads = limit("maxConcurrentUpload", 1, 16);
    let state = session["state"].as_str().map(str::to_owned);
    // EmailSubmission must use the same account as the Email being sent.
    let submission = session["capabilities"][JMAP_SUBMISSION].is_object()
        && session["accounts"][&account_id]["accountCapabilities"][JMAP_SUBMISSION].is_object()
        && session["accounts"][&account_id]["isReadOnly"].as_bool() != Some(true);
    Ok(JmapSession {
        account_id,
        api_url,
        download_url,
        upload_url,
        event_source_url,
        submission,
        max_get,
        max_set,
        max_calls,
        max_request,
        max_upload,
        max_concurrent_requests,
        max_concurrent_uploads,
        state,
    })
}
