//! Local, account-independent contacts. No network API or additional crate.
//! The book is bounded; learned senders never replace a name explicitly saved
//! by the user. Only contact details (not message bodies) are persisted.
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_CONTACTS: usize = 4_000;
pub const MAX_SUGGESTIONS: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Contact {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub saved: bool,
    #[serde(default)]
    pub favorite: bool,
}

impl Contact {
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            return self.email.clone();
        }
        let quoted = self
            .name
            .chars()
            .any(|c| matches!(c, ',' | '"' | '<' | '>' | ';' | '(' | ')' | '\\'))
            || self.name.starts_with(' ')
            || self.name.ends_with(' ');
        let label = if quoted {
            format!(
                "\"{}\"",
                self.name.replace('\\', "\\\\").replace('"', "\\\"")
            )
        } else {
            self.name.clone()
        };
        format!("{label} <{}>", self.email)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ContactBook {
    #[serde(default)]
    pub entries: Vec<Contact>,
}

impl ContactBook {
    /// Expose the user's own configured sender identities in recipient search.
    /// These are derived in memory at startup, without contacting providers
    /// or automatically changing contacts.json. Existing saved names win.
    pub fn include_account_identities(&mut self, accounts: &[crate::config::AccountConfig]) {
        for account in accounts {
            let identity = crate::outgoing::sender_from_account(account);
            if let Ok(mailbox) = identity.parse::<lettre::message::Mailbox>() {
                let email = mailbox.email.to_string();
                if self.find(&email).is_none_or(|index| !self.entries[index].saved) {
                    let name = mailbox.name.unwrap_or_else(|| account.name.clone());
                    let _ = self.upsert(&name, &email, true);
                }
            }
        }
    }

    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .or_else(dirs::config_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("jerc")
            .join("contacts.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let book: Self = serde_json::from_slice(&contents)
            .with_context(|| format!("invalid contacts in {}", path.display()))?;
        if book.entries.len() > MAX_CONTACTS {
            anyhow::bail!("contacts file exceeds {MAX_CONTACTS} entries; refusing to truncate it");
        }
        Ok(book)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("contacts path has no parent directory")?;
        std::fs::create_dir_all(parent).context("create contacts directory")?;
        // NamedTempFile uses private permissions and a unique name. Persist is
        // an atomic same-directory rename, so a crash cannot truncate the book.
        let mut temp =
            tempfile::NamedTempFile::new_in(parent).context("create contacts temp file")?;
        serde_json::to_writer_pretty(&mut temp, self).context("serialize contacts")?;
        temp.write_all(b"\n")?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(path).context("replace contacts file")?;
        Ok(())
    }

    pub fn find(&self, email: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.email.eq_ignore_ascii_case(email))
    }

    pub fn upsert(&mut self, name: &str, email: &str, saved: bool) -> Result<bool> {
        let email = email.trim();
        let name = name.trim();
        if email.len() > 320
            || name.len() > 200
            || email.chars().any(char::is_control)
            || name.chars().any(char::is_control)
            || email.parse::<lettre::Address>().is_err()
        {
            anyhow::bail!("enter a valid email address and a name without control characters");
        }
        if let Some(index) = self.find(email) {
            let contact = &mut self.entries[index];
            if contact.saved && !saved {
                return Ok(false);
            }
            let new_name = if name.is_empty() && !saved {
                contact.name.clone()
            } else {
                name.to_owned()
            };
            if contact.name == new_name && contact.saved == (contact.saved || saved) {
                return Ok(false);
            }
            contact.name = new_name;
            contact.saved |= saved;
            return Ok(true);
        }
        if self.entries.len() >= MAX_CONTACTS {
            if let Some(index) = self
                .entries
                .iter()
                .position(|contact| !contact.saved && !contact.favorite)
            {
                self.entries.remove(index);
            } else {
                anyhow::bail!("contact book is full");
            }
        }
        self.entries.push(Contact {
            name: name.to_owned(),
            email: email.to_owned(),
            saved,
            favorite: false,
        });
        Ok(true)
    }

