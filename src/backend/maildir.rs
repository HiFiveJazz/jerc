use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use crate::backend::MailBackend;
use crate::mail::{parse_body, parse_summary, read_all, read_header_block};
use crate::model::{Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessagePage, MessageSummary};

#[derive(Debug)]
pub struct MaildirBackend {
    root: PathBuf,
    selected_mailbox: Option<String>,
    selected_index: HashMap<MessageId, PathBuf>,
}

impl MaildirBackend {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            selected_mailbox: None,
            selected_index: HashMap::new(),
        }
    }

    fn inbox_path(&self) -> &Path {
        &self.root
    }

    fn mailbox_path(&self, name: &str) -> Result<PathBuf> {
        if name.eq_ignore_ascii_case("INBOX") {
            return Ok(self.inbox_path().to_path_buf());
        }

        let direct = self.root.join(name);
        if is_maildir(&direct) {
            return Ok(direct);
        }

        // Maildir++: Foo/Bar maps to .Foo.Bar
        let encoded = format!(".{}", name.replace('/', "."));
        let maildirpp = self.root.join(encoded);
        if is_maildir(&maildirpp) {
            return Ok(maildirpp);
        }

        Err(anyhow!("unknown Maildir mailbox {name:?}"))
    }

    fn discover_mailboxes(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        if is_maildir(&self.root) {
            out.push(("INBOX".to_owned(), self.root.clone()));
        }

        let entries = fs::read_dir(&self.root)
            .with_context(|| format!("reading Maildir root {}", self.root.display()))?;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if !is_maildir(&path) {
                continue;
            }
            let raw = entry.file_name().to_string_lossy().into_owned();
            let name = if let Some(stripped) = raw.strip_prefix('.') {
                stripped.replace('.', "/")
            } else {
                raw
            };
            if !name.is_empty() && name != "cur" && name != "new" && name != "tmp" {
                out.push((name, path));
            }
        }

        out.sort_by(|a, b| mailbox_sort_key(&a.0).cmp(&mailbox_sort_key(&b.0)));
        Ok(out)
    }

    fn scan_mailbox(&mut self, name: &str) -> Result<Vec<MessageSummary>> {
        let path = self.mailbox_path(name)?;
        let mut summaries = Vec::new();
        let mut index = HashMap::new();

        for subdir in ["new", "cur"] {
            let dir = path.join(subdir);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let file_path = entry.path();
                let metadata = entry.metadata()?;
                let id = message_id_for(name, subdir, &entry.file_name().to_string_lossy());
                let flags = if subdir == "new" {
                    MessageFlags::empty()
                } else {
                    flags_from_filename(&entry.file_name().to_string_lossy())
                };

                let file = File::open(&file_path)?;
                let headers = match read_header_block(file) {
                    Ok(headers) => headers,
                    Err(_) => continue,
                };
                let summary = match parse_summary(id.clone(), &headers, metadata.len(), flags) {
                    Ok(summary) => summary,
                    Err(_) => continue,
                };
                index.insert(id, file_path);
                summaries.push(summary);
            }
        }

        summaries.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then_with(|| b.id.cmp(&a.id))
        });
        self.selected_mailbox = Some(name.to_owned());
        self.selected_index = index;
        Ok(summaries)
    }

    fn locate(&mut self, mailbox: &str, id: &MessageId) -> Result<PathBuf> {
        if self.selected_mailbox.as_deref() != Some(mailbox) || !self.selected_index.contains_key(id) {
            let _ = self.scan_mailbox(mailbox)?;
        }
        self.selected_index
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("message {id} no longer exists in {mailbox}"))
    }

    fn mailbox_counts(path: &Path) -> Result<(usize, usize)> {
        let mut total = 0usize;
        let mut unread = 0usize;
        for subdir in ["new", "cur"] {
            let dir = path.join(subdir);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                total += 1;
                if subdir == "new"
                    || !flags_from_filename(&entry.file_name().to_string_lossy())
                        .contains(MessageFlags::SEEN)
                {
                    unread += 1;
                }
            }
        }
        Ok((total, unread))
    }

    fn refresh_selected_path(&mut self, id: &MessageId, path: PathBuf) {
        self.selected_index.insert(id.clone(), path);
    }
}

