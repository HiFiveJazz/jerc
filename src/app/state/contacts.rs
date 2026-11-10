//! Contacts behavior.

use super::*;

impl App {
    /// On-disk writes are delayed to batch incoming pages; no I/O in drawing.
    pub(super) fn mark_contacts_dirty(&mut self) {
        if !self.contacts_writable {
            return;
        }
        self.contacts_dirty = true;
        if self.contacts_save_due.is_none() {
            self.contacts_save_due = Some(Instant::now() + Duration::from_secs(3));
        }
    }

    pub fn contact_save_deadline(&self) -> Option<Instant> {
        self.contacts_save_due
    }

    pub fn flush_contacts(&mut self, force: bool) {
        if !self.contacts_dirty || !self.contacts_writable {
            return;
        }
        if !force
            && self
                .contacts_save_due
                .is_some_and(|due| Instant::now() < due)
        {
            return;
        }
        match self.contacts.save(&self.contacts_path) {
            Ok(()) => {
                self.contacts_dirty = false;
                self.contacts_save_due = None;
            }
            Err(error) => {
                self.contacts_save_due = Some(Instant::now() + Duration::from_secs(10));
                self.status = StatusLine::error(format!("contacts not saved: {error:#}"));
            }
        }
    }

    pub fn contact_suggestions(&self) -> Vec<usize> {
        if !self.contacts_autocomplete {
            return Vec::new();
        }
        let Some(composer) = &self.composer else {
            return Vec::new();
        };
        if composer.mode != ComposeMode::Insert
            || !matches!(
                composer.field,
                ComposeField::To | ComposeField::Cc | ComposeField::Bcc
            )
            || composer.help_visible
            || composer.confirm_send
            || composer.sending
            || composer.exit_prompt
            || composer.file_picker.is_some()
            || composer.attachment_manager_open
            || composer.advanced_headers_open
            || self.contacts_modal.is_some()
        {
            return Vec::new();
        }
        let query = recipient_query(composer.field_value(), composer.cursor);
        if composer.dismissed_contact_query.as_deref() == Some(query.as_str()) {
            return Vec::new();
        }
        let mut excluded = HashSet::new();
        for field in [&composer.to, &composer.cc, &composer.bcc] {
            for (_, address) in recipients(field) {
                excluded.insert(address.to_ascii_lowercase());
            }
        }
        // Do not exclude our own account identities: composing a message to
        // oneself is valid, and often how users test a newly saved contact.
        // Already-completed recipients are still filtered above.
        self.contacts.suggest(&query, &excluded)
    }

    pub(super) fn accept_contact_suggestion(&mut self, id: usize) {
        let Some(contact) = self.contacts.entries.get(id) else {
            return;
        };
        let Some(composer) = &mut self.composer else {
            return;
        };
        composer.remember_edit();
        let cursor = composer.cursor;
        composer.cursor = replace_recipient(composer.field_mut(), cursor, contact);
        composer.contact_suggestion = 0;
        composer.dismissed_contact_query =
            Some(recipient_query(composer.field_value(), composer.cursor));
    }

    /// The contacts manager can insert a whole visual/marked selection in a
    /// single undoable composer edit. The active unfinished recipient is the
    /// only segment replaced; other To/Cc/Bcc entries remain unchanged.
    pub(super) fn accept_contacts_from_manager(&mut self, ids: &[usize]) {
        let Some(composer) = &mut self.composer else {
            return;
        };
        if !matches!(
            composer.field,
            ComposeField::To | ComposeField::Cc | ComposeField::Bcc
        ) {
            return;
        }
        let contacts: Vec<_> = ids
            .iter()
            .filter_map(|&id| self.contacts.entries.get(id))
            .collect();
        if contacts.is_empty() {
            return;
        }
        composer.remember_edit();
        let cursor = composer.cursor;
        composer.cursor = replace_recipients(composer.field_mut(), cursor, &contacts);
        composer.contact_suggestion = 0;
        composer.dismissed_contact_query =
            Some(recipient_query(composer.field_value(), composer.cursor));
    }

    pub(super) fn open_contacts(&mut self) {
        self.contacts_modal = Some(ContactsModal {
            view_height: 15,
            matches: self.contacts.filtered(""),
            ..ContactsModal::default()
        });
    }

