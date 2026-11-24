//! Message reader, attachment and image interaction
use super::*;

impl App {
    pub(super) fn export_selected_raw(&mut self) {
        let path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("jerc")
            .join("debug-message.eml");
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some(mailbox) = account.current_mailbox.clone() else {
            return;
        };
        if account.cached_mailbox_unverified() {
            self.status = StatusLine::info("raw MIME export requires a verified server connection");
            return;
        }
        let Some(id) = account.selected_message().map(|message| message.id.clone()) else {
            return;
        };
        match account
            .worker
            .post(ActionKind::ExportRaw { mailbox, id, path })
        {
            Ok(_) => self.status = StatusLine::info("exporting raw message…"),
            Err(_) => {
                self.status = StatusLine::error("mail worker queue is busy; export not started");
            }
        }
    }

    pub(super) fn open_original_html(&mut self) {
        let Some(html) = self
            .account()
            .and_then(AccountState::selected_body)
            .and_then(|body| body.html_source.as_deref())
            .filter(|html| !html.trim().is_empty())
            .map(str::to_owned)
        else {
            self.status = StatusLine::temporary(
                "selected message does not have an HTML original",
                Duration::from_secs(2),
            );
            return;
        };

        let directory = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("jerc")
            .join("browser");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            self.status = StatusLine::error(format!(
                "could not create browser preview directory: {error}"
            ));
            return;
        }
        let path = directory.join("message.html");
        if let Err(error) = std::fs::write(&path, html.as_bytes()) {
            self.status = StatusLine::error(format!("could not write browser preview: {error}"));
            return;
        }

