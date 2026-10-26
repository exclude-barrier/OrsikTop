//! `UiState` settings dialog and the additional-server list editor.
use super::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum SettingsField {
    Host,
    Port,
    Gpu,
    Refresh,
    ProcessRefresh,
    OfflineGrace,
    AutoDiscovery,
    /// The additional-server list (add/edit/delete of `servers`).
    ServerList,
}

impl UiState {
    pub fn open_settings(&mut self, active_server: &str, settings: &AppConfig) {
        let editable_server = settings.server.as_deref().unwrap_or(active_server);
        let (host, port) = endpoint_parts(editable_server);
        self.help_open = false;
        self.process_search_open = false;
        self.settings_open = true;
        self.settings_field = SettingsField::Host;
        self.settings_host = host;
        self.settings_port = port.to_string();
        // Keep the exact original and, when parseable, a structured base so
        // saving can preserve everything the dialog does not edit (scheme,
        // path, userinfo, …) instead of rebuilding a bare `http://host:port`.
        // A scheme-less legacy endpoint is parsed as `http://…` so host and
        // port stay editable while userinfo and path survive.
        self.settings_server_original = editable_server.to_string();
        self.settings_server_base = parse_endpoint_base(editable_server);
        self.settings_gpu = settings.gpu_selector.as_string();
        self.settings_refresh_ms = settings.refresh_ms.to_string();
        self.settings_process_refresh_ms = settings.process_refresh_ms.to_string();
        self.settings_offline_grace_ms = settings.offline_grace_ms.to_string();
        self.settings_auto_discovery = settings.auto_discovery;
        self.settings_servers = settings.servers.clone();
        self.settings_server_index = 0;
        self.settings_server_draft = None;
        self.settings_server_edit_index = None;
        self.settings_server_error = None;
        self.settings_error = None;
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_server_draft = None;
        self.settings_server_edit_index = None;
        self.settings_server_error = None;
        self.settings_error = None;
    }

    pub fn is_settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn settings_next_field(&mut self) {
        self.settings_field = match self.settings_field {
            SettingsField::Host => SettingsField::Port,
            SettingsField::Port => SettingsField::Gpu,
            SettingsField::Gpu => SettingsField::Refresh,
            SettingsField::Refresh => SettingsField::ProcessRefresh,
            SettingsField::ProcessRefresh => SettingsField::OfflineGrace,
            SettingsField::OfflineGrace => SettingsField::AutoDiscovery,
            SettingsField::AutoDiscovery => SettingsField::ServerList,
            SettingsField::ServerList => SettingsField::Host,
        };
        self.settings_server_error = None;
        self.settings_error = None;
    }

    pub fn settings_previous_field(&mut self) {
        self.settings_field = match self.settings_field {
            SettingsField::Host => SettingsField::ServerList,
            SettingsField::Port => SettingsField::Host,
            SettingsField::Gpu => SettingsField::Port,
            SettingsField::Refresh => SettingsField::Gpu,
            SettingsField::ProcessRefresh => SettingsField::Refresh,
            SettingsField::OfflineGrace => SettingsField::ProcessRefresh,
            SettingsField::AutoDiscovery => SettingsField::ServerList,
            SettingsField::ServerList => SettingsField::AutoDiscovery,
        };
        self.settings_server_error = None;
        self.settings_error = None;
    }

    pub fn settings_backspace(&mut self) {
        match self.settings_field {
            SettingsField::Host => {
                self.settings_host.pop();
            }
            SettingsField::Port => {
                self.settings_port.pop();
            }
            SettingsField::Gpu => {
                self.settings_gpu.pop();
            }
            SettingsField::Refresh => {
                self.settings_refresh_ms.pop();
            }
            SettingsField::ProcessRefresh => {
                self.settings_process_refresh_ms.pop();
            }
            SettingsField::OfflineGrace => {
                self.settings_offline_grace_ms.pop();
            }
            SettingsField::AutoDiscovery => {}
            SettingsField::ServerList => {}
        }
        self.settings_error = None;
    }