    pub fn edit(&mut self, original: Option<&str>, name: &str, email: &str) -> Result<()> {
        let email = email.trim();
        if let Some(existing) = self.find(email)
            && original.is_none_or(|old| !self.entries[existing].email.eq_ignore_ascii_case(old))
        {
            anyhow::bail!("that email address is already in your contacts");
        }
        if let Some(old) = original
            && let Some(index) = self.find(old)
        {
            // Validate before replacing the original contact.
            let mut trial = Self::default();
            trial.upsert(name, email, true)?;
            let favorite = self.entries[index].favorite;
            self.entries[index] = Contact {
                name: name.trim().to_owned(),
                email: email.to_owned(),
                saved: true,
                favorite,
            };
            return Ok(());
        }
        self.upsert(name, email, true)?;
        Ok(())
    }

    pub fn filtered(&self, query: &str) -> Vec<usize> {
        let mut items: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, contact)| match_score(query, contact).map(|score| (index, score)))
            .collect();
        items.sort_by(|(ia, sa), (ib, sb)| {
            let a = &self.entries[*ia];
            let b = &self.entries[*ib];
            sb.cmp(sa)
                .then_with(|| b.favorite.cmp(&a.favorite))
                .then_with(|| b.saved.cmp(&a.saved))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.email.cmp(&b.email))
        });
        items.into_iter().map(|(i, _)| i).collect()
    }

    pub fn suggest(&self, query: &str, excluded: &HashSet<String>) -> Vec<usize> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        let query_len = query.chars().count();
        let short_prefix = (query_len <= 2).then(|| query.to_lowercase());
        self.filtered(query)
            .into_iter()
            .filter(|&i| {
                let contact = &self.entries[i];
                if excluded.contains(&contact.email.to_ascii_lowercase()) {
                    return false;
                }
                // One letter is only enough for explicitly saved/favorite
                // contacts. For one or two letters, require a name or email
                // local-part prefix; fuzzy matching across the email domain
                // otherwise makes "ja" match "john@example.org".
                if let Some(prefix) = &short_prefix {
                    if query_len == 1 && !(contact.saved || contact.favorite) {
                        return false;
                    }
                    let matches_word = |text: &str| {
                        text.split(|ch: char| !ch.is_alphanumeric())
                            .any(|word| word.to_lowercase().starts_with(prefix))
                    };
                    return matches_word(&contact.name)
                        || matches_word(contact.email.split('@').next().unwrap_or_default());
                }
                true
            })
            .take(MAX_SUGGESTIONS)
            .collect()
    }
}

fn match_score(query: &str, contact: &Contact) -> Option<i32> {
    if query.trim().is_empty() {
        return Some(0);
    }
    let needle = query.trim().to_lowercase();
    let name = contact.name.to_lowercase();
    let email = contact.email.to_lowercase();
    if email == needle {
        return Some(200);
    }
    if name == needle {
        return Some(180);
    }
    if name.starts_with(&needle) {
        return Some(160);
    }
    if email.starts_with(&needle) {
        return Some(140);
    }
    if name.contains(&needle) {
        return Some(120);
    }
    if email.contains(&needle) {
        return Some(100);
    }
    let fuzzy = |haystack: &str| {
        let mut chars = haystack.chars();
        needle.chars().all(|n| chars.by_ref().any(|c| c == n))
    };
    if fuzzy(&name) || fuzzy(&email) {
        Some(25)
    } else {
        None
    }
}

/// Return the range of the recipient currently under the editor cursor. Commas
/// inside quoted names or angle-bracket addresses are not separators.
pub fn recipient_segment(text: &str, char_cursor: usize) -> (usize, usize) {
    let cursor = char_cursor.min(text.chars().count());
    let mut quoted = false;
    let mut escaped = false;
    let mut angle = false;
    let mut start = 0usize;
    let mut boundaries = Vec::new();
    for (offset, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                boundaries.push((start, offset));
                start = offset + 1;
            }
            _ => {}
        }
    }
    boundaries.push((start, text.len()));
    let cursor_byte = char_to_byte(text, cursor);
    let chosen = boundaries
        .iter()
        .position(|(_, end)| cursor_byte <= *end)
        .unwrap_or(boundaries.len().saturating_sub(1));
    boundaries[chosen]
}

pub fn recipient_query(text: &str, cursor: usize) -> String {
    let (start, end) = recipient_segment(text, cursor);
    text[start..end].trim().to_owned()
}

pub fn char_to_byte(text: &str, char_index: usize) -> usize {
    crate::app::vim::byte_offset(text, char_index)
}

pub fn replace_recipient(text: &mut String, cursor: usize, contact: &Contact) -> usize {
    replace_recipients(text, cursor, &[contact])
}