impl MailBackend for MaildirBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::MAILDIR
    }

    fn connect(&mut self) -> Result<()> {
        if !self.root.exists() {
            bail!("Maildir root does not exist: {}", self.root.display());
        }
        if !is_maildir(&self.root) {
            bail!(
                "{} is not a Maildir (expected cur/, new/, tmp/)",
                self.root.display()
            );
        }
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        self.discover_mailboxes()?
            .into_iter()
            .map(|(name, path)| {
                let (total, unread) = Self::mailbox_counts(&path)?;
                Ok(Mailbox {
                    role: role_for_name(&name),
                    name,
                    total,
                    unread,
                    counts_known: true,
                })
            })
            .collect()
    }

    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)> {
        let path = self.mailbox_path(name)?;
        Self::mailbox_counts(&path)
    }

    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage> {
        let messages = self.scan_mailbox(name)?;
        let start = offset.min(messages.len());
        let end = if limit == 0 {
            messages.len()
        } else {
            start.saturating_add(limit).min(messages.len())
        };
        let page = messages[start..end].to_vec();
        Ok(MessagePage {
            messages: page,
            offset,
            next_offset: end,
            has_more: end < messages.len(),
        })
    }

    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let raw = self.fetch_raw(mailbox, id)?;
        parse_body(id.clone(), &raw).context("parsing Maildir message body")
    }

    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        let path = self.locate(mailbox, id)?;
        read_all(File::open(&path)?).with_context(|| format!("reading message from {}", path.display()))
    }

    fn append_message(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        let maildir = self.mailbox_path(mailbox)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let unique = format!(
            "{}.M{}P{}.aerc-rs",
            now.as_secs(),
            now.subsec_nanos(),
            std::process::id()
        );
        let tmp = maildir.join("tmp").join(&unique);
        fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        let filename = if seen {
            format!("{unique}{}2,S", maildir_info_separator())
        } else {
            unique
        };
        let target_dir = if seen { "cur" } else { "new" };
        let target = maildir.join(target_dir).join(filename);
        if let Err(error) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(error)
                .with_context(|| format!("filing sent message in {}", target.display()));
        }
        Ok(())
    }

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        let old = self.locate(mailbox, id)?;
        let maildir = self.mailbox_path(mailbox)?;
        let filename = old
            .file_name()
            .ok_or_else(|| anyhow!("message path has no filename"))?
            .to_string_lossy();
        let (base, _) = split_maildir_info(&filename);
        let mut flags = flags_from_filename(&filename);
        flags.set(MessageFlags::SEEN, seen);

        let was_new = old
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|name| name == "new");
        let target_dir = if was_new && !seen { "new" } else { "cur" };
        let new_name = if target_dir == "new" {
            base.to_owned()
        } else {
            format!("{base}{}2,{}", maildir_info_separator(), flags.to_maildir_suffix())
        };
        let new_path = maildir.join(target_dir).join(new_name);
        move_file(&old, &new_path)?;
        self.refresh_selected_path(id, new_path);
        Ok(())
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        let path = self.locate(mailbox, id)?;
        fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
        self.selected_index.remove(id);
        Ok(())
    }

    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()> {
        let old = self.locate(source, id)?;
        let destination_root = self.mailbox_path(destination)?;
        let filename = old
            .file_name()
            .ok_or_else(|| anyhow!("message path has no filename"))?;
        let target_subdir = if old
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|name| name == "new")
        {
            "new"
        } else {
            "cur"
        };
        let target = destination_root.join(target_subdir).join(filename);
        move_file(&old, &target)?;
        self.selected_index.remove(id);
        Ok(())
    }
}

fn is_maildir(path: &Path) -> bool {
    path.join("cur").is_dir() && path.join("new").is_dir() && path.join("tmp").is_dir()
}

fn message_id_for(mailbox: &str, subdir: &str, filename: &str) -> MessageId {
    MessageId(format!("{mailbox}\0{subdir}\0{filename}"))
}

fn maildir_info_separator() -> char {
    if cfg!(windows) { '!' } else { ':' }
}

fn split_maildir_info(filename: &str) -> (&str, Option<&str>) {
    for separator in [":2,", "!2,"] {
        if let Some((base, suffix)) = filename.split_once(separator) {
            return (base, Some(suffix));
        }
    }
    (filename, None)
}

fn flags_from_filename(filename: &str) -> MessageFlags {
    split_maildir_info(filename).1.map_or_else(
        MessageFlags::empty,
        MessageFlags::from_maildir_suffix,
    )
}

fn role_for_name(name: &str) -> MailboxRole {
    let lower = name.to_ascii_lowercase();
    if lower == "inbox" {
        MailboxRole::Inbox
    } else if lower.contains("sent") {
        MailboxRole::Sent
    } else if lower.contains("draft") {
        MailboxRole::Drafts
    } else if lower.contains("trash") || lower.contains("deleted") {
        MailboxRole::Trash
    } else if lower.contains("archive") || lower.contains("all mail") {
        MailboxRole::Archive
    } else if lower.contains("junk") || lower.contains("spam") {
        MailboxRole::Junk
    } else {
        MailboxRole::Other
    }
}

fn mailbox_sort_key(name: &str) -> (u8, String) {
    let role = role_for_name(name);
    let weight = match role {
        MailboxRole::Inbox => 0,
        MailboxRole::Sent => 1,
        MailboxRole::Drafts => 2,
        MailboxRole::Archive => 3,
        MailboxRole::Trash => 4,
        MailboxRole::Junk => 5,
        MailboxRole::Other => 6,
    };
    (weight, name.to_ascii_lowercase())
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            fs::copy(source, destination)?;
            fs::remove_file(source)?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{flags_from_filename, role_for_name};
    use crate::model::{MailboxRole, MessageFlags};

    #[test]
    fn detects_maildir_flags() {
        for filename in ["abc:2,RS", "abc!2,RS"] {
            let flags = flags_from_filename(filename);
            assert!(flags.contains(MessageFlags::SEEN));
            assert!(flags.contains(MessageFlags::ANSWERED));
        }
    }

    #[test]
    fn guesses_common_roles() {
        assert_eq!(role_for_name("INBOX"), MailboxRole::Inbox);
        assert_eq!(role_for_name("Sent Items"), MailboxRole::Sent);
        assert_eq!(role_for_name("Archive/2026"), MailboxRole::Archive);
    }
}