    pub(super) fn handle_contacts_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.contacts_modal.take() else {
            return;
        };
        if self.handle_contacts_modal_key(&mut modal, key) {
            self.contacts_modal = Some(modal);
        }
    }

    /// Returns false to close the manager. Removing the modal from App while
    /// handling it permits edits without nested mutable borrows.
    pub(super) fn handle_contacts_modal_key(
        &mut self,
        modal: &mut ContactsModal,
        key: KeyEvent,
    ) -> bool {
        if modal.help {
            if crate::app::help_palette::handle_help_key(
                key,
                CONTACT_HELP_ENTRIES,
                &mut modal.help_query,
                &mut modal.help_selected,
                &mut modal.help_scroll,
                crate::app::help_palette::HelpKeys::MODAL,
            ) {
                modal.help = false;
                modal.help_query.clear();
            }
            return true;
        }
        if modal.edit.is_some()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('s')
        {
            self.save_contact_editor(modal);
            return true;
        }
        if let Some(editor) = modal.edit.as_mut() {
            use crate::app::ComposeMode;
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('w') => editor.clear_line(),
                    KeyCode::Char('r') => editor.redo(),
                    _ => {}
                }
                return true;
            }
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                editor.toggle();
                return true;
            }
            if key.code == KeyCode::Esc {
                if editor.mode != ComposeMode::Normal {
                    editor.mode = ComposeMode::Normal;
                    editor.visual_anchor = None;
                    editor.vim_sequence.clear();
                } else {
                    modal.edit = None;
                }
                return true;
            }
            if key.code == KeyCode::Enter {
                if editor.active_email {
                    self.save_contact_editor(modal);
                } else {
                    editor.toggle();
                }
                return true;
            }
            if editor.mode == ComposeMode::Insert {
                match key.code {
                    KeyCode::Left => {
                        editor.cursor = crate::app::vim::horizontal(
                            editor.cursor,
                            editor.value().chars().count(),
                            -1,
                        )
                    }
                    KeyCode::Right => {
                        editor.cursor = crate::app::vim::horizontal(
                            editor.cursor,
                            editor.value().chars().count(),
                            1,
                        )
                    }
                    KeyCode::Home => editor.cursor = 0,
                    KeyCode::End => editor.cursor = editor.value().chars().count(),
                    KeyCode::Backspace => editor.backspace(),
                    KeyCode::Delete => editor.delete_range(editor.cursor, editor.cursor + 1),
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                    {
                        editor.insert(ch)
                    }
                    _ => {}
                }
                return true;
            }
            if let Some(prefix) = editor.vim_sequence.take_command() {
                match (prefix, key.code) {
                    (crate::app::vim::VimPrefix::Yank, KeyCode::Char('y')) => {
                        self.yank_register = Some(YankRegister {
                            text: editor.value().to_owned(),
                            linewise: false,
                        });
                    }
                    (crate::app::vim::VimPrefix::Delete, KeyCode::Char('d')) => {
                        editor.delete_range(0, editor.value().chars().count());
                    }
                    _ => {}
                }
                return true;
            }
            if editor.mode == ComposeMode::Visual {
                match key.code {
                    KeyCode::Char('y') => {
                        if let Some(text) = editor.selected_text() {
                            self.yank_register = Some(YankRegister {
                                text,
                                linewise: false,
                            });
                        }
                        editor.mode = ComposeMode::Normal;
                        editor.visual_anchor = None;
                    }
                    KeyCode::Char('d' | 'x') => {
                        if let Some((start, end)) = editor.selected_range() {
                            let cut = (key.code == KeyCode::Char('x'))
                                .then(|| crate::app::vim::text_range(editor.value(), start, end))
                                .flatten()
                                .map(str::to_owned);
                            editor.delete_range(start, end);
                            if let Some(cut) = cut
                                && let Some(status) =
                                    record_cut(&mut self.yank_register, cut, false)
                            {
                                self.status = status;
                            }
                        }
                        editor.mode = ComposeMode::Normal;
                        editor.visual_anchor = None;
                    }
                    KeyCode::Char('p' | 'P') => {
                        if let Some(register) = self.yank_register.as_ref()
                            && let Some((start, end)) = editor.selected_range()
                        {
                            let text = register.text.replace(['\n', '\r'], " ");
                            editor.delete_range(start, end);
                            editor.insert_text(&text);
                        }
                        editor.mode = ComposeMode::Normal;
                        editor.visual_anchor = None;
                    }
                    _ => {}
                }
                if matches!(key.code, KeyCode::Char('y' | 'd' | 'x' | 'p' | 'P')) {
                    return true;
                }
            }
            match key.code {
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                    editor.cursor = crate::app::vim::horizontal(
                        editor.cursor,
                        editor.value().chars().count(),
                        -1,
                    );
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    editor.cursor = crate::app::vim::horizontal(
                        editor.cursor,
                        editor.value().chars().count(),
                        1,
                    )
                }
                KeyCode::Char('w') => editor.move_word(true),
                KeyCode::Char('e') => {
                    editor.cursor = crate::app::vim::word_end(editor.value(), editor.cursor, true);
                }
                KeyCode::Char('b') => editor.move_word(false),
                KeyCode::Char('0' | 'H') | KeyCode::Home => editor.cursor = 0,
                KeyCode::Char('$' | 'L') | KeyCode::End => {
                    editor.cursor = editor.value().chars().count()
                }
                KeyCode::Char('i') if editor.mode == ComposeMode::Normal => {
                    editor.mode = ComposeMode::Insert
                }
                KeyCode::Char('a') if editor.mode == ComposeMode::Normal => {
                    editor.cursor = (editor.cursor + 1).min(editor.value().chars().count());
                    editor.mode = ComposeMode::Insert;
                }
                KeyCode::Char('I') if editor.mode == ComposeMode::Normal => {
                    editor.cursor = 0;
                    editor.mode = ComposeMode::Insert;
                }
                KeyCode::Char('A') if editor.mode == ComposeMode::Normal => {
                    editor.cursor = editor.value().chars().count();
                    editor.mode = ComposeMode::Insert;
                }
                KeyCode::Char('v' | 'V') if editor.mode == ComposeMode::Normal => {
                    let last = editor.value().chars().count();
                    if key.code == KeyCode::Char('V') {
                        editor.visual_anchor = Some(0);
                        editor.cursor = last.saturating_sub(1);
                    } else {
                        let anchor = editor.cursor.min(last.saturating_sub(1));
                        editor.visual_anchor = Some(anchor);
                        editor.cursor = anchor;
                    }
                    editor.mode = ComposeMode::Visual;
                }
                KeyCode::Char('u') if editor.mode == ComposeMode::Normal => editor.undo(),
                KeyCode::Char('x') if editor.mode == ComposeMode::Normal => {
                    let start = editor.cursor;
                    let cut = crate::app::vim::text_range(editor.value(), start, start + 1)
                        .map(str::to_owned);
                    if let Some(cut) = cut {
                        editor.delete_range(start, start + 1);
                        if let Some(status) = record_cut(&mut self.yank_register, cut, false) {
                            self.status = status;
                        }
                    }
                }
                KeyCode::Char('D') if editor.mode == ComposeMode::Normal => {
                    editor.delete_range(editor.cursor, editor.value().chars().count());
                }
                KeyCode::Char('d') if editor.mode == ComposeMode::Normal => editor
                    .vim_sequence
                    .start(crate::app::vim::VimPrefix::Delete),
                KeyCode::Char('y') if editor.mode == ComposeMode::Normal => {
                    editor.vim_sequence.start(crate::app::vim::VimPrefix::Yank)
                }
                KeyCode::Char('Y') if editor.mode == ComposeMode::Normal => {
                    self.yank_register = Some(YankRegister {
                        text: editor.value().to_owned(),
                        linewise: false,
                    });
                }
                KeyCode::Char('p' | 'P') if editor.mode == ComposeMode::Normal => {
                    if let Some(register) = self.yank_register.as_ref() {
                        let text = register.text.replace(['\n', '\r'], " ");
                        if key.code == KeyCode::Char('p') {
                            editor.cursor = (editor.cursor + 1).min(editor.value().chars().count());
                        }
                        editor.insert_text(&text);
                    }
                }
                _ => {}
            }
            return true;
        }
        if modal.confirm_delete {
            let count = modal.action_ids(&self.contacts).len();
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('j') => modal.delete_review.queue(count, SMOOTH_SCROLL_ROWS),
                    KeyCode::Char('k') => modal.delete_review.queue(count, -SMOOTH_SCROLL_ROWS),
                    _ => {}
                }
                return true;
            }
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => modal.delete_review.queue(count, 1),
                KeyCode::Char('k') | KeyCode::Up => modal.delete_review.queue(count, -1),
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    if !self.contacts_writable {
                        self.status = StatusLine::error(
                            "contacts are read-only: fix the contacts.json load error",
                        );
                    } else {
                        let mut ids = modal.action_ids(&self.contacts);
                        // Remove highest indices first so earlier indices stay valid.
                        ids.sort_unstable_by(|a, b| b.cmp(a));
                        for id in &ids {
                            self.contacts.entries.remove(*id);
                        }
                        if !ids.is_empty() {
                            self.mark_contacts_dirty();
                        }
                        modal.clear_selection();
                        modal.matches = self.contacts.filtered(&modal.query);
                        modal.ensure_visible(modal.matches.len());
                        self.status = StatusLine::temporary(
                            format!("deleted {} contact(s)", ids.len()),
                            Duration::from_secs(2),
                        );
                    }
                    modal.confirm_delete = false;
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') | KeyCode::Char('q') => {
                    modal.confirm_delete = false
                }
                _ => {}
            }
            return true;
        }
        if modal.searching {
            // Keep the filtered list navigable without leaving the search
            // input. This mirrors Ctrl-j/k in the normal contacts view.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('j' | 'J') | KeyCode::Enter => {
                        modal.smooth(modal.matches.len(), SMOOTH_SCROLL_ROWS)
                    }
                    KeyCode::Char('k' | 'K') => {
                        modal.smooth(modal.matches.len(), -SMOOTH_SCROLL_ROWS)
                    }
                    _ => {}
                }
                return true;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Enter => modal.searching = false,
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    modal.query.clear();
                    modal.selected = 0;
                    modal.scroll = 0;
                }
                KeyCode::Backspace => {
                    modal.query.pop();
                    modal.selected = 0;
                    modal.scroll = 0;
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    modal.query.push(ch);
                    modal.selected = 0;
                    modal.scroll = 0;
                }
                _ => {}
            }
            modal.smooth_target = None;
            modal.matches = self.contacts.filtered(&modal.query);
            modal.visual_anchor = None;
            modal.ensure_visible(modal.matches.len());
            return true;
        }
        let len = modal.matches.len();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Some enhanced keyboard protocols report Ctrl-J as
                // Ctrl+Enter instead of Ctrl+Char('j').
                KeyCode::Char('j' | 'J') | KeyCode::Enter => modal.smooth(len, SMOOTH_SCROLL_ROWS),
                KeyCode::Char('k' | 'K') => modal.smooth(len, -SMOOTH_SCROLL_ROWS),
                _ => {}
            }
            return true;
        }
        let mut preserve_count = false;
        match key.code {
            KeyCode::Esc if modal.visual_anchor.is_some() || !modal.marked.is_empty() => {
                modal.clear_selection();
            }
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Char('v' | 'V') if len > 0 => {
                let linewise = key.code == KeyCode::Char('V');
                if modal.visual_anchor.is_some() && modal.visual_linewise == linewise {
                    modal.visual_anchor = None;
                } else {
                    if modal.visual_anchor.is_none() {
                        modal.visual_anchor = Some(modal.selected);
                    }
                    modal.visual_linewise = linewise;
                }
            }
            KeyCode::Char(' ') => {
                if modal.visual_anchor.is_some() {
                    modal.mark_visual_range(&self.contacts);
                } else {
                    modal.toggle_mark(&self.contacts);
                }
            }
            KeyCode::Char('?') => {
                modal.help = true;
                modal.help_query.clear();
                modal.help_selected = 0;
                modal.help_scroll = 0;
            }
            KeyCode::Char('/') => {
                modal.searching = true;
                modal.visual_anchor = None;
            }
            KeyCode::Char('a') => modal.edit = Some(ContactEdit::new(None)),
            KeyCode::Char('e') => {
                modal.edit = modal
                    .matches
                    .get(modal.selected)
                    .map(|&i| ContactEdit::new(Some(&self.contacts.entries[i])));
            }
            KeyCode::Enter => {
                let ids = modal.action_ids(&self.contacts);
                if self.composer.as_ref().is_some_and(|c| {
                    matches!(
                        c.field,
                        ComposeField::To | ComposeField::Cc | ComposeField::Bcc
                    )
                }) {
                    if !ids.is_empty() {
                        self.accept_contacts_from_manager(&ids);
                        return false;
                    }
                } else if ids.len() == 1 {
                    modal.edit = ids
                        .first()
                        .map(|&id| ContactEdit::new(Some(&self.contacts.entries[id])));
                } else if ids.len() > 1 {
                    self.status = StatusLine::temporary(
                        "Open compose To/Cc/Bcc to insert multiple contacts",
                        Duration::from_secs(3),
                    );
                }
            }
            KeyCode::Char('p') => {
                if !self.contacts_writable {
                    self.status = StatusLine::error(
                        "contacts are read-only: fix the contacts.json load error",
                    );
                } else {
                    let ids = modal.action_ids(&self.contacts);
                    if !ids.is_empty() {
                        let focused = modal.matches.get(modal.selected).copied();
                        let all_favorite = ids.iter().all(|&id| self.contacts.entries[id].favorite);
                        for &id in &ids {
                            self.contacts.entries[id].favorite = !all_favorite;
                        }
                        self.mark_contacts_dirty();
                        modal.clear_selection();
                        modal.matches = self.contacts.filtered(&modal.query);
                        if let Some(position) = focused
                            .and_then(|id| modal.matches.iter().position(|&row_id| row_id == id))
                        {
                            modal.selected = position;
                        }
                        modal.ensure_visible(modal.matches.len());
                    }
                }
            }
            KeyCode::Char('d') => {
                if !modal.action_ids(&self.contacts).is_empty() {
                    modal.confirm_delete = true;
                    modal.smooth_target = None;
                    modal.delete_review = ReviewScroll::default();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                modal.navigate(len, modal.pending_count.unwrap_or(1).min(9999) as isize);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                modal.navigate(len, -(modal.pending_count.unwrap_or(1).min(9999) as isize));
            }
            KeyCode::PageDown => modal.navigate(len, modal.view_height.max(1) as isize),
            KeyCode::PageUp => modal.navigate(len, -(modal.view_height.max(1) as isize)),
            KeyCode::Char('H') => {
                modal.selected = modal.scroll;
                modal.smooth_target = None;
            }
            KeyCode::Char('M') => {
                modal.selected = (modal.scroll + modal.view_height / 2).min(len.saturating_sub(1));
                modal.smooth_target = None;
            }
            KeyCode::Char('L') => {
                modal.selected =
                    (modal.scroll + modal.view_height.saturating_sub(1)).min(len.saturating_sub(1));
                modal.smooth_target = None;
            }
            KeyCode::Char('G') | KeyCode::End => {
                modal.selected = modal
                    .pending_count
                    .map(|n| n.saturating_sub(1))
                    .unwrap_or(len.saturating_sub(1))
                    .min(len.saturating_sub(1));
                modal.ensure_visible(len);
                modal.smooth_target = None;
            }
            KeyCode::Char('g') => {
                if modal.pending_g {
                    modal.selected = modal
                        .pending_count
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(0)
                        .min(len.saturating_sub(1));
                    modal.ensure_visible(len);
                    modal.smooth_target = None;
                } else {
                    modal.pending_g = true;
                    preserve_count = true;
                }
            }
            KeyCode::Char('0') if modal.pending_count.is_none() => {
                modal.selected = 0;
                modal.scroll = 0;
                modal.smooth_target = None;
            }
            KeyCode::Char(d @ '0'..='9') => {
                modal.pending_count = Some(
                    modal
                        .pending_count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(d.to_digit(10).unwrap_or(0) as usize)
                        .min(9999),
                );
                preserve_count = true;
            }
            _ => {}
        }
        if !preserve_count {
            modal.pending_count = None;
            modal.pending_g = false;
        }
        true
    }

    pub(super) fn save_contact_editor(&mut self, modal: &mut ContactsModal) {
        let Some(editor) = modal.edit.as_ref().cloned() else {
            return;
        };
        if !self.contacts_writable {
            self.status =
                StatusLine::error("contacts are read-only: fix the contacts.json load error");
            return;
        }
        match self.contacts.edit(
            editor.original_email.as_deref(),
            &editor.name,
            &editor.email,
        ) {
            Ok(()) => {
                self.mark_contacts_dirty();
                self.flush_contacts(true);
                modal.edit = None;
                modal.matches = self.contacts.filtered(&modal.query);
                if let Some(old) = editor.original_email.as_deref()
                    && modal.marked.remove(&old.to_ascii_lowercase())
                {
                    modal.marked.insert(editor.email.to_ascii_lowercase());
                }
                modal.visual_anchor = None;
                if let Some(id) = self.contacts.find(&editor.email)
                    && let Some(position) = modal.matches.iter().position(|i| *i == id)
                {
                    modal.selected = position;
                }
                modal.ensure_visible(modal.matches.len());
                self.status = StatusLine::temporary("contact saved", Duration::from_secs(2));
            }
            Err(error) => self.status = StatusLine::error(format!("contact: {error:#}")),
        }
    }
}
