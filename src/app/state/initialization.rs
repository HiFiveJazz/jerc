//! Application bootstrap and cache restoration.

use super::*;

impl App {
    /// Final process exit only: network reads must not delay returning to the
    /// shell. The cache writers still perform their ordinary local flush.
    pub fn detach_network_workers_for_exit(&mut self) {
        for account in &mut self.accounts {
            account.worker.detach_for_exit();
        }
    }

    pub fn new(config: &Config, workers: Vec<WorkerHandle>) -> Self {
        // A crash may bypass TempPath's normal cleanup. Best-effort removal
        // of old private attachment exports runs once, off the UI startup path.
        static ATTACHMENT_CLEANUP_ONCE: std::sync::Once = std::sync::Once::new();
        ATTACHMENT_CLEANUP_ONCE.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("jerc-attachment-cleanup".into())
                .spawn(cleanup_stale_attachment_files);
        });
        let app_search_mode = config.ui.search_mode;
        let accounts = config
            .accounts
            .iter()
            .zip(workers)
            .map(|(config, worker)| {
                let (snapshot, cache_writer) = cache::open(config);
                let body_store = cache_writer.as_ref().map(cache::Writer::body_store);
                let raw_store = cache_writer.as_ref().map(cache::Writer::raw_store);
                let deferred_seen = snapshot.deferred_seen.clone();
                let uid_validities = snapshot
                    .folders
                    .iter()
                    .filter_map(|(name, folder)| {
                        folder.uid_validity.map(|validity| (name.clone(), validity))
                    })
                    .collect();
                let uid_nexts = snapshot
                    .folders
                    .iter()
                    .filter_map(|(name, folder)| {
                        folder.uid_next.map(|uid_next| (name.clone(), uid_next))
                    })
                    .collect();
                let unverified_mailboxes = snapshot.folders.keys().cloned().collect();
                let mut warm_names = snapshot.folders.keys().cloned().collect::<Vec<_>>();
                warm_names.sort();
                warm_names.retain(|name| name != &config.default_mailbox);
                warm_names.insert(0, config.default_mailbox.clone());
                warm_names.truncate(MAX_WARM_MAILBOXES);
                let warm_folder_order = warm_names
                    .into_iter()
                    .filter(|name| snapshot.folders.contains_key(name))
                    .collect::<VecDeque<_>>();
                let mut warm_folders = snapshot.folders;
                warm_folders.retain(|name, _| warm_folder_order.contains(name));
                let mut account = AccountState {
                    name: config.name.clone(),
                    default_mailbox: config.default_mailbox.clone(),
                    exclude_mailboxes: config.exclude_mailboxes.clone(),
                    from: crate::outgoing::sender_from_account(config),
                    outgoing_configured: config.outgoing.is_some(),
                    copy_to: config.copy_to.clone(),
                    capabilities: None,
                    mailboxes: snapshot
                        .mailboxes
                        .into_iter()
                        .filter(|mailbox| {
                            !crate::config::mailbox_is_excluded(
                                &mailbox.name,
                                &config.exclude_mailboxes,
                            )
                        })
                        .collect(),
                    mailbox_index: 0,
                    mailbox_view_offset: 0,
                    mailbox_view_height: 1,
                    current_mailbox: None,
                    messages: Vec::new(),
                    message_index: 0,
                    preview_scroll: 0,
                    message_view_offset: 0,
                    message_view_height: 1,
                    smooth_message_target: None,
                    smooth_mailbox_target: None,
                    smooth_preview_target: None,
                    preview_max_scroll: 0,
                    prefer_html: true,
                    // Interactive pagination has its own small page size, so an old
                    // `max_messages = 500` config no longer makes All Mail block on
                    // five hundred headers before first paint.
                    page_size: config.page_size.max(1),
                    next_page_offset: 0,
                    has_more: false,
                    page_inflight: None,
                    page_request_id: None,
                    body_request_id: None,
                    last_prefetch_window: None,
                    last_prefetch_anchor: None,
                    mailbox_cache: warm_folders,
                    warm_folder_order,
                    cache_writer,
                    body_store,
                    raw_store,
                    uid_validities,
                    uid_nexts,
                    unverified_mailboxes,
                    search_query: String::new(),
                    search_editing: false,
                    search_mode: app_search_mode,
                    bodies: HashMap::new(),
                    pending_bodies: HashSet::new(),
                    pending_seen: HashMap::new(),
                    deferred_seen,
                    inflight_deferred_seen: HashMap::new(),
                    status_requests: MailboxStatusQueue::default(),
                    pending_inbox_refresh: false,
                    pending_mutation_labels: HashMap::new(),
                    connecting: false,
                    mailboxes_loading: false,
                    messages_loading: false,
                    last_scan_micros: None,
                    restore_message_index: None,
                    worker,
                };
                if let Some(index) = account
                    .mailboxes
                    .iter()
                    .position(|m| m.name == account.default_mailbox)
                {
                    account.mailbox_index = index;
                }
                if let Some(name) = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone())
                {
                    let _ = account.restore_cached_mailbox(&name);
                }
                account
            })
            .collect();
        let mut themes = load_themes();
        let border_type = match config.ui.border_style {
            UiBorderStyle::Square => BorderType::Plain,
            UiBorderStyle::Rounded => BorderType::Rounded,
        };
        for theme in &mut themes {
            theme.border_type = border_type;
        }
        let theme_index = theme_index(&themes, &config.ui.theme);
        let contacts_path = ContactBook::default_path();
        let loaded_contacts = ContactBook::load(&contacts_path);
        let contacts_writable = loaded_contacts.is_ok();
        let contacts_warning = loaded_contacts.as_ref().err().map(|error| {
            format!("Contacts file could not be loaded; preserving it unchanged: {error:#}")
        });
        let mut contacts = loaded_contacts.unwrap_or_default();
        contacts.include_account_identities(&config.accounts);
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            sync_spinner_frame: 0,
            startup_warnings: contacts_warning.into_iter().collect(),
            show_startup_warnings: false,
            should_quit: false,
            setup: config.accounts.is_empty().then(|| SetupWizard::new(true)),
            remove_account_confirmation: None,
            setup_config_path: config
                .config_path
                .clone()
                .unwrap_or_else(crate::config::default_config_path),
            setup_reload_requested: false,
            setup_sender: None,
            inbox_poll_interval: Duration::from_secs(config.ui.inbox_poll_seconds.clamp(10, 3600)),
            new_mail_bell_enabled: config.ui.new_mail_bell,
            new_mail_bell_pending: false,
            new_mail_notifications_enabled: config.ui.new_mail_notifications,
            notification_icon_style: config.ui.notification_icon_style,
            new_mail_since_launch: 0,
            new_mail_sound_name: config.ui.new_mail_sound_name.clone(),
            new_mail_sound_file: config.ui.new_mail_sound_file.clone(),
            new_mail_notices: VecDeque::new(),
            mailbox_width_percent: config.ui.mailbox_width_percent.clamp(12, 50),
            mailbox_resizing: false,
            preview_width_percent: config.ui.preview_width_percent.clamp(25, 75),
            preview_resizing: false,
            sidebar_visible: true,
            preview_split: false,
            message_open: false,
            reader_cursor: 0,
            reader_column: 0,
            reader_visual_anchor: None,
            reader_visual_linewise: false,
            reader_smooth_target: None,
            reader_preferred_column: None,
            reader_pending_g: false,
            reader_pending_inner_object: false,
            reader_line_count: 0,
            reader_view_height: 1,
            reader_scroll_follow_pending: false,
            image_picker: None,
            image_loader: None,
            reader_image: None,
            reader_image_pending: None,
            reader_image_size_hint: None,
            reader_image_target_width: 48,
            reader_image_target_height: 18,
            reader_images_enabled: config.ui.remote_images,
            quoted_diff_highlighting: false,
            show_header_details: false,
            theme_config_path: config.config_path.clone(),
            attachment_picker_visible: false,
            attachment_selection: 0,
            attachment_files: Arc::new(Vec::new()),
            opened_attachment_files: VecDeque::new(),
            attachment_source: None,
            attachment_cache: VecDeque::new(),
            attachment_marked: HashSet::new(),
            attachment_visual_anchor: None,
            attachment_save_directory: None,
            attachment_destination_picker: None,
            attachment_new_folder: None,
            attachment_pending_count: 0,
            attachment_pending_g: false,
            attachment_smooth_target: None,
            attachment_preview_due: None,
            attachment_preview_inflight: HashSet::new(),
            attachment_preview_results: Arc::new(Mutex::new(VecDeque::new())),
            attachment_preview_cache: VecDeque::new(),
            attachment_pending: None,
            attachment_pending_request_id: None,
            attachment_preview: None,
            attachment_preview_identity: None,
            compose_file_preview_image: None,
            compose_file_preview_identity: None,
            help_visible: false,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            theme_picker_visible: false,
            theme_query: String::new(),
            theme_selected: theme_index,
            theme_scroll: 0,
            themes,
            theme_index,
            theme_before_picker: theme_index,
            count_prefix: None,
            visual_anchor: None,
            message_marked: HashSet::new(),
            visual_selection_style: config.ui.visual_selection_style,
            confirmation: None,
            detach_confirmation: None,
            composer: None,
            yank_register: None,
            contacts,
            contacts_modal: None,
            contacts_path,
            contacts_writable,
            contacts_autocomplete: config.contacts.autocomplete,
            contacts_learn_inbox: config.contacts.learn_from_inbox,
            contacts_learn_sent: config.contacts.learn_from_sent,
            contacts_dirty: false,
            contacts_save_due: None,
            pending_compose: None,
            pending_draft: None,
            pending_draft_replacements: HashMap::new(),
            pending_send: None,
            resumed_draft_account: None,
        }
    }
}
