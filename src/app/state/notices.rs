//! Browsable, newest-first active notices. Selection reuses the same Vim
//! navigation/visual/mark helpers as Contacts and the mail browser. Durable
//! journal resolution is kept separate from simply acknowledging a notice.
use super::*;

#[derive(Debug, Clone, Default)]
pub(crate) struct NoticeBrowser {
    pub selected: usize,
    /// Stable identity so an arriving notice does not steal the cursor.
    pub focus_id: Option<String>,
    pub scroll: usize,
    pub view_height: usize,
    pub count: Option<usize>,
    pub pending_g: bool,
    pub visual_anchor: Option<usize>,
    pub marked: HashSet<String>,
    pub query: String,
    pub searching: bool,
    /// Reuse the same searchable help model/handler used by Contacts.
    pub help: bool,
    pub help_query: String,
    pub help_selected: usize,
    pub help_scroll: usize,
    pub detail: Option<String>,
    pub detail_scroll: u16,
    /// Stable IDs of selected notices awaiting dismiss confirmation.
    pub confirming: Vec<String>,
}

impl NoticeBrowser {
    pub fn ensure_visible(&mut self, length: usize) {
        self.selected = self.selected.min(length.saturating_sub(1));
        let height = self.view_height.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(height) {
            self.scroll = self.selected.saturating_sub(height - 1);
        }
    }

    pub fn move_by(&mut self, length: usize, delta: isize) {
        self.selected = bounded_index(self.selected, length, delta);
        self.ensure_visible(length);
    }