/// Replace the active recipient with one or more contacts, leaving other
/// comma-separated recipients intact. Useful for inserting a visual/marked
/// range from the contacts manager without discarding To/Cc/Bcc text.
pub fn replace_recipients(text: &mut String, cursor: usize, contacts: &[&Contact]) -> usize {
    if contacts.is_empty() {
        return cursor;
    }
    let (start, end) = recipient_segment(text, cursor);
    let mut formatted = contacts
        .iter()
        .map(|contact| contact.display())
        .collect::<Vec<_>>()
        .join(", ");
    // The segment starts immediately after a separator comma, so replacing it
    // also removes any existing indentation. Restore exactly one space.
    if start != 0 {
        formatted.insert(0, ' ');
    }
    text.replace_range(start..end, &formatted);
    let pos = start + formatted.len();
    if pos == text.len() {
        text.push_str(", ");
    }
    let mut new_cursor = text[..pos].chars().count();
    if text[pos..].starts_with(", ") && pos + 2 == text.len() {
        new_cursor += 2;
    }
    new_cursor
}

/// Parse a list of already complete addresses without splitting quoted commas.
/// Used only for contact discovery and duplicate suggestion filtering.
pub fn recipients(text: &str) -> Vec<(String, String)> {
    let mut output = Vec::new();
    let mut position = 0;
    while position < text.len() {
        let segment = &text[position..];
        let (start, end) = recipient_segment(segment, 0);
        // recipient_segment(0) always resolves the first segment.
        let piece = segment[start..end].trim();
        if let Some((name, email)) = parse_address_piece(piece) {
            output.push((name, email));
        }
        if end >= segment.len() {
            break;
        }
        position += end + 1;
    }
    output
}