    pub fn settings_insert_char(&mut self, ch: char) {
        match self.settings_field {
            SettingsField::Host
                if !ch.is_control()
                    && !ch.is_whitespace()
                    && self.settings_host.chars().count() < 64 =>
            {
                self.settings_host.push(ch);
            }
            SettingsField::Port if ch.is_ascii_digit() && self.settings_port.len() < 5 => {
                self.settings_port.push(ch);
            }
            SettingsField::Gpu
                if !ch.is_control() && !ch.is_whitespace() && self.settings_gpu.len() < 40 =>
            {
                self.settings_gpu.push(ch);
            }
            SettingsField::Refresh if ch.is_ascii_digit() && self.settings_refresh_ms.len() < 6 => {
                self.settings_refresh_ms.push(ch);
            }
            SettingsField::ProcessRefresh
                if ch.is_ascii_digit() && self.settings_process_refresh_ms.len() < 6 =>
            {
                self.settings_process_refresh_ms.push(ch);
            }
            SettingsField::OfflineGrace
                if ch.is_ascii_digit() && self.settings_offline_grace_ms.len() < 6 =>
            {
                self.settings_offline_grace_ms.push(ch);
            }
            SettingsField::AutoDiscovery => {}
            _ => {}
        }
        self.settings_error = None;
    }

    /// Apply one key press to the open settings dialog.
    ///
    /// Returns `true` when the key requests "validate, save and apply"
    /// (`Enter` outside a server draft); the caller then persists
    /// [`Self::settings_config`] and adopts it at runtime. While a server
    /// entry is being typed the dialog behaves as a plain text field: `Enter`
    /// commits the entry, `Esc` abandons it, every other key edits the draft.
    pub fn settings_handle_key(&mut self, code: KeyCode) -> bool {
        if self.settings_server_is_editing() {
            match code {
                KeyCode::Esc => self.settings_server_draft_cancel(),
                KeyCode::Backspace => self.settings_server_draft_backspace(),
                KeyCode::Enter => self.settings_server_draft_commit(),
                KeyCode::Char(ch) => self.settings_server_draft_insert_char(ch),
                _ => {}
            }
            return false;
        }
        let on_list = self.settings_server_list_focused();
        match code {
            KeyCode::Esc => self.close_settings(),
            KeyCode::Tab => self.settings_next_field(),
            KeyCode::BackTab => self.settings_previous_field(),
            // On the server list, Up/Down move the highlight and leave the
            // list at either end; elsewhere they move between fields.
            KeyCode::Down if on_list => self.settings_server_step(true),
            KeyCode::Up if on_list => self.settings_server_step(false),
            KeyCode::Down => self.settings_next_field(),
            KeyCode::Up => self.settings_previous_field(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                self.settings_toggle_selected();
            }
            KeyCode::Backspace => self.settings_backspace(),
            // Enter always saves — also on the server list — so the save key
            // never changes meaning with focus.
            KeyCode::Enter => {
                self.settings_server_error = None;
                return true;
            }
            KeyCode::Char('a') | KeyCode::Insert if on_list => self.settings_server_begin_add(),
            KeyCode::Char('e') if on_list => self.settings_server_begin_edit(),
            KeyCode::Char('d') | KeyCode::Delete if on_list => self.settings_server_delete(),
            KeyCode::Char(ch) => self.settings_insert_char(ch),
            _ => {}
        }
        false
    }

    pub fn settings_toggle_selected(&mut self) {
        if self.settings_field == SettingsField::AutoDiscovery {
            self.settings_auto_discovery = !self.settings_auto_discovery;
            self.settings_error = None;
        }
    }

    /// True while the additional-server list is focused (its own key map).
    pub fn settings_server_list_focused(&self) -> bool {
        self.settings_field == SettingsField::ServerList
    }

    /// True while a new/edited server entry is being typed.
    pub fn settings_server_is_editing(&self) -> bool {
        self.settings_server_draft.is_some()
    }

    /// Additional configured endpoints (`servers`) currently being edited.
    pub fn settings_server_entries(&self) -> &[String] {
        &self.settings_servers
    }

    /// Current text of the server-entry draft, if any.
    pub fn settings_server_draft(&self) -> Option<&str> {
        self.settings_server_draft.as_deref()
    }

    /// Error of the additional-server list (validation or duplicate), if any.
    pub fn settings_server_error(&self) -> Option<&str> {
        self.settings_server_error.as_deref()
    }

