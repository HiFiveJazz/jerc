//! File picker behavior.

use super::*;

impl ComposeFilePicker {
    pub(super) fn open(directory: PathBuf) -> anyhow::Result<Self> {
        Self::open_with_mode(directory, false)
    }

    pub(super) fn open_directory_only(directory: PathBuf) -> anyhow::Result<Self> {
        Self::open_with_mode(directory, true)
    }

    pub(super) fn open_with_mode(directory: PathBuf, directory_only: bool) -> anyhow::Result<Self> {
        let mut picker = Self {
            directory,
            entries: Vec::new(),
            selected: 0,
            show_hidden: false,
            directory_only,
            parent_entries: Vec::new(),
            parent_selected: 0,
            preview: ComposeFilePreview::Empty,
            pending_count: 0,
            pending_g: false,
            smooth_target: None,
            visual_anchor: None,
            help_visible: false,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            search_query: None,
            pdf_preview_due: None,
            pdf_preview_inflight: HashSet::new(),
            pdf_preview_results: Arc::new(Mutex::new(VecDeque::new())),
            pdf_preview_cache: VecDeque::new(),
        };
        picker.refresh()?;
        Ok(picker)
    }

    pub(super) fn refresh(&mut self) -> anyhow::Result<()> {
        self.smooth_target = None;
        self.visual_anchor = None;
        self.entries = compose_read_dir(&self.directory, self.show_hidden)?;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.parent_entries.clear();
        self.parent_selected = 0;
        if let Some(parent) = self.directory.parent() {
            self.parent_entries = compose_read_dir(parent, self.show_hidden).unwrap_or_default();
            if let Some(index) = self.parent_entries.iter()
                .position(|entry| entry.path == self.directory) {
                self.parent_selected = index;
            }
        }
        self.refresh_preview();
        Ok(())
    }