fn parse_address_piece(piece: &str) -> Option<(String, String)> {
    let (name, email) = if let Some(open) = piece.rfind('<') {
        if !piece.ends_with('>') {
            return None;
        }
        (
            piece[..open].trim().trim_matches('"').replace("\\\"", "\""),
            piece[open + 1..piece.len() - 1].trim(),
        )
    } else {
        (String::new(), piece.trim())
    };
    if email.parse::<lettre::Address>().is_ok() {
        Some((name, email.to_owned()))
    } else {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContactsModal {
    pub query: String,
    /// Cached filtered/sorted contact indices; recomputed only on edits/search.
    pub matches: Vec<usize>,
    pub searching: bool,
    pub selected: usize,
    pub scroll: usize,
    pub view_height: usize,
    pub pending_count: Option<usize>,
    pub pending_g: bool,
    pub smooth_target: Option<usize>,
    /// Visual-mode anchor in the currently filtered/sorted list. Both v and V
    /// select complete contact rows: there is no characterwise list selection.
    pub visual_anchor: Option<usize>,
    pub visual_linewise: bool,
    /// Stable email identities, not row indices (favorites/search re-sort rows).
    pub marked: HashSet<String>,
    pub edit: Option<ContactEdit>,
    pub confirm_delete: bool,
    pub delete_review: crate::app::ReviewScroll,
    pub help: bool,
    pub help_query: String,
    pub help_selected: usize,
    pub help_scroll: usize,
}

impl ContactsModal {
    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        self.visual_anchor
            .filter(|_| !self.matches.is_empty())
            .map(|anchor| (anchor.min(self.selected), anchor.max(self.selected)))
    }

    #[must_use]
    pub fn in_visual_range(&self, row: usize) -> bool {
        self.visual_range()
            .is_some_and(|(start, end)| (start..=end).contains(&row))
    }

    pub fn toggle_mark(&mut self, book: &ContactBook) {
        if let Some(&id) = self.matches.get(self.selected)
            && let Some(contact) = book.entries.get(id)
        {
            let identity = contact.email.to_ascii_lowercase();
            if !self.marked.remove(&identity) {
                self.marked.insert(identity);
            }
            // Yazi-style marking: space also advances the cursor.
            self.navigate(self.matches.len(), 1);
        }
    }

    /// In visual mode, Space converts the whole range into persistent marks.
    /// If every entry was already marked it clears them instead.
    pub fn mark_visual_range(&mut self, book: &ContactBook) {
        let Some((start, end)) = self.visual_range() else {
            return;
        };
        let addresses: Vec<String> = self.matches[start..=end]
            .iter()
            .filter_map(|&id| book.entries.get(id))
            .map(|contact| contact.email.to_ascii_lowercase())
            .collect();
        crate::app::vim::toggle_group(&mut self.marked, &addresses);
        self.visual_anchor = None;
    }

    /// A visual range plus independently marked rows form the action set.
    /// Marks intentionally persist through search/filter changes, so users
    /// can gather recipients in multiple searches. The UI displays their count.
    #[must_use]
    pub fn action_ids(&self, book: &ContactBook) -> Vec<usize> {
        // Convert stable email marks to current book IDs only at action time:
        // contact reorderings and search changes cannot change the recipients.
        let marked = book.entries.iter().enumerate()
            .filter(|(_, contact)| self.marked.contains(&contact.email.to_ascii_lowercase()))
            .map(|(index, _)| index)
            .collect::<HashSet<_>>();
        let all = (0..book.entries.len()).collect::<Vec<_>>();
        crate::app::vim::selected(
            &self.matches, &all, self.selected, self.visual_anchor, &marked,
        )
    }

    pub fn clear_selection(&mut self) {
        self.visual_anchor = None;
        self.marked.clear();
    }

    pub fn navigate(&mut self, length: usize, delta: isize) {
        self.smooth_target = None;
        self.selected = crate::app::navigation::bounded_index(self.selected, length, delta);
        self.ensure_visible(length);
    }
    pub fn ensure_visible(&mut self, length: usize) {
        self.selected = self.selected.min(length.saturating_sub(1));
        let height = self.view_height.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll.saturating_add(height) {
            self.scroll = self.selected.saturating_sub(height.saturating_sub(1));
        }
    }
    pub fn smooth(&mut self, length: usize, offset: isize) {
        let base = self.smooth_target.unwrap_or(self.selected);
        let target = crate::app::navigation::bounded_index(base, length, offset);
        self.smooth_target = (target != self.selected).then_some(target);
    }
    pub fn advance(&mut self, length: usize) {
        if let Some(target) = self.smooth_target {
            self.navigate(length, if target > self.selected { 1 } else { -1 });
            if self.selected != target {
                self.smooth_target = Some(target);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContactEdit {
    pub original_email: Option<String>,
    pub name: String,
    pub email: String,
    pub active_email: bool,
    pub cursor: usize,
    pub mode: crate::app::ComposeMode,
    pub visual_anchor: Option<usize>,
    pub pending_yank: bool,
    pub pending_delete: bool,
    undo: Vec<(String, String, bool, usize)>,
    redo: Vec<(String, String, bool, usize)>,
}

impl ContactEdit {
    pub fn new(contact: Option<&Contact>) -> Self {
        let name = contact.map_or_else(String::new, |c| c.name.clone());
        let email = contact.map_or_else(String::new, |c| c.email.clone());
        let cursor = name.chars().count();
        Self {
            original_email: contact.map(|c| c.email.clone()),
            name,
            email,
            active_email: false,
            cursor,
            mode: crate::app::ComposeMode::Insert,
            visual_anchor: None,
            pending_yank: false,
            pending_delete: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
    pub fn value(&self) -> &str {
        if self.active_email {
            &self.email
        } else {
            &self.name
        }
    }
    fn value_mut(&mut self) -> &mut String {
        if self.active_email {
            &mut self.email
        } else {
            &mut self.name
        }
    }
    pub fn toggle(&mut self) {
        self.active_email = !self.active_email;
        self.cursor = self.value().chars().count();
        self.visual_anchor = None;
        self.pending_yank = false;
        self.pending_delete = false;
    }
    fn snapshot(&self) -> (String, String, bool, usize) {
        (self.name.clone(), self.email.clone(), self.active_email, self.cursor)
    }
    fn restore(&mut self, snapshot: (String, String, bool, usize)) {
        (self.name, self.email, self.active_email, self.cursor) = snapshot;
        self.visual_anchor = None;
        self.mode = crate::app::ComposeMode::Normal;
    }
    fn record_edit(&mut self) {
        if self.undo.len() == 100 { self.undo.remove(0); }
        self.undo.push(self.snapshot());
        self.redo.clear();
    }
    pub fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(prev);
        }
    }
    pub fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(next);
        }
    }
    pub fn delete_range(&mut self, start: usize, end: usize) {
        let end = end.min(self.value().chars().count());
        let start = start.min(end);
        if start == end { return; }
        self.record_edit();
        let a = char_to_byte(self.value(), start);
        let b = char_to_byte(self.value(), end);
        self.value_mut().replace_range(a..b, "");
        self.cursor = start;
        self.visual_anchor = None;
    }
    pub fn clear_line(&mut self) {
        self.delete_range(0, self.value().chars().count());
        self.mode = crate::app::ComposeMode::Insert;
    }
    pub fn selected_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        if self.mode != crate::app::ComposeMode::Visual { return None; }
        if self.pending_delete || self.pending_yank { return None; }
        crate::app::vim::visual_chars(anchor, self.cursor, self.value().chars().count())
    }
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selected_range()?;
        Some(self.value().chars().skip(start).take(end - start).collect())
    }
    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() { return; }
        self.record_edit();
        let offset = char_to_byte(self.value(), self.cursor);
        self.value_mut().insert_str(offset, text);
        self.cursor += text.chars().count();
        self.visual_anchor = None;
    }
    pub fn insert(&mut self, ch: char) {
        if !ch.is_control() {
            self.insert_text(&ch.to_string());
        }
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_range(self.cursor - 1, self.cursor);
        }
    }
    pub fn move_word(&mut self, forward: bool) {
        self.cursor = crate::app::vim::word_start(self.value(), self.cursor, !forward, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn autocontact_display_quotes_parenthesized_names_for_send() {
        let recipients = [
            Contact { name: "Jasmeet (Jazz) Bhatia".into(), email: "jazz@example.org".into(), saved: true, favorite: false },
            Contact { name: "Jasmeet (Japan) Bhatia".into(), email: "japan@example.org".into(), saved: true, favorite: false },
            Contact { name: "James Schneider".into(), email: "james@example.org".into(), saved: true, favorite: false },
            Contact { name: "Björk (B)".into(), email: "bjork@example.org".into(), saved: true, favorite: false },
        ].iter().map(Contact::display).collect::<Vec<_>>().join(", ");
        assert!(recipients.contains("\"Jasmeet (Jazz) Bhatia\""));
        assert!(crate::outgoing::validate_recipients(&recipients, "", "").is_ok());
    }

    #[test]
    fn contact_vim_field_operations_preserve_unicode_and_undo() {
        let mut editor = ContactEdit::new(None);
        editor.insert_text("Jasmeet (Jazz)");
        editor.mode = crate::app::ComposeMode::Normal;
        editor.cursor = 0;
        editor.move_word(true);
        assert!(editor.cursor > 0);
        editor.clear_line();
        assert!(editor.name.is_empty());
        editor.undo();
        assert_eq!(editor.name, "Jasmeet (Jazz)");
        editor.toggle();
        editor.insert_text("jazz@example.org");
        assert_eq!(editor.email, "jazz@example.org");
        editor.clear_line();
        assert!(editor.email.is_empty());
        editor.undo();
        assert_eq!(editor.email, "jazz@example.org");
    }

    #[test]
    fn own_configured_address_autocompletes_without_overwriting_saved_contact() {
        let config: crate::config::Config = toml::from_str(
            "[[accounts]]\nname = \"Main\"\nsource = \"imap\"\nemail = \"me@example.org\"\nfrom = \"Jazz <me@example.org>\"\n",
        )
        .unwrap();
        let mut book = super::ContactBook::default();
        book.include_account_identities(&config.accounts);
        assert_eq!(book.suggest("j", &std::collections::HashSet::new()), vec![0]);
        assert_eq!(book.entries[0].display(), "Jazz <me@example.org>");
        book.edit(Some("me@example.org"), "Preferred Name", "me@example.org")
            .unwrap();
        book.include_account_identities(&config.accounts);
        assert_eq!(book.entries.len(), 1);
        assert_eq!(book.entries[0].name, "Preferred Name");
    }

    #[test]
    fn recipient_parser_preserves_quoted_commas_and_unicode() {
        let list = "\"Doe, Jane\" <jane@example.org>, Þór <thor@example.org>";
        assert_eq!(recipients(list).len(), 2);
        assert_eq!(recipients(list)[0].0, "Doe, Jane");
        assert_eq!(recipients(list)[1].0, "Þór");
    }
    #[test]
    fn replace_only_active_recipient() {
        let contact = Contact {
            name: "Jane Doe".into(),
            email: "jane@example.org".into(),
            saved: true,
            favorite: false,
        };
        let mut s = "alice@example.org, ja, other@example.org".to_owned();
        let cursor = "alice@example.org, ja".chars().count();
        let point = replace_recipient(&mut s, cursor, &contact);
        assert_eq!(
            s,
            "alice@example.org, Jane Doe <jane@example.org>, other@example.org"
        );
        assert_eq!(
            point,
            "alice@example.org, Jane Doe <jane@example.org>"
                .chars()
                .count()
        );
    }
    #[test]
    fn completing_recipient_normalizes_missing_separator_space() {
        let contact = Contact {
            name: "Jane Doe".into(),
            email: "jane@example.org".into(),
            saved: true,
            favorite: false,
        };
        let mut text = "alice@example.org,ja, other@example.org".to_owned();
        let cursor = "alice@example.org,ja".chars().count();
        let point = replace_recipient(&mut text, cursor, &contact);
        assert_eq!(
            text,
            "alice@example.org, Jane Doe <jane@example.org>, other@example.org"
        );
        assert_eq!(
            point,
            "alice@example.org, Jane Doe <jane@example.org>"
                .chars()
                .count()
        );
    }
    #[test]
    fn completing_recipient_preserves_quoted_comma_in_prior_name() {
        let contact = Contact {
            name: "Zoë, Jane".into(),
            email: "jane@example.org".into(),
            saved: true,
            favorite: false,
        };
        let mut text = "\"Doe, John\" <john@example.org>,  ja, other@example.org".to_owned();
        let cursor = "\"Doe, John\" <john@example.org>,  ja".chars().count();
        let point = replace_recipient(&mut text, cursor, &contact);
        assert_eq!(
            text,
            "\"Doe, John\" <john@example.org>, \"Zoë, Jane\" <jane@example.org>, other@example.org"
        );
        assert_eq!(
            point,
            "\"Doe, John\" <john@example.org>, \"Zoë, Jane\" <jane@example.org>"
                .chars()
                .count()
        );
    }

    #[test]
    fn marked_contacts_survive_favorite_sort_and_search() {
        let mut book = ContactBook::default();
        book.upsert("Alba", "alba@example.org", true).unwrap();
        book.upsert("Bea", "bea@example.org", true).unwrap();
        book.upsert("Cara", "cara@example.org", true).unwrap();
        let mut modal = ContactsModal {
            matches: book.filtered(""),
            view_height: 3,
            ..Default::default()
        };
        modal.toggle_mark(&book);
        assert_eq!(modal.selected, 1);
        assert!(modal.marked.contains("alba@example.org"));
        book.entries[0].favorite = true;
        modal.matches = book.filtered("");
        modal.query = "bea".into();
        modal.matches = book.filtered(&modal.query);
        let ids = modal.action_ids(&book);
        assert!(ids.contains(&0));
    }

    #[test]
    fn visual_contact_range_uses_counted_and_smooth_motions() {
        let mut book = ContactBook::default();
        for id in 0..40 {
            book.upsert(
                &format!("Name{id:02}"),
                &format!("name{id:02}@example.org"),
                true,
            )
            .unwrap();
        }
        let mut modal = ContactsModal {
            matches: book.filtered(""),
            view_height: 8,
            ..Default::default()
        };
        modal.visual_anchor = Some(2);
        modal.selected = 2;
        modal.navigate(40, 5);
        assert_eq!(modal.visual_range(), Some((2, 7)));
        assert_eq!(modal.action_ids(&book).len(), 6);
        modal.smooth(40, 22);
        for _ in 0..22 {
            modal.advance(40);
        }
        assert_eq!(modal.selected, 29);
        assert_eq!(modal.visual_range(), Some((2, 29)));
        modal.mark_visual_range(&book);
        assert_eq!(modal.marked.len(), 28);
        assert_eq!(modal.visual_range(), None);
        assert_eq!(modal.action_ids(&book).len(), 28);
    }

    #[test]
    fn batch_recipient_insert_preserves_quoted_comma_and_other_addresses() {
        let a = Contact {
            name: "Doe, Jane".into(),
            email: "jane@example.org".into(),
            saved: true,
            favorite: false,
        };
        let b = Contact {
            name: "Béa".into(),
            email: "bea@example.org".into(),
            saved: true,
            favorite: true,
        };
        let mut to = "existing@example.org, j, final@example.org".to_owned();
        let cursor = "existing@example.org, j".chars().count();
        let result = replace_recipients(&mut to, cursor, &[&a, &b]);
        assert_eq!(
            to,
            "existing@example.org, \"Doe, Jane\" <jane@example.org>, Béa <bea@example.org>, final@example.org"
        );
        assert_eq!(
            result,
            "existing@example.org, \"Doe, Jane\" <jane@example.org>, Béa <bea@example.org>"
                .chars()
                .count()
        );
    }
    #[test]
    fn learned_contacts_do_not_overwrite_saved_names() {
        let mut book = ContactBook::default();
        book.upsert("Original", "hi@example.org", true).unwrap();
        assert!(!book.upsert("Other", "HI@EXAMPLE.org", false).unwrap());
        assert_eq!(book.entries[0].name, "Original");
    }
    #[test]
    fn save_and_load_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("contacts.json");
        let mut book = ContactBook::default();
        book.upsert("Jane", "jane@example.org", true).unwrap();
        book.save(&path).unwrap();
        assert_eq!(ContactBook::load(&path).unwrap().entries, book.entries);
    }
    #[test]
    fn malformed_json_is_not_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("contacts.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(ContactBook::load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }
    #[test]
    fn completing_recipient_preserves_quoted_names() {
        let contact = Contact {
            name: "Doe, Jane".into(),
            email: "jane@example.org".into(),
            saved: true,
            favorite: false,
        };
        assert_eq!(contact.display(), "\"Doe, Jane\" <jane@example.org>");
        let mut text = "ja".to_owned();
        let end = replace_recipient(&mut text, 2, &contact);
        assert_eq!(text, "\"Doe, Jane\" <jane@example.org>, ");
        assert_eq!(end, text.chars().count());
    }
    #[test]
    fn fuzzy_and_favorites() {
        let mut book = ContactBook::default();
        book.upsert("Jane Doe", "jane@example.org", true).unwrap();
        book.upsert("John Smith", "john@example.org", false)
            .unwrap();
        assert_eq!(book.suggest("jdo", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("ja", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("j", &HashSet::new()), vec![0]);
        assert!(book.suggest("s", &HashSet::new()).is_empty());
    }
    #[test]
    fn short_autocomplete_does_not_match_characters_in_email_domains() {
        let mut book = ContactBook::default();
        book.upsert("Jane Doe", "jane@example.org", true).unwrap();
        book.upsert("John Smith", "john@example.org", false)
            .unwrap();
        assert_eq!(book.suggest("ja", &HashSet::new()), vec![0]);
        // Longer input still benefits from fuzzy matching (initials in a name).
        assert_eq!(book.suggest("jdo", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("jo", &HashSet::new()), vec![1]);
    }
    #[test]
    fn short_autocomplete_matches_a_name_word_or_email_local_part() {
        let mut book = ContactBook::default();
        book.upsert("Doe, Jane", "person@example.org", true)
            .unwrap();
        book.upsert("Other", "jane.doe@example.org", false).unwrap();
        assert_eq!(book.suggest("ja", &HashSet::new()), vec![1, 0]);
        let excluded = HashSet::from(["person@example.org".to_owned()]);
        assert_eq!(book.suggest("ja", &excluded), vec![1]);
    }
    #[test]
    fn manually_saved_favorite_is_suggested_even_for_own_email() {
        let mut book = ContactBook::default();
        book.upsert("Personal Account", "me@example.org", true)
            .unwrap();
        book.entries[0].favorite = true;
        // Account identity is not a disqualifier. Sending a test message to
        // yourself is a useful and valid composition workflow.
        assert_eq!(book.suggest("p", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("personal", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("me@", &HashSet::new()), vec![0]);
    }
    #[test]
    fn single_letter_suggestions_do_not_expose_learned_only_contacts() {
        let mut book = ContactBook::default();
        book.upsert("Jason Saved", "saved@example.org", true)
            .unwrap();
        book.upsert("Jared Learned", "learned@example.org", false)
            .unwrap();
        assert_eq!(book.suggest("j", &HashSet::new()), vec![0]);
        assert_eq!(book.suggest("ja", &HashSet::new()), vec![0, 1]);
        let excluded = HashSet::from(["saved@example.org".to_owned()]);
        assert!(book.suggest("j", &excluded).is_empty());
    }
    #[test]
    fn smooth_scroll_honors_boundaries() {
        let mut modal = ContactsModal {
            view_height: 10,
            ..Default::default()
        };
        modal.smooth(200, 25);
        for _ in 0..25 {
            modal.advance(200);
        }
        assert_eq!(modal.selected, 25);
        assert_eq!(modal.scroll, 16);
        assert!(modal.smooth_target.is_none());
    }
}