    /// Move the highlight one entry up (`down == false`) or down within the
    /// additional-server list. At either end of the list (or on an empty
    /// list) focus leaves the list for the neighbouring field instead, so
    /// `↑`/`↓` never trap the user inside the list.
    pub fn settings_server_step(&mut self, down: bool) {
        let len = self.settings_servers.len();
        let index = self.settings_server_index.min(len.saturating_sub(1));
        if down && index + 1 < len {
            self.settings_server_index = index + 1;
            self.settings_server_error = None;
        } else if !down && len > 0 && index > 0 {
            self.settings_server_index = index - 1;
            self.settings_server_error = None;
        } else if down {
            self.settings_next_field();
        } else {
            self.settings_previous_field();
        }
    }

    /// Begin inserting a new additional endpoint.
    pub fn settings_server_begin_add(&mut self) {
        self.settings_server_draft = Some(String::new());
        self.settings_server_edit_index = None;
        self.settings_server_error = None;
    }

    /// Begin editing the highlighted additional endpoint.
    pub fn settings_server_begin_edit(&mut self) {
        if let Some(entry) = self.settings_servers.get(self.settings_server_index) {
            self.settings_server_draft = Some(entry.clone());
            self.settings_server_edit_index = Some(self.settings_server_index);
            self.settings_server_error = None;
        }
    }

    /// Remove the highlighted additional endpoint.
    pub fn settings_server_delete(&mut self) {
        if self.settings_server_index < self.settings_servers.len() {
            self.settings_servers.remove(self.settings_server_index);
            if self.settings_server_index >= self.settings_servers.len() {
                self.settings_server_index = self.settings_servers.len().saturating_sub(1);
            }
            self.settings_server_error = None;
        }
    }

    pub fn settings_server_draft_insert_char(&mut self, ch: char) {
        if let Some(draft) = self.settings_server_draft.as_mut() {
            if !ch.is_control()
                && !ch.is_whitespace()
                && draft.chars().count() < MAX_SERVER_DRAFT_CHARS
            {
                draft.push(ch);
            }
        }
        self.settings_server_error = None;
    }

    pub fn settings_server_draft_backspace(&mut self) {
        if let Some(draft) = self.settings_server_draft.as_mut() {
            draft.pop();
        }
        self.settings_server_error = None;
    }

    /// Abandon the current draft without changing the list.
    pub fn settings_server_draft_cancel(&mut self) {
        self.settings_server_draft = None;
        self.settings_server_edit_index = None;
        self.settings_server_error = None;
    }