    pub(super) fn refresh_preview(&mut self) {
        // Only the *current* PDF may be scheduled. An in-flight conversion from
        // a previous row can finish in the background but cannot replace this preview.
        self.pdf_preview_due = None;
        if self.directory_only {
            // Unlike the compose picker, saving only needs directory contents.
            // Never block navigation on decoding images or reading file bodies.
            self.preview = match self.entries.get(self.selected) {
                None => ComposeFilePreview::Empty,
                Some(entry) if entry.is_directory => match compose_read_dir(&entry.path, self.show_hidden) {
                    Ok(entries) => ComposeFilePreview::Directory(entries),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                },
                Some(entry) => match std::fs::metadata(&entry.path) {
                    Ok(metadata) => ComposeFilePreview::Binary(metadata.len()),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                },
            };
            return;
        }
        self.preview = match self.entries.get(self.selected) {
            None => ComposeFilePreview::Empty,
            Some(entry) if entry.is_directory => match compose_read_dir(&entry.path, self.show_hidden) {
                Ok(entries) => ComposeFilePreview::Directory(entries),
                Err(error) => ComposeFilePreview::Error(error.to_string()),
            },
            Some(entry) if entry.path.extension().and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf")) => {
                if let Some((_, cached)) = self.pdf_preview_cache.iter()
                    .find(|(path, _)| path == &entry.path) {
                    match cached {
                        Ok(bytes) => ComposeFilePreview::Image(bytes.clone()),
                        Err(error) => ComposeFilePreview::Error(error.clone()),
                    }
                } else {
                    if !self.pdf_preview_inflight.contains(&entry.path) {
                        self.pdf_preview_due = Some((entry.path.clone(), Instant::now() + PDF_PREVIEW_DEBOUNCE));
                    }
                    ComposeFilePreview::Loading("Rendering PDF preview…")
                }
            }
            Some(entry) if compose_image_extension(&entry.path) => {
                match std::fs::metadata(&entry.path) {
                    Ok(metadata) if metadata.len() <= 16 * 1024 * 1024 => {
                        match std::fs::read(&entry.path) {
                            Ok(bytes) => ComposeFilePreview::Image(bytes),
                            Err(error) => ComposeFilePreview::Error(error.to_string()),
                        }
                    }
                    Ok(metadata) => ComposeFilePreview::Binary(metadata.len()),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                }
            }
            Some(entry) => match std::fs::File::open(&entry.path) {
                Err(error) => ComposeFilePreview::Error(error.to_string()),
                Ok(mut file) => match file.metadata() {
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                    Ok(metadata) => {
                        // No unbounded reads while traversing large binary attachments.
                        let mut bytes = Vec::new();
                        match std::io::Read::by_ref(&mut file).take(24 * 1024).read_to_end(&mut bytes) {
                            Err(error) => ComposeFilePreview::Error(error.to_string()),
                            Ok(_) => match String::from_utf8(bytes) {
                                Ok(text) if !text.contains('\0') => {
                                    let truncated = metadata.len() > 24 * 1024 || text.lines().count() > 180;
                                    if entry.path.extension().and_then(|ext| ext.to_str())
                                        .is_some_and(|ext| ext.eq_ignore_ascii_case("patch") || ext.eq_ignore_ascii_case("diff")) {
                                        ComposeFilePreview::Patch(crate::patch_preview::highlight_patch(&text, truncated))
                                    } else {
                                        ComposeFilePreview::Text { lines: text.lines().take(180).map(str::to_owned).collect(), truncated }
                                    }
                                }
                                _ => ComposeFilePreview::Binary(metadata.len()),
                            },
                        }
                    }
                },
            },
        };
    }

    /// Poll completed previews without waiting, and launch only the most
    /// recently selected PDF once the selection has been stable for 140 ms.
    /// Yazi similarly preloads previews off the UI thread, avoiding navigation
    /// stalls on external converters.
    pub(super) fn advance_pdf_preview(&mut self) {
        // Drain a completed result batch under one brief lock. The mutex never
        // covers the expensive Poppler process or image decoding.
        let completed = if let Ok(mut results) = self.pdf_preview_results.lock() {
            results.drain(..).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut refresh = false;
        for (path, result) in completed {
            self.pdf_preview_inflight.remove(&path);
            self.pdf_preview_cache.retain(|(cached, _)| cached != &path);
            self.pdf_preview_cache.push_back((path.clone(), result));
            while self.pdf_preview_cache.len() > PDF_PREVIEW_CACHE_SIZE {
                self.pdf_preview_cache.pop_front();
            }
            refresh |= self.smooth_target.is_none()
                && self.entries.get(self.selected).is_some_and(|entry| entry.path == path);
        }
        if refresh { self.refresh_preview(); }

        let ready = self.pdf_preview_due.as_ref()
            .is_some_and(|(_, due)| *due <= Instant::now());
        if !ready || self.pdf_preview_inflight.len() >= PDF_PREVIEW_CONCURRENCY { return; }
        let Some((path, _)) = self.pdf_preview_due.take() else { return; };
        if self.pdf_preview_inflight.contains(&path) { return; }
        let results = Arc::clone(&self.pdf_preview_results);
        let worker_path = path.clone();
        match std::thread::Builder::new().name("aerc-pdf-preview".into()).spawn(move || {
            let result = compose_pdf_thumbnail(&worker_path).map_err(|error| error.to_string());
            if let Ok(mut queue) = results.lock() {
                queue.push_back((worker_path, result));
            }
        }) {
            Ok(_handle) => { self.pdf_preview_inflight.insert(path); }
            Err(error) => { self.preview = ComposeFilePreview::Error(format!("PDF preview worker: {error}")); }
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.smooth_target = None;
        self.selected = self.selected.saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        self.refresh_preview();
    }

    pub(super) fn queue_smooth_scroll(&mut self, delta: isize) {
        // Queue relative to the *visible* cursor so opposite-direction keys
        // reverse immediately instead of inheriting an unfinished target.
        let target = self.selected.saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        self.smooth_target = (target != self.selected).then_some(target);
        // A new scroll invalidates a pending preview debounce; schedule only
        // the final destination once the animation has settled.
        if self.smooth_target.is_some() { self.pdf_preview_due = None; }
    }

    pub(super) fn advance_smooth_scroll(&mut self) {
        let Some(target) = self.smooth_target else { return; };
        if target == self.selected { self.smooth_target = None; return; }
        self.selected = self.selected.saturating_add_signed(if target > self.selected { 1 } else { -1 });
        // Rebuilding a preview for every *intermediate* animation row makes
        // Ctrl-j/k do filesystem I/O and image rasterization for files the user
        // never stopped on. Keep the old preview while moving and refresh once.
        if target == self.selected {
            self.smooth_target = None;
            self.refresh_preview();
        }
    }

    pub fn help_matches(&self) -> Vec<usize> {
        let query = self.help_query.trim().to_lowercase();
        crate::app::PICKER_HELP_ENTRIES.iter().enumerate()
            .filter(|(_, entry)| query.is_empty() || format!("{} {} {}", entry.category, entry.key, entry.description)
                .to_lowercase().contains(&query))
            .map(|(index, _)| index).collect()
    }

    /// Immutable match indices for the renderer's current-search counter.
    pub fn search_matches_for_display(&self) -> Vec<usize> { self.search_matches() }

    pub(super) fn search_matches(&self) -> Vec<usize> {
        let query = self.search_query.as_deref().unwrap_or("").to_lowercase();
        let matches: Vec<usize> = self.entries.iter().enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&query))
            .map(|(index, _)| index).collect();
        // A directory jump is the primary use case. Fall back to files when
        // no folder matches, so slash remains useful for finding attachments.
        let directories: Vec<usize> = matches.iter().copied()
            .filter(|&index| self.entries[index].is_directory).collect();
        if directories.is_empty() { matches } else { directories }
    }

    pub(super) fn update_search_selection(&mut self) {
        self.smooth_target = None;
        if let Some(index) = self.search_matches().first().copied() {
            self.selected = index;
            self.refresh_preview();
        }
    }

    pub(super) fn next_search_match(&mut self, backwards: bool) {
        self.smooth_target = None;
        let matches = self.search_matches();
        if matches.is_empty() { return; }
        let next = matches.iter().position(|&index| index == self.selected)
            .map_or(0, |position| if backwards {
                (position + matches.len() - 1) % matches.len()
            } else { (position + 1) % matches.len() });
        self.selected = matches[next];
        self.refresh_preview();
    }

    pub(super) fn enter_directory(&mut self, directory: PathBuf) -> anyhow::Result<()> {
        let previous = self.directory.clone();
        self.search_query = None;
        self.directory = directory;
        self.selected = 0;
        if let Err(error) = self.refresh() {
            self.directory = previous;
            self.refresh()?;
            return Err(error);
        }
        if self.directory == previous.parent().unwrap_or(Path::new("")) {
            if let Some(index) = self.entries.iter().position(|entry| entry.path == previous) {
                self.selected = index;
                self.refresh_preview();
            }
        }
        Ok(())
    }

}