        match open_url(path.to_string_lossy().as_ref()) {
            Ok(()) => {
                self.status = StatusLine::temporary(
                    "opened original HTML email in browser",
                    Duration::from_secs(3),
                );
            }
            Err(error) => {
                self.status =
                    StatusLine::error(format!("could not open original HTML email: {error}"));
            }
        }
    }

    pub(super) fn toggle_html(&mut self) {
        let mode = {
            let Some(account) = self.account_mut() else {
                return;
            };
            if !account
                .selected_body()
                .is_some_and(|body| body.has_html_alternative() && body.has_plain_view())
            {
                None
            } else {
                account.prefer_html = !account.prefer_html;
                Some(account.prefer_html)
            }
        };
        let Some(prefer_html) = mode else {
            self.status = StatusLine::temporary(
                "selected message does not have both rich and plain views",
                Duration::from_secs(2),
            );
            return;
        };
        self.reader_navigation = ReaderNavigation::default();
        self.reader_pending_inner_object = false;
        self.reader_image = None;
        self.reader_image_pending = None;
        self.reader_image_size_hint = None;
        if let Some(account) = self.account_mut() {
            account.preview_scroll = 0;
            account.smooth_preview_target = None;
        }
        self.status = StatusLine::temporary(
            if prefer_html {
                "HTML mail rendering enabled"
            } else {
                "plain-text mail rendering enabled"
            },
            Duration::from_secs(2),
        );
    }

    pub(super) fn toggle_header_details(&mut self) {
        self.show_header_details = !self.show_header_details;
        self.status = StatusLine::temporary(
            if self.show_header_details {
                "email addresses and Message-ID shown"
            } else {
                "email addresses and Message-ID hidden"
            },
            Duration::from_secs(2),
        );
    }

    /// Drag the mailbox divider. Mouse moves over the attachment list select
    /// the hovered file, and footer clicks activate Open/Save/Mark actions.
    /// Persist a divider adjustment to the active config.toml, including custom
    /// --config paths. When Jerc has no config file, the width remains session-only.
    pub(super) fn save_panel_width(&mut self, panel: crate::config::PanelWidth) {
        let Some(path) = self.theme_config_path.as_deref() else {
            return;
        };
        let value = match panel {
            crate::config::PanelWidth::Mailbox => self.mailbox_width_percent,
            crate::config::PanelWidth::Preview => self.preview_width_percent,
        };
        if let Err(error) = crate::config::save_ui_panel_width(path, panel, value) {
            self.status = StatusLine::error(format!("could not save panel width: {error:#}"));
        }
    }

    /// Terminal graphics for the compose picker. Cached by path and cell size;
    /// selection changes never decode images on each render frame.
    pub fn ensure_compose_file_preview(&mut self, width: u16, height: u16) {
        if self
            .composer
            .as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| picker.smooth_target.is_some())
        {
            return;
        }
        let candidate = self
            .composer
            .as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .and_then(|picker| {
                picker
                    .entries
                    .get(picker.selected)
                    .map(|entry| (entry.path.clone(), &picker.preview))
            });
        let Some((path, ComposeFilePreview::Image(bytes))) = candidate else {
            self.compose_file_preview_image = None;
            self.compose_file_preview_identity = None;
            return;
        };
        let identity = (path, width, height);
        if self.compose_file_preview_identity.as_ref() == Some(&identity) {
            return;
        }
        self.compose_file_preview_identity = Some(identity);
        self.compose_file_preview_image = None;
        let Some(picker) = &self.image_picker else {
            return;
        };
        if let Ok(image) = image::load_from_memory(bytes) {
            let target = reader_image_target_size(
                (image.width(), image.height()),
                (None, None),
                (picker.font_size().width, picker.font_size().height),
                (width.max(1), height.max(1)),
            );
            self.compose_file_preview_image = picker
                .new_protocol(
                    image,
                    target,
                    Resize::Fit(Some(image::imageops::FilterType::Lanczos3)),
                )
                .ok();
        }
    }

    #[must_use]
    pub fn compose_file_preview_image(&self) -> Option<&Protocol> {
        self.compose_file_preview_image.as_ref()
    }

    #[must_use]
    pub fn attachment_preview(&self) -> Option<&Protocol> {
        self.attachment_preview.as_ref()
    }

    pub(super) fn open_attachment(&mut self) {
        if let Some(file) = self
            .remote_attachment_refs
            .get(self.attachment_selection)
            .cloned()
        {
            self.post_remote_transfer(crate::worker::RemoteAttachmentJob::Open(file));
            return;
        }
        let Some(file) = self.attachment_files.get(self.attachment_selection) else {
            return;
        };
        let filename = file.filename.clone();
        match export_attachment_for_open(file) {
            Ok(path) => {
                match open_url(&path.to_string_lossy()) {
                    Ok(()) => {
                        // External viewers may open asynchronously, so retain
                        // the file while Jerc runs, not just for this call.
                        self.opened_attachment_files.push_back(Arc::new(path));
                        while self.opened_attachment_files.len() > 4 {
                            self.opened_attachment_files.pop_front();
                        }
                        self.status = StatusLine::temporary(
                            format!("opened {filename} with system viewer"),
                            Duration::from_secs(3),
                        );
                    }
                    Err(error) => {
                        self.status =
                            StatusLine::error(format!("could not open attachment: {error}"));
                    }
                }
            }
            Err(error) => {
                self.status = StatusLine::error(format!("could not export attachment: {error:#}"))
            }
        }
    }

    pub fn configure_mail_images(&mut self, picker: Picker, runtime_tx: SyncSender<RuntimeEvent>) {
        self.image_picker = Some(picker);
        self.image_runtime_tx = Some(runtime_tx);
    }

    pub fn ensure_reader_image(&mut self, width: u16, height: u16) {
        if !self.reader_images_enabled || !self.message_open {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        if !account.prefer_html {
            return;
        }
        let Some(candidate) = account
            .selected_body()
            .and_then(|body| body.html_image_candidates().into_iter().next())
        else {
            return;
        };
        let source = candidate.source;
        if self
            .reader_image
            .as_ref()
            .is_some_and(|image| image.source == source)
            || self.reader_image_pending.as_deref() == Some(source.as_str())
        {
            return;
        }
        self.reader_image = None;
        self.reader_image_target_width = width.max(1);
        self.reader_image_target_height = height.max(1);
        self.reader_image_size_hint = Some((candidate.width_px, candidate.height_px));
        if self.image_loader.is_none()
            && let Some(tx) = &self.image_runtime_tx
        {
            self.image_loader = Some(MailImageLoader::spawn(tx.clone()));
        }
        if let Some(loader) = &self.image_loader
            && loader.request(source.clone()).is_ok()
        {
            self.reader_image_pending = Some(source);
            self.status = StatusLine::info("loading remote email image…");
        } else {
            self.reader_image_size_hint = None;
        }
    }

    pub fn handle_mail_image_event(&mut self, event: MailImageEvent) {
        if self.reader_image_pending.as_deref() != Some(event.source.as_str()) {
            return;
        }
        self.reader_image_pending = None;
        let size_hint = self.reader_image_size_hint.take().unwrap_or((None, None));
        let Some(picker) = &self.image_picker else {
            return;
        };
        match event.result {
            Ok(image) => {
                let font_size = picker.font_size();
                let target = reader_image_target_size(
                    (image.width(), image.height()),
                    (size_hint.0, size_hint.1),
                    (font_size.width, font_size.height),
                    (
                        self.reader_image_target_width,
                        self.reader_image_target_height,
                    ),
                );
                match picker.new_protocol(
                    image,
                    target,
                    Resize::Fit(Some(image::imageops::FilterType::Lanczos3)),
                ) {
                    Ok(protocol) => {
                        self.reader_image = Some(ReaderImage {
                            source: event.source,
                            protocol,
                        });
                        self.status = StatusLine::info("remote email image loaded");
                    }
                    Err(error) => {
                        self.status = StatusLine::temporary(
                            format!("image render failed: {error}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            Err(error) => {
                if error != "tracking pixel ignored" {
                    self.status = StatusLine::temporary(error, Duration::from_secs(3));
                }
            }
        }
    }

    #[must_use]
    pub fn reader_image_protocol(&self) -> Option<&Protocol> {
        if self.reader_images_enabled {
            self.reader_image.as_ref().map(|image| &image.protocol)
        } else {
            None
        }
    }

    pub(super) fn toggle_remote_images(&mut self) {
        self.reader_images_enabled = !self.reader_images_enabled;
        self.reader_image = None;
        self.reader_image_pending = None;
        self.reader_image_size_hint = None;
        self.status = StatusLine::temporary(
            if self.reader_images_enabled {
                "remote images enabled for this session; senders may observe image requests"
            } else {
                "remote images disabled"
            },
            Duration::from_secs(4),
        );
    }
}