    /// Commit the current draft: validate and de-duplicate against the primary
    /// endpoint and every other additional entry, then insert or replace it.
    /// A rejected entry keeps the draft open and shows a redacted reason.
    pub fn settings_server_draft_commit(&mut self) {
        let Some(draft) = self.settings_server_draft.clone() else {
            return;
        };
        let endpoint = match crate::server_editor::normalize_endpoint(&draft) {
            Ok(endpoint) => endpoint,
            Err(message) => {
                self.settings_server_error = Some(message);
                return;
            }
        };
        // The primary endpoint is the first source of truth; reject a candidate
        // that resolves to the same target as the primary before looking at the
        // additional entries.
        if let Ok(primary) = self.settings_endpoint() {
            if crate::server_editor::endpoint_key(&primary)
                == crate::server_editor::endpoint_key(&endpoint)
            {
                let label = crate::server_editor::display_label(&primary);
                self.settings_server_error =
                    Some(format!("Duplicates the primary endpoint: {label}"));
                return;
            }
        }
        // Identity of every additional endpoint except the one being replaced.
        let others: Vec<String> = self
            .settings_servers
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != self.settings_server_edit_index)
            .map(|(_, entry)| entry.clone())
            .collect();
        if let Some(duplicate) = crate::server_editor::find_duplicate(&others, &endpoint) {
            let label = crate::server_editor::display_label(duplicate);
            self.settings_server_error = Some(format!("Endpoint already configured: {label}"));
            return;
        }
        match self.settings_server_edit_index {
            Some(index) if index < self.settings_servers.len() => {
                self.settings_servers[index] = endpoint;
                self.settings_server_index = index;
            }
            _ => {
                self.settings_servers.push(endpoint);
                self.settings_server_index = self.settings_servers.len() - 1;
            }
        }
        self.settings_server_draft = None;
        self.settings_server_edit_index = None;
        self.settings_server_error = None;
    }

    /// Monitoring status of the configured endpoints, given the primary
    /// endpoint the dialog would save (see [`crate::server_editor::monitored_flags`]).
    pub fn settings_server_monitoring(&self) -> crate::server_editor::Monitoring {
        let primary = self.settings_endpoint().ok();
        crate::server_editor::monitored_flags(primary.as_deref(), &self.settings_servers)
    }

    pub fn settings_config(&self) -> Result<AppConfig, String> {
        let endpoint = self.settings_endpoint()?;
        let gpu_selector = GpuSelector::parse(&self.settings_gpu);
        let refresh_ms = parse_setting_u64(
            &self.settings_refresh_ms,
            "Refresh",
            MIN_REFRESH_MS,
            MAX_REFRESH_MS,
        )?;
        let process_refresh_ms = parse_setting_u64(
            &self.settings_process_refresh_ms,
            "Process refresh",
            MIN_PROCESS_REFRESH_MS,
            MAX_PROCESS_REFRESH_MS,
        )?;
        let offline_grace_ms = parse_setting_u64(
            &self.settings_offline_grace_ms,
            "Offline grace",
            0,
            MAX_OFFLINE_GRACE_MS,
        )?;

        Ok(AppConfig {
            server: Some(endpoint),
            servers: self.settings_servers.clone(),
            gpu_selector,
            refresh_ms,
            process_refresh_ms,
            offline_grace_ms,
            auto_discovery: self.settings_auto_discovery,
        })
    }

    /// The endpoint to persist from the dialog.
    ///
    /// When the dialog was opened with a parseable URL, only host and port are
    /// edited: scheme, path, userinfo, query and fragment are preserved. If
    /// host and port are unchanged the exact original string is returned, so
    /// opening the dialog and saving without edits round-trips byte for byte
    /// (no silent `https` → `http` downgrade). Legacy endpoints without a
    /// scheme are parsed as `http://…`: unchanged they round-trip byte for
    /// byte, and when host/port are edited the `http` scheme is added while
    /// userinfo and path are kept. Only an endpoint with no recoverable host
    /// falls back to the historical `http://host:port` builder.
    pub(crate) fn settings_endpoint(&self) -> Result<String, String> {
        // `host_str` keeps the brackets of an IPv6 literal; compare and set
        // hosts in their bare form and re-add the brackets for the URL parser.
        let host = self
            .settings_host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']');
        if host.is_empty() {
            return Err("Host / IP must not be empty".to_string());
        }
        if host.chars().any(char::is_whitespace) {
            return Err("Host / IP contains invalid characters".to_string());
        }
        let port = self
            .settings_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Port must be between 1 and 65535".to_string())?;

        let Some(base) = &self.settings_server_base else {
            return build_endpoint(&self.settings_host, &self.settings_port);
        };

        // Compare against the values the fields were initialised with, so a
        // scheme-less endpoint without a port (legacy default 8080, while its
        // `http://…` base would report 80) still counts as unchanged.
        let (base_host, base_port) = endpoint_parts(&self.settings_server_original);
        let base_host = base_host.trim_start_matches('[').trim_end_matches(']');
        if host.eq_ignore_ascii_case(base_host) && port == base_port {
            return Ok(self.settings_server_original.clone());
        }

        let mut url = base.clone();
        let host_for_url = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        url.set_host(Some(&host_for_url))
            .map_err(|_| "Host / IP is not a valid URL host".to_string())?;
        if default_port_for_scheme(url.scheme()) == Some(port) {
            url.set_port(None)
                .map_err(|_| "Port is not valid for this endpoint".to_string())?;
        } else {
            url.set_port(Some(port))
                .map_err(|_| "Port is not valid for this endpoint".to_string())?;
        }
        Ok(url.to_string())
    }

    pub fn set_settings_error(&mut self, error: String) {
        self.settings_error = Some(error);
    }
}