    pub fn in_visual_range(&self, index: usize) -> bool {
        self.visual_anchor.is_some_and(|start| {
            (start.min(self.selected)..=start.max(self.selected)).contains(&index)
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NoticeRow {
    pub id: String,
    pub level: &'static str,
    pub kind: &'static str,
    pub account: String,
    pub summary: String,
    pub details: String,
    pub review: Option<ReviewItem>,
    pub first_seen: u64,
}

/// Namespaced digest to distinguish an intentionally dismissed notice from
/// an ordinary 'seen' warning. Both use the existing private acknowledgement store.
pub(super) fn dismissed_notice_key(id: &str) -> String {
    key_digest(&format!("dismissed-notice:{id}"))
}

impl App {
    pub(super) fn notice_identity_digests(&self) -> Vec<String> {
        self.warning_entries()
            .into_iter()
            .flat_map(|entry| {
                entry
                    .identities
                    .into_iter()
                    .map(|identity| key_digest(&identity))
            })
            .collect()
    }

    /// The notice journal stores *observation* timestamps rather than IMAP
    /// operation timestamps: the existing WAL format has no time field. Old
    /// notices have an unknown age until first inspected; afterward ordering
    /// is stable across restarts without changing the mailbox/outbox journals.
    pub(super) fn all_notice_rows(&self) -> Vec<NoticeRow> {
        let mut rows = Vec::new();
        for notice in self.warning_entries() {
            if notice
                .identities
                .first()
                .is_some_and(|key| key.starts_with("review-"))
            {
                // These aggregate safety notices become one row per resolvable
                // journal record, rather than an unselectable account summary.
                continue;
            }
            let first = notice.identities.first().map_or("", String::as_str);
            let (kind, level) = if first.starts_with("connection:") {
                ("Connection", "Warning")
            } else if first.starts_with("queue-") {
                ("Queue", "Info")
            } else if first.starts_with("configuration:") {
                ("Config", "Warning")
            } else {
                ("Storage", "Error")
            };
            let account = self
                .accounts
                .iter()
                .find(|account| notice.message.starts_with(&format!("{}: ", account.name)))
                .map_or_else(|| "Jerc".to_owned(), |account| account.name.clone());
            let summary = notice
                .message
                .strip_prefix(&format!("{account}: "))
                .unwrap_or(&notice.message)
                .to_owned();
            let digests = notice
                .identities
                .iter()
                .map(|identity| key_digest(identity))
                .collect::<Vec<_>>();
            let first_seen = digests
                .iter()
                .filter_map(|key| self.notice_first_seen.get(key).copied())
                .max()
                .unwrap_or(u64::MAX);
            rows.push(NoticeRow {
                id: key_digest(&digests.join(";")),
                kind,
                level,
                account,
                summary,
                details: notice.message,
                review: None,
                first_seen,
            });
        }
        for (index, account) in self.accounts.iter().enumerate() {
            for entry in &account.deferred_mutations {
                if entry.state != cache::DeferredMutationState::NeedsReview
                    || account
                        .inflight_deferred_mutations
                        .values()
                        .any(|inflight| mutation_notice_key(inflight) == mutation_notice_key(entry))
                {
                    continue;
                }
                let item = ReviewItem {
                    account: index,
                    operation: ReviewOperation::Mutation(entry.clone()),
                };
                let action = match &entry.kind {
                    cache::DeferredMutationKind::Delete => "Delete".to_owned(),
                    cache::DeferredMutationKind::Move { destination } => {
                        format!("Move to {destination}")
                    }
                    cache::DeferredMutationKind::Archive { destination } => {
                        format!("Archive to {destination}")
                    }
                    cache::DeferredMutationKind::Unarchive { destination } => {
                        format!("Unarchive to {destination}")
                    }
                };
                let id = key_digest(&format!(
                    "review-mutation:{}:{}",
                    account.name,
                    mutation_notice_key(entry)
                ));
                rows.push(NoticeRow {
                    first_seen: self.notice_first_seen.get(&id).copied().unwrap_or(u64::MAX),
                    id,
                    kind: "Review",
                    level: "Action",
                    account: account.name.clone(),
                    summary: format!(
                        "{action} · {} · {} message(s) · result uncertain",
                        entry.mailbox,
                        entry.ids.len()
                    ),
                    details: self.review_item_details(&item),
                    review: Some(item),
                });
            }
            for entry in &account.outbox {
                if entry.state != crate::outgoing::outbox::SendState::NeedsReview
                    || account.inflight_outbox.values().any(|id| id == &entry.id)
                {
                    continue;
                }
                let item = ReviewItem {
                    account: index,
                    operation: ReviewOperation::Outbox(entry.id.clone()),
                };
                let id = key_digest(&format!("review-outbox:{}:{}", account.name, entry.id));
                rows.push(NoticeRow {
                    first_seen: self.notice_first_seen.get(&id).copied().unwrap_or(u64::MAX),
                    id,
                    kind: "Review",
                    level: "Action",
                    account: account.name.clone(),
                    summary: format!("SMTP result uncertain · {}", entry.message.subject),
                    details: self.review_item_details(&item),
                    review: Some(item),
                });
            }
        }
        // Unrecorded notices are newest, followed by first-observed time.
        // A stable reversed journal order is used only when times tie.
        rows.reverse();
        rows.sort_by_key(|a| std::cmp::Reverse(a.first_seen));
        rows
    }

    /// Only the UI is dismissed: the IMAP mutation journal and SMTP Outbox
    /// remain untouched. Dismissed notices persist until their underlying
    /// condition clears; a distinct new notice gets a new identity.
    pub(crate) fn notice_rows(&self) -> Vec<NoticeRow> {
        self.all_notice_rows()
            .into_iter()
            .filter(|row| {
                !self
                    .acknowledged_warning_keys
                    .contains(&dismissed_notice_key(&row.id))
            })
            .collect()
    }

    fn dismiss_notices(&mut self, ids: &[String]) -> bool {
        let active = self
            .notice_rows()
            .into_iter()
            .map(|row| row.id)
            .collect::<HashSet<_>>();
        let mut keys = self.acknowledged_warning_keys.clone();
        let mut removed = 0;
        for id in ids {
            if active.contains(id) && keys.insert(dismissed_notice_key(id)) {
                removed += 1;
            }
        }
        if removed == 0 {
            return false;
        }
        // Save before mutating the live view. Failure never silently discards
        // a warning, and no mail/server operation is invoked by dismissal.
        if let Some(store) = &self.notice_ack_store
            && let Err(error) = store.save_state(&keys, &self.notice_first_seen)
        {
            self.status = StatusLine::temporary_error(
                format!("could not save dismissed notices: {error}"),
                Duration::from_secs(6),
            );
            return false;
        }
        self.acknowledged_warning_keys = keys;
        self.status = StatusLine::temporary(
            format!(
                "{removed} {} dismissed",
                if removed == 1 { "notice" } else { "notices" }
            ),
            Duration::from_secs(5),
        );
        true
    }

    /// Reset selection only after dismissal has been saved successfully.
    /// Keep this shared by the confirmed and immediate actions.
    fn finish_notice_dismissal(&mut self) {
        self.notices.visual_anchor = None;
        self.notices.marked.clear();
        self.notices.focus_id = None;
        self.notices.selected = self
            .notices
            .selected
            .min(self.filtered_notices().len().saturating_sub(1));
    }

    pub(crate) fn filtered_notices(&self) -> Vec<NoticeRow> {
        let mut rows = self.notice_rows();
        if !self.notices.query.trim().is_empty() {
            let query = self.notices.query.to_lowercase();
            rows.retain(|row| {
                format!(
                    "{} {} {} {} {}",
                    row.account, row.kind, row.level, row.summary, row.details
                )
                .to_lowercase()
                .contains(&query)
            });
        }
        rows
    }

    pub(super) fn handle_notices_key(&mut self, key: KeyEvent) {
        // Dismissal only hides the selected notices. The selected identities
        // are frozen and checked again before saving, just like mail actions.
        if !self.notices.confirming.is_empty() {
            match key.code {
                KeyCode::Char('y' | 'Y') if key.modifiers.is_empty() => {
                    let ids = std::mem::take(&mut self.notices.confirming);
                    if self.dismiss_notices(&ids) {
                        self.finish_notice_dismissal();
                    }
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') => self.notices.confirming.clear(),
                _ => {}
            }
            return;
        }
        if self.notices.help {
            if crate::app::help_palette::handle_help_key(
                key,
                crate::app::NOTICE_HELP_ENTRIES,
                &mut self.notices.help_query,
                &mut self.notices.help_selected,
                &mut self.notices.help_scroll,
                crate::app::help_palette::HelpKeys::MODAL,
            ) {
                self.notices.help = false;
                self.notices.help_query.clear();
            }
            return;
        }
        if self.notices.detail.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.notices.detail = None;
                    self.notices.detail_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.notices.detail_scroll = self.notices.detail_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.notices.detail_scroll = self.notices.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    self.notices.detail_scroll = self.notices.detail_scroll.saturating_add(15)
                }
                KeyCode::PageUp => {
                    self.notices.detail_scroll = self.notices.detail_scroll.saturating_sub(15)
                }
                KeyCode::Char('g') => self.notices.detail_scroll = 0,
                KeyCode::Char('?') if key.modifiers.is_empty() => {
                    self.notices.help = true;
                    self.notices.help_query.clear();
                    self.notices.help_selected = 0;
                    self.notices.help_scroll = 0;
                }
                KeyCode::Char('d' | 'D')
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    // Act on this detailed notice, not a different row
                    // inserted above it while the reader was open.
                    if let Some(id) = self.notices.detail.take()
                        && let Some(index) =
                            self.filtered_notices().iter().position(|row| row.id == id)
                    {
                        self.notices.selected = index;
                        self.notices.focus_id = Some(id);
                        self.handle_notices_key(key);
                    }
                }
                _ => {}
            }
            return;
        }
        if self.notices.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.notices.searching = false,
                KeyCode::Backspace => {
                    self.notices.query.pop();
                    self.notices.selected = 0;
                    self.notices.scroll = 0;
                    self.notices.focus_id = None;
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.notices.query.clear();
                    self.notices.selected = 0;
                    self.notices.scroll = 0;
                    self.notices.focus_id = None;
                }
                KeyCode::Char(c) if key.modifiers.is_empty() => {
                    self.notices.query.push(c);
                    self.notices.selected = 0;
                    self.notices.scroll = 0;
                    self.notices.focus_id = None;
                }
                _ => {}
            }
            return;
        }
        let rows = self.filtered_notices();
        let length = rows.len();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('j' | 'J') | KeyCode::Enter => {
                    self.notices.move_by(length, SMOOTH_SCROLL_ROWS)
                }
                KeyCode::Char('k' | 'K') => self.notices.move_by(length, -SMOOTH_SCROLL_ROWS),
                _ => {}
            }
            self.notices.focus_id = rows.get(self.notices.selected).map(|row| row.id.clone());
            return;
        }
        if key.modifiers.intersects(KeyModifiers::ALT) {
            return;
        }

        let count = self.notices.count.unwrap_or(1).min(9999);
        let mut preserve = false;
        match key.code {
            KeyCode::Esc
                if self.notices.visual_anchor.is_some() || !self.notices.marked.is_empty() =>
            {
                self.notices.visual_anchor = None;
                self.notices.marked.clear();
            }
            KeyCode::Esc | KeyCode::Char('q' | '!') => {
                // Include notices that arrived while the browser was open.
                self.inspect_warnings();
                self.show_startup_warnings = false;
                self.notices = NoticeBrowser::default();
                return;
            }
            KeyCode::Char('v' | 'V') if length > 0 => {
                if self.notices.visual_anchor.is_some() {
                    self.notices.visual_anchor = None;
                } else {
                    self.notices.visual_anchor = Some(self.notices.selected);
                }
            }
            KeyCode::Char(' ') if length > 0 => {
                if let Some(anchor) = self.notices.visual_anchor.take() {
                    let start = anchor.min(self.notices.selected).min(length - 1);
                    let end = anchor.max(self.notices.selected).min(length - 1);
                    let ids = rows[start..=end]
                        .iter()
                        .map(|row| row.id.clone())
                        .collect::<Vec<_>>();
                    crate::app::vim::toggle_group(&mut self.notices.marked, &ids);
                } else {
                    let id = rows[self.notices.selected].id.clone();
                    if !self.notices.marked.remove(&id) {
                        self.notices.marked.insert(id);
                    }
                    self.notices.move_by(length, 1);
                }
            }
            KeyCode::Enter if length > 0 => {
                self.notices.detail = Some(rows[self.notices.selected].id.clone());
                self.notices.detail_scroll = 0;
            }
            KeyCode::Char('d' | 'D') if length > 0 => {
                let all = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
                let ids = crate::app::vim::selected(
                    &all,
                    &all,
                    self.notices.selected,
                    self.notices.visual_anchor,
                    &self.notices.marked,
                );
                let selected = rows
                    .iter()
                    .filter(|row| ids.contains(&row.id))
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>();
                if key.code == KeyCode::Char('D') {
                    // Uppercase D skips only the prompt; recovery records stay intact.
                    if self.dismiss_notices(&selected) {
                        self.finish_notice_dismissal();
                    }
                    // Do not restore a stale focus ID from rows dismissed above.
                    return;
                }
                self.notices.confirming = selected;
            }
            KeyCode::Char('?') => {
                self.notices.help = true;
                self.notices.help_query.clear();
                self.notices.help_selected = 0;
                self.notices.help_scroll = 0;
            }
            KeyCode::Char('/') => {
                self.notices.searching = true;
                self.notices.visual_anchor = None;
            }
            KeyCode::Char('j') | KeyCode::Down => self.notices.move_by(length, count as isize),
            KeyCode::Char('k') | KeyCode::Up => self.notices.move_by(length, -(count as isize)),
            KeyCode::PageDown => self
                .notices
                .move_by(length, self.notices.view_height.max(1) as isize),
            KeyCode::PageUp => self
                .notices
                .move_by(length, -(self.notices.view_height.max(1) as isize)),
            KeyCode::Char('H') => self.notices.selected = self.notices.scroll,
            KeyCode::Char('M') => {
                self.notices.selected = (self.notices.scroll + self.notices.view_height / 2)
                    .min(length.saturating_sub(1))
            }
            KeyCode::Char('L') => {
                self.notices.selected = (self.notices.scroll
                    + self.notices.view_height.saturating_sub(1))
                .min(length.saturating_sub(1))
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.notices.selected = self
                    .notices
                    .count
                    .map_or(length.saturating_sub(1), |n| n.saturating_sub(1))
                    .min(length.saturating_sub(1));
                self.notices.ensure_visible(length);
            }
            KeyCode::Char('g') => {
                if self.notices.pending_g {
                    self.notices.selected = self
                        .notices
                        .count
                        .map_or(0, |n| n.saturating_sub(1))
                        .min(length.saturating_sub(1));
                    self.notices.ensure_visible(length);
                } else {
                    self.notices.pending_g = true;
                    preserve = true;
                }
            }
            KeyCode::Char('0') if self.notices.count.is_none() => {
                self.notices.selected = 0;
                self.notices.scroll = 0;
            }
            KeyCode::Char(d @ '0'..='9') => {
                self.notices.count = Some(
                    self.notices
                        .count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(d.to_digit(10).unwrap_or(0) as usize)
                        .min(9999),
                );
                preserve = true;
            }
            _ => {}
        }
        if !preserve {
            self.notices.count = None;
            self.notices.pending_g = false;
        }
        self.notices.focus_id = rows.get(self.notices.selected).map(|row| row.id.clone());
    }
}
