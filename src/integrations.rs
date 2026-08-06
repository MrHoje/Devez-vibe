//! Keyboard-first pickers for MCP servers, plugins, and plugin marketplaces.
//!
//! Each picker owns its own key handling and overlay rendering and reports back
//! through a small result enum. Anything that needs the app-server is handed to
//! `state`, which turns it into an `Action` the event loop runs; the picker is
//! then reopened with fresh data. Keeping RPC out of here is what lets every
//! navigation and filtering rule be unit-tested without a live server.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

use crate::editor::Editor;
use crate::renderer::{OverlayLine, OverlayStyle, OverlayView, PICKER_ROWS, visible_window};

/// Rows a scrollable detail body lists at once, before it starts scrolling.
const DETAIL_ROWS: usize = 12;

/// How far PageUp/PageDown jump, matching the resume picker.
const PAGE: usize = 8;

// ---------------------------------------------------------------------------
// MCP servers
// ---------------------------------------------------------------------------

/// One entry of `mcpServerStatus/list`.
#[derive(Clone)]
pub struct McpServerInfo {
    pub name: String,
    /// `serverInfo.title`, which is the human label when the server sends one.
    pub title: Option<String>,
    pub version: Option<String>,
    /// `unsupported`, `bearerToken`, `oAuth`, or `notLoggedIn`.
    pub auth_status: String,
    pub tools: Vec<String>,
    pub resources: usize,
    pub website_url: Option<String>,
    /// Set from `mcpServer/startupStatus/updated` when a server failed to start.
    pub failure: Option<String>,
}

impl McpServerInfo {
    pub fn list_from_value(response: &Value) -> Vec<Self> {
        let mut servers = response
            .get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(Self::from_value)
            .collect::<Vec<_>>();
        servers.sort_by_key(|server| server.name.to_lowercase());
        servers
    }

    fn from_value(entry: &Value) -> Option<Self> {
        let name = entry.get("name")?.as_str()?.to_owned();
        let info = entry.get("serverInfo");
        let mut tools = entry
            .get("tools")
            .and_then(Value::as_object)
            .map(|tools| tools.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        tools.sort();
        Some(Self {
            name,
            title: info
                .and_then(|info| info.get("title"))
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .map(ToOwned::to_owned),
            version: info
                .and_then(|info| info.get("version"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            auth_status: entry
                .get("authStatus")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            tools,
            resources: entry
                .get("resources")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            website_url: info
                .and_then(|info| info.get("websiteUrl"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            failure: None,
        })
    }

    pub fn needs_login(&self) -> bool {
        self.auth_status == "notLoggedIn"
    }

    /// Builds a server entry for tests that only care about name, auth and tool
    /// count.
    #[cfg(test)]
    pub fn probe(name: &str, auth_status: &str, tools: usize) -> Self {
        Self {
            name: name.to_owned(),
            title: None,
            version: None,
            auth_status: auth_status.to_owned(),
            tools: (0..tools).map(|index| format!("tool{index}")).collect(),
            resources: 0,
            website_url: None,
            failure: None,
        }
    }

    fn status(&self) -> &str {
        if self.failure.is_some() {
            return "failed";
        }
        match self.auth_status.as_str() {
            "notLoggedIn" => "needs login",
            "unsupported" => "connected",
            "bearerToken" | "oAuth" => "authenticated",
            other => other,
        }
    }

    fn glyph(&self) -> &'static str {
        if self.failure.is_some() {
            "✗"
        } else if self.needs_login() {
            "○"
        } else {
            "✓"
        }
    }

    fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// Reads a failed `mcpServer/startupStatus/updated` into a server name and the
/// reason to show for it. Returns `None` for every other status, so a server
/// that is merely starting is not mistaken for a broken one.
pub fn parse_startup_failure(params: &Value) -> Option<(String, String)> {
    if params.get("status").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("MCP")
        .to_owned();
    // Expired credentials are the one failure the user can fix from here, so it
    // gets the actionable message rather than the raw transport error.
    let detail = if params.get("failureReason").and_then(Value::as_str)
        == Some("reauthenticationRequired")
    {
        format!("인증이 만료되었습니다. /mcp login {name}")
    } else {
        params
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("MCP 서버를 시작하지 못했습니다.")
            .to_owned()
    };
    Some((name, detail))
}

pub enum McpPickerResult {
    None,
    Cancel,
    /// Start the OAuth flow for this server.
    Login(String),
    /// Re-read the MCP config and restart the servers.
    Reconnect,
}

pub struct McpPicker {
    servers: Vec<McpServerInfo>,
    selected: usize,
    /// `true` while the selected server's detail page is open.
    detail: bool,
    tool_offset: usize,
    query: Editor,
    notice: Option<String>,
}

impl McpPicker {
    pub fn new(servers: Vec<McpServerInfo>) -> Self {
        Self {
            servers,
            selected: 0,
            detail: false,
            tool_offset: 0,
            query: Editor::default(),
            notice: None,
        }
    }

    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> McpPickerResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return McpPickerResult::None;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.detail {
            return self.handle_detail_key(key, ctrl);
        }

        match key.code {
            KeyCode::Esc => McpPickerResult::Cancel,
            KeyCode::Char('c') if ctrl => McpPickerResult::Cancel,
            KeyCode::Enter => {
                if self.filtered().is_empty() {
                    McpPickerResult::None
                } else {
                    self.detail = true;
                    self.tool_offset = 0;
                    McpPickerResult::None
                }
            }
            KeyCode::Char('r') if ctrl => McpPickerResult::Reconnect,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                McpPickerResult::None
            }
            KeyCode::Char('p') if ctrl => {
                self.selected = self.selected.saturating_sub(1);
                McpPickerResult::None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.filtered().len().saturating_sub(1));
                McpPickerResult::None
            }
            KeyCode::Char('n') if ctrl => {
                self.selected = (self.selected + 1).min(self.filtered().len().saturating_sub(1));
                McpPickerResult::None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(PAGE);
                McpPickerResult::None
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + PAGE).min(self.filtered().len().saturating_sub(1));
                McpPickerResult::None
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.selected = 0;
                McpPickerResult::None
            }
            KeyCode::Backspace if ctrl => {
                self.query.delete_word_left();
                self.selected = 0;
                McpPickerResult::None
            }
            KeyCode::Backspace => {
                self.query.backspace();
                self.selected = 0;
                McpPickerResult::None
            }
            KeyCode::Char(ch) if !ctrl => {
                self.query.insert(ch);
                self.selected = 0;
                McpPickerResult::None
            }
            _ => McpPickerResult::None,
        }
    }

    fn handle_detail_key(&mut self, key: KeyEvent, ctrl: bool) -> McpPickerResult {
        let tools = self
            .selected_server()
            .map_or(0, |server| server.tools.len());
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                self.detail = false;
                McpPickerResult::None
            }
            KeyCode::Char('c') if ctrl => McpPickerResult::Cancel,
            KeyCode::Char('l') => match self.selected_server() {
                Some(server) if server.needs_login() => McpPickerResult::Login(server.name.clone()),
                Some(_) => {
                    self.notice = Some("이 서버는 OAuth 로그인이 필요하지 않습니다.".to_owned());
                    McpPickerResult::None
                }
                None => McpPickerResult::None,
            },
            KeyCode::Char('r') => McpPickerResult::Reconnect,
            KeyCode::Up => {
                self.tool_offset = self.tool_offset.saturating_sub(1);
                McpPickerResult::None
            }
            KeyCode::Down => {
                self.tool_offset = (self.tool_offset + 1).min(tools.saturating_sub(DETAIL_ROWS));
                McpPickerResult::None
            }
            KeyCode::PageUp => {
                self.tool_offset = self.tool_offset.saturating_sub(PAGE);
                McpPickerResult::None
            }
            KeyCode::PageDown => {
                self.tool_offset = (self.tool_offset + PAGE).min(tools.saturating_sub(DETAIL_ROWS));
                McpPickerResult::None
            }
            _ => McpPickerResult::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if !self.detail {
            self.query.insert_str(text);
            self.selected = 0;
        }
    }

    /// Applies a failure report, so a server that never came up is still listed.
    pub fn apply_failure(&mut self, name: &str, detail: Option<String>) {
        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.name.eq_ignore_ascii_case(name))
        {
            server.failure = Some(detail.unwrap_or_else(|| "시작하지 못했습니다.".to_owned()));
        }
    }

    fn filtered(&self) -> Vec<&McpServerInfo> {
        let query = self.query.text().to_lowercase();
        self.servers
            .iter()
            .filter(|server| {
                query.is_empty()
                    || server.name.to_lowercase().contains(&query)
                    || server.label().to_lowercase().contains(&query)
                    || server
                        .tools
                        .iter()
                        .any(|tool| tool.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn selected_server(&self) -> Option<&McpServerInfo> {
        self.filtered().get(self.selected).copied()
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        if self.detail {
            return self.detail_view();
        }
        let filtered = self.filtered();
        let window = visible_window(Some(self.selected), filtered.len(), PICKER_ROWS);
        let start = window.start;
        let mut lines = filtered[window]
            .iter()
            .enumerate()
            .map(|(offset, server)| OverlayLine {
                text: format!(
                    "{} {}  ·  {}  ·  {} tools",
                    server.glyph(),
                    server.label(),
                    server.status(),
                    server.tools.len()
                ),
                selected: start + offset == self.selected,
                muted: false,
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(OverlayLine {
                text: if self.servers.is_empty() {
                    "연결된 MCP 서버가 없습니다.".to_owned()
                } else {
                    "검색 결과가 없습니다.".to_owned()
                },
                selected: false,
                muted: true,
            });
        }
        OverlayView {
            closable: false,
            title: format!("MCP servers · {}", self.servers.len()),
            lines,
            slider: None,
            hint: self
                .notice
                .clone()
                .unwrap_or_else(|| "↑↓ 이동  Enter 상세  Ctrl+R 재연결  Esc 닫기".to_owned()),
            style: OverlayStyle::Panel,
            input: Some(&self.query),
            input_label: "",
            input_placeholder: "서버 또는 도구 이름으로 검색…",
        }
    }

    fn detail_view(&self) -> OverlayView<'_> {
        let Some(server) = self.selected_server() else {
            return OverlayView {
                closable: false,
                title: "MCP server".to_owned(),
                lines: vec![OverlayLine {
                    text: "서버를 찾을 수 없습니다.".to_owned(),
                    selected: false,
                    muted: true,
                }],
                slider: None,
                hint: "Esc 뒤로".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            };
        };

        let mut lines = vec![OverlayLine {
            text: format!("Status: {}", server.status()),
            selected: false,
            muted: false,
        }];
        if let Some(failure) = server.failure.as_deref() {
            lines.push(OverlayLine {
                text: format!("Error: {failure}"),
                selected: false,
                muted: false,
            });
        }
        lines.push(OverlayLine {
            text: format!("Auth: {}", server.auth_status),
            selected: false,
            muted: true,
        });
        if let Some(version) = server.version.as_deref() {
            lines.push(OverlayLine {
                text: format!("Version: {version}"),
                selected: false,
                muted: true,
            });
        }
        if let Some(url) = server.website_url.as_deref() {
            lines.push(OverlayLine {
                text: format!("Website: {url}"),
                selected: false,
                muted: true,
            });
        }
        if server.resources > 0 {
            lines.push(OverlayLine {
                text: format!("Resources: {}", server.resources),
                selected: false,
                muted: true,
            });
        }
        lines.push(OverlayLine {
            text: String::new(),
            selected: false,
            muted: true,
        });
        lines.push(OverlayLine {
            text: format!("Tools ({})", server.tools.len()),
            selected: false,
            muted: false,
        });
        if server.tools.is_empty() {
            lines.push(OverlayLine {
                text: "  이 서버는 도구를 제공하지 않습니다.".to_owned(),
                selected: false,
                muted: true,
            });
        } else {
            let end = (self.tool_offset + DETAIL_ROWS).min(server.tools.len());
            for tool in &server.tools[self.tool_offset..end] {
                lines.push(OverlayLine {
                    text: format!("  • {tool}"),
                    selected: false,
                    muted: true,
                });
            }
            if end < server.tools.len() {
                lines.push(OverlayLine {
                    text: format!("  … +{}", server.tools.len() - end),
                    selected: false,
                    muted: true,
                });
            }
        }

        let hint = self.notice.clone().unwrap_or_else(|| {
            if server.needs_login() {
                "L 로그인  R 재연결  ↑↓ 도구 스크롤  Esc 뒤로".to_owned()
            } else {
                "R 재연결  ↑↓ 도구 스크롤  Esc 뒤로".to_owned()
            }
        });
        OverlayView {
            closable: false,
            title: format!("MCP · {}", server.label()),
            lines,
            slider: None,
            hint,
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// One plugin from `plugin/list`, with the policy flags that decide which
/// actions are legal.
#[derive(Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub marketplace_name: String,
    /// `None` for the remote catalogue, which has no local manifest path.
    pub marketplace_path: Option<String>,
    pub remote_marketplace_name: Option<String>,
    pub description: Option<String>,
    pub installed: bool,
    pub enabled: bool,
    pub available: bool,
    pub toggle_allowed: bool,
    pub uninstall_allowed: bool,
    pub developer: Option<String>,
    pub capabilities: Vec<String>,
    pub website_url: Option<String>,
    pub privacy_policy_url: Option<String>,
    pub terms_of_service_url: Option<String>,
    pub must_show_interstitial: Option<bool>,
}

impl PluginInfo {
    fn from_value(
        plugin: &Value,
        marketplace_name: &str,
        marketplace_path: Option<String>,
    ) -> Self {
        let name = plugin
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("plugin")
            .to_owned();
        let interface = plugin.get("interface");
        let display_name = interface
            .and_then(|interface| interface.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_owned();
        let install_policy = plugin.get("installPolicy").and_then(Value::as_str);
        let availability = plugin.get("availability").and_then(Value::as_str);
        let installed = plugin
            .get("installed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            id: plugin
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_owned(),
            name,
            display_name,
            marketplace_name: marketplace_name.to_owned(),
            remote_marketplace_name: marketplace_path
                .is_none()
                .then(|| marketplace_name.to_owned()),
            marketplace_path,
            description: interface
                .and_then(|interface| interface.get("shortDescription"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned),
            installed,
            enabled: plugin
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            available: availability != Some("DISABLED_BY_ADMIN")
                && install_policy != Some("NOT_AVAILABLE"),
            toggle_allowed: installed
                && install_policy != Some("INSTALLED_BY_DEFAULT")
                && availability != Some("DISABLED_BY_ADMIN"),
            uninstall_allowed: installed && install_policy != Some("INSTALLED_BY_DEFAULT"),
            developer: interface
                .and_then(|interface| interface.get("developerName"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            capabilities: interface
                .and_then(|interface| interface.get("capabilities"))
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect(),
            website_url: interface
                .and_then(|interface| interface.get("websiteUrl"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            privacy_policy_url: interface
                .and_then(|interface| interface.get("privacyPolicyUrl"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            terms_of_service_url: interface
                .and_then(|interface| interface.get("termsOfServiceUrl"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            must_show_interstitial: plugin
                .get("mustShowInstallationInterstitial")
                .and_then(Value::as_bool),
        }
    }

    /// The subset the app-server needs to act on this plugin.
    pub fn target(&self) -> PluginTarget {
        PluginTarget {
            id: self.id.clone(),
            name: self.name.clone(),
            marketplace_path: self.marketplace_path.clone(),
            remote_marketplace_name: self.remote_marketplace_name.clone(),
        }
    }

    fn glyph(&self) -> &'static str {
        if !self.available {
            "⊘"
        } else if self.installed && self.enabled {
            "✓"
        } else if self.installed {
            "○"
        } else {
            "·"
        }
    }

    fn status(&self) -> &'static str {
        if !self.available {
            "blocked by admin"
        } else if self.installed && self.enabled {
            "enabled"
        } else if self.installed {
            "disabled"
        } else {
            "not installed"
        }
    }

    /// The disclosure block shown before installing, so the user sees who wrote
    /// the plugin and what it may reach before it lands in their config.
    pub fn install_disclosure(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(developer) = self.developer.as_deref().filter(|text| !text.is_empty()) {
            lines.push(format!("Developer: {developer}"));
        }
        if !self.capabilities.is_empty() {
            lines.push(format!("Capabilities: {}", self.capabilities.join(", ")));
        }
        if let Some(url) = self.website_url.as_deref() {
            lines.push(format!("Website: {url}"));
        }
        if let Some(url) = self.privacy_policy_url.as_deref() {
            lines.push(format!("Privacy: {url}"));
        }
        if let Some(url) = self.terms_of_service_url.as_deref() {
            lines.push(format!("Terms: {url}"));
        }
        if self.must_show_interstitial.is_none() {
            lines.push("설치 확인 정책이 제공되지 않아 안전하게 확인을 요구합니다.".to_owned());
        }
        lines
    }
}

/// Identifies a plugin for `plugin/read`, `plugin/install`, and friends.
#[derive(Clone)]
pub struct PluginTarget {
    pub id: String,
    pub name: String,
    pub marketplace_path: Option<String>,
    pub remote_marketplace_name: Option<String>,
}

/// One marketplace from `plugin/list`.
#[derive(Clone)]
pub struct MarketplaceInfo {
    pub name: String,
    pub display_name: String,
    pub path: Option<String>,
    pub plugin_count: usize,
    pub installed_count: usize,
}

impl MarketplaceInfo {
    /// `marketplace/remove` and `marketplace/upgrade` only take local git or
    /// path marketplaces; the remote catalogue is not configured by the user.
    pub fn is_configurable(&self) -> bool {
        self.path.is_some()
    }
}

/// The parsed whole of a `plugin/list` response.
pub struct PluginCatalog {
    pub marketplaces: Vec<MarketplaceInfo>,
    pub plugins: Vec<PluginInfo>,
    pub load_errors: Vec<String>,
}

impl PluginCatalog {
    pub fn from_value(response: &Value) -> Self {
        let mut marketplaces = Vec::new();
        let mut plugins = Vec::new();
        for marketplace in response
            .get("marketplaces")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(name) = marketplace.get("name").and_then(Value::as_str) else {
                continue;
            };
            let path = marketplace
                .get("path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let entries = marketplace
                .get("plugins")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let start = plugins.len();
            for plugin in entries {
                plugins.push(PluginInfo::from_value(plugin, name, path.clone()));
            }
            marketplaces.push(MarketplaceInfo {
                name: name.to_owned(),
                display_name: marketplace
                    .get("interface")
                    .and_then(|interface| interface.get("displayName"))
                    .and_then(Value::as_str)
                    .unwrap_or(name)
                    .to_owned(),
                path,
                plugin_count: entries.len(),
                installed_count: plugins[start..]
                    .iter()
                    .filter(|plugin| plugin.installed)
                    .count(),
            });
        }
        Self {
            marketplaces,
            plugins,
            load_errors: response
                .get("marketplaceLoadErrors")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|error| {
                    let name = error
                        .get("name")
                        .or_else(|| error.get("marketplaceName"))
                        .and_then(Value::as_str)
                        .unwrap_or("marketplace");
                    let message = error
                        .get("message")
                        .or_else(|| error.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("불러오지 못했습니다.");
                    format!("{name}: {message}")
                })
                .collect(),
        }
    }

    /// Resolves a `/plugins <subcommand> NAME` argument. An exact id, name, or
    /// display-name match wins; otherwise a partial match only counts when it
    /// is the single candidate, so an ambiguous name never acts on the wrong
    /// plugin.
    pub fn resolve(&self, query: &str) -> Option<&PluginInfo> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        if let Some(plugin) = self.plugins.iter().find(|plugin| {
            plugin.id.eq_ignore_ascii_case(query)
                || plugin.name.eq_ignore_ascii_case(query)
                || plugin.display_name.eq_ignore_ascii_case(query)
        }) {
            return Some(plugin);
        }
        let lowered = query.to_lowercase();
        let mut matches = self.plugins.iter().filter(|plugin| {
            plugin.name.to_lowercase().contains(&lowered)
                || plugin.display_name.to_lowercase().contains(&lowered)
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn installed(&self) -> Vec<&PluginInfo> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.installed)
            .collect()
    }
}

/// The contents `plugin/read` reports for one plugin.
#[derive(Clone, Default)]
pub struct PluginDetail {
    pub summary: Option<String>,
    pub description: Option<String>,
    pub share_url: Option<String>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub apps: Vec<String>,
    pub hooks: usize,
    pub scheduled_tasks: usize,
}

impl PluginDetail {
    pub fn from_value(response: &Value) -> Self {
        let plugin = response.get("plugin").unwrap_or(response);
        let names = |key: &str| {
            plugin
                .get(key)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("(unnamed)")
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        let text = |key: &str| {
            plugin
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        };
        Self {
            summary: text("summary"),
            description: text("description"),
            share_url: text("shareUrl"),
            skills: names("skills"),
            mcp_servers: names("mcpServers"),
            apps: names("apps"),
            hooks: plugin
                .get("hooks")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            scheduled_tasks: plugin
                .get("scheduledTasks")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        }
    }
}

/// Which slice of the catalogue the plugin list is showing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PluginScope {
    Installed,
    Marketplace(String),
}

pub enum PluginPickerResult {
    None,
    Cancel,
    /// Read this plugin's contents, then reopen the picker on its detail page.
    OpenDetail(PluginTarget),
    Install(Box<PluginInfo>),
    Uninstall(Box<PluginInfo>),
    SetEnabled {
        plugin: Box<PluginInfo>,
        enabled: bool,
    },
    OpenMarketplaces,
    OpenUrl(String),
}

enum PluginView {
    /// Top level: installed plugins, each marketplace, and marketplace admin.
    Scopes,
    Plugins(PluginScope),
    /// Boxed because the detail page carries far more than the list views, and
    /// the picker holds one of these for its whole lifetime.
    Detail(Box<PluginDetailView>),
}

struct PluginDetailView {
    target: PluginTarget,
    detail: Option<PluginDetail>,
    offset: usize,
    /// The list Esc goes back to, so a plugin opened from `Installed` does not
    /// dump the user into its marketplace instead.
    origin: PluginScope,
}

pub struct PluginPicker {
    catalog: PluginCatalog,
    view: PluginView,
    query: Editor,
    selected: usize,
    notice: Option<String>,
}

impl PluginPicker {
    pub fn new(catalog: PluginCatalog, scope: Option<PluginScope>) -> Self {
        Self {
            catalog,
            view: match scope {
                Some(scope) => PluginView::Plugins(scope),
                None => PluginView::Scopes,
            },
            query: Editor::default(),
            selected: 0,
            notice: None,
        }
    }

    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }

    /// Reopens directly on a plugin's detail page, once `plugin/read` returned.
    pub fn into_detail(
        mut self,
        target: PluginTarget,
        detail: PluginDetail,
        origin: Option<PluginScope>,
    ) -> Self {
        let origin = origin.unwrap_or_else(|| {
            self.catalog
                .plugins
                .iter()
                .find(|plugin| plugin.id == target.id)
                .map_or(PluginScope::Installed, |plugin| {
                    PluginScope::Marketplace(plugin.marketplace_name.clone())
                })
        });
        self.view = PluginView::Detail(Box::new(PluginDetailView {
            target,
            detail: Some(detail),
            offset: 0,
            origin,
        }));
        self
    }

    /// The scope to restore after an action reloads the catalogue.
    pub fn scope(&self) -> Option<PluginScope> {
        match &self.view {
            PluginView::Scopes => None,
            PluginView::Plugins(scope) => Some(scope.clone()),
            PluginView::Detail(view) => Some(view.origin.clone()),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PluginPickerResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return PluginPickerResult::None;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &self.view {
            PluginView::Scopes => self.handle_scope_key(key, ctrl),
            PluginView::Plugins(_) => self.handle_list_key(key, ctrl),
            PluginView::Detail(_) => self.handle_detail_key(key, ctrl),
        }
    }

    fn handle_scope_key(&mut self, key: KeyEvent, ctrl: bool) -> PluginPickerResult {
        let rows = self.scope_rows().len();
        match key.code {
            KeyCode::Esc => PluginPickerResult::Cancel,
            KeyCode::Char('c') if ctrl => PluginPickerResult::Cancel,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PluginPickerResult::None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(rows.saturating_sub(1));
                PluginPickerResult::None
            }
            KeyCode::Enter => {
                // The marketplace-admin row is always last.
                if self.selected + 1 == rows {
                    return PluginPickerResult::OpenMarketplaces;
                }
                let scope = match self.scope_rows().get(self.selected) {
                    Some(ScopeRow::Installed) => PluginScope::Installed,
                    Some(ScopeRow::Marketplace(name)) => PluginScope::Marketplace(name.clone()),
                    _ => return PluginPickerResult::None,
                };
                self.view = PluginView::Plugins(scope);
                self.selected = 0;
                self.query.clear();
                PluginPickerResult::None
            }
            KeyCode::Char('m') => PluginPickerResult::OpenMarketplaces,
            _ => PluginPickerResult::None,
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent, ctrl: bool) -> PluginPickerResult {
        let len = self.visible_plugins().len();
        match key.code {
            KeyCode::Esc => {
                self.view = PluginView::Scopes;
                self.selected = 0;
                self.query.clear();
                PluginPickerResult::None
            }
            KeyCode::Char('c') if ctrl => PluginPickerResult::Cancel,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PluginPickerResult::None
            }
            KeyCode::Char('p') if ctrl => {
                self.selected = self.selected.saturating_sub(1);
                PluginPickerResult::None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(len.saturating_sub(1));
                PluginPickerResult::None
            }
            KeyCode::Char('n') if ctrl => {
                self.selected = (self.selected + 1).min(len.saturating_sub(1));
                PluginPickerResult::None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(PAGE);
                PluginPickerResult::None
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + PAGE).min(len.saturating_sub(1));
                PluginPickerResult::None
            }
            KeyCode::Enter => match self.selected_plugin() {
                Some(plugin) => PluginPickerResult::OpenDetail(plugin.target()),
                None => PluginPickerResult::None,
            },
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.selected = 0;
                PluginPickerResult::None
            }
            KeyCode::Backspace if ctrl => {
                self.query.delete_word_left();
                self.selected = 0;
                PluginPickerResult::None
            }
            KeyCode::Backspace => {
                self.query.backspace();
                self.selected = 0;
                PluginPickerResult::None
            }
            KeyCode::Char(ch) if !ctrl => {
                self.query.insert(ch);
                self.selected = 0;
                PluginPickerResult::None
            }
            _ => PluginPickerResult::None,
        }
    }

    fn handle_detail_key(&mut self, key: KeyEvent, ctrl: bool) -> PluginPickerResult {
        let Some(plugin) = self.detail_plugin().cloned() else {
            self.view = PluginView::Scopes;
            return PluginPickerResult::None;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                let origin = match &self.view {
                    PluginView::Detail(view) => view.origin.clone(),
                    _ => PluginScope::Marketplace(plugin.marketplace_name.clone()),
                };
                self.view = PluginView::Plugins(origin);
                self.selected = 0;
                PluginPickerResult::None
            }
            KeyCode::Char('c') if ctrl => PluginPickerResult::Cancel,
            KeyCode::Char('i') => self.install(plugin),
            KeyCode::Char('x') => self.uninstall(plugin),
            KeyCode::Char('e') => self.set_enabled(plugin, true),
            KeyCode::Char('d') => self.set_enabled(plugin, false),
            KeyCode::Char('o') => match plugin.website_url.clone() {
                Some(url) => PluginPickerResult::OpenUrl(url),
                None => {
                    self.notice = Some("웹사이트 정보가 없습니다.".to_owned());
                    PluginPickerResult::None
                }
            },
            KeyCode::Up => {
                if let PluginView::Detail(view) = &mut self.view {
                    view.offset = view.offset.saturating_sub(1);
                }
                PluginPickerResult::None
            }
            KeyCode::Down => {
                let body = self.detail_body().len();
                if let PluginView::Detail(view) = &mut self.view {
                    view.offset = (view.offset + 1).min(body.saturating_sub(DETAIL_ROWS));
                }
                PluginPickerResult::None
            }
            _ => PluginPickerResult::None,
        }
    }

    fn install(&mut self, plugin: PluginInfo) -> PluginPickerResult {
        if plugin.installed {
            self.notice = Some("이미 설치되어 있습니다.".to_owned());
            return PluginPickerResult::None;
        }
        if !plugin.available {
            self.notice = Some("관리자 정책으로 설치할 수 없습니다.".to_owned());
            return PluginPickerResult::None;
        }
        PluginPickerResult::Install(Box::new(plugin))
    }

    fn uninstall(&mut self, plugin: PluginInfo) -> PluginPickerResult {
        if !plugin.installed {
            self.notice = Some("설치되지 않은 플러그인입니다.".to_owned());
            return PluginPickerResult::None;
        }
        if !plugin.uninstall_allowed {
            self.notice = Some("관리자가 설치한 플러그인은 제거할 수 없습니다.".to_owned());
            return PluginPickerResult::None;
        }
        PluginPickerResult::Uninstall(Box::new(plugin))
    }

    fn set_enabled(&mut self, plugin: PluginInfo, enabled: bool) -> PluginPickerResult {
        if !plugin.installed {
            self.notice = Some("먼저 설치하세요. (i)".to_owned());
            return PluginPickerResult::None;
        }
        if !plugin.toggle_allowed {
            self.notice = Some("관리자 정책으로 관리되는 플러그인입니다.".to_owned());
            return PluginPickerResult::None;
        }
        if plugin.enabled == enabled {
            self.notice = Some(format!(
                "이미 {}되어 있습니다.",
                if enabled { "활성화" } else { "비활성화" }
            ));
            return PluginPickerResult::None;
        }
        PluginPickerResult::SetEnabled {
            plugin: Box::new(plugin),
            enabled,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if matches!(self.view, PluginView::Plugins(_)) {
            self.query.insert_str(text);
            self.selected = 0;
        }
    }

    fn scope_rows(&self) -> Vec<ScopeRow> {
        let mut rows = vec![ScopeRow::Installed];
        rows.extend(
            self.catalog
                .marketplaces
                .iter()
                .map(|marketplace| ScopeRow::Marketplace(marketplace.name.clone())),
        );
        rows.push(ScopeRow::Marketplaces);
        rows
    }

    /// The plugin rows for the current scope, newest-relevant first: installed
    /// plugins lead, then the rest alphabetically. The remote catalogue runs to
    /// thousands of entries, so the search field is the primary way in.
    fn visible_plugins(&self) -> Vec<&PluginInfo> {
        let PluginView::Plugins(scope) = &self.view else {
            return Vec::new();
        };
        let query = self.query.text().to_lowercase();
        let mut plugins = match scope {
            PluginScope::Installed => self.catalog.installed(),
            PluginScope::Marketplace(name) => self
                .catalog
                .plugins
                .iter()
                .filter(|plugin| &plugin.marketplace_name == name)
                .collect(),
        };
        if !query.is_empty() {
            plugins.retain(|plugin| {
                plugin.name.to_lowercase().contains(&query)
                    || plugin.display_name.to_lowercase().contains(&query)
                    || plugin
                        .description
                        .as_deref()
                        .is_some_and(|text| text.to_lowercase().contains(&query))
            });
        }
        plugins.sort_by(|left, right| {
            right.installed.cmp(&left.installed).then_with(|| {
                left.display_name
                    .to_lowercase()
                    .cmp(&right.display_name.to_lowercase())
            })
        });
        plugins
    }

    fn selected_plugin(&self) -> Option<&PluginInfo> {
        self.visible_plugins().get(self.selected).copied()
    }

    fn detail_plugin(&self) -> Option<&PluginInfo> {
        let PluginView::Detail(view) = &self.view else {
            return None;
        };
        self.catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == view.target.id)
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        match &self.view {
            PluginView::Scopes => self.scopes_view(),
            PluginView::Plugins(scope) => self.plugins_view(scope),
            PluginView::Detail(view) => self.detail_view(view.offset),
        }
    }

    fn scopes_view(&self) -> OverlayView<'_> {
        let rows = self.scope_rows();
        let window = visible_window(Some(self.selected), rows.len(), PICKER_ROWS);
        let start = window.start;
        let mut lines = rows[window]
            .iter()
            .enumerate()
            .map(|(offset, row)| {
                let text = match row {
                    ScopeRow::Installed => {
                        format!("Installed  ·  {}", self.catalog.installed().len())
                    }
                    ScopeRow::Marketplace(name) => {
                        let marketplace = self
                            .catalog
                            .marketplaces
                            .iter()
                            .find(|candidate| &candidate.name == name);
                        match marketplace {
                            Some(marketplace) => format!(
                                "{}  ·  {} plugins  ·  {} installed",
                                marketplace.display_name,
                                marketplace.plugin_count,
                                marketplace.installed_count
                            ),
                            None => name.clone(),
                        }
                    }
                    ScopeRow::Marketplaces => "Manage marketplaces →".to_owned(),
                };
                OverlayLine {
                    text,
                    selected: start + offset == self.selected,
                    muted: matches!(row, ScopeRow::Marketplaces),
                }
            })
            .collect::<Vec<_>>();
        for error in &self.catalog.load_errors {
            lines.push(OverlayLine {
                text: format!("! {error}"),
                selected: false,
                muted: true,
            });
        }
        OverlayView {
            closable: false,
            title: "Plugins".to_owned(),
            lines,
            slider: None,
            hint: self
                .notice
                .clone()
                .unwrap_or_else(|| "↑↓ 이동  Enter 열기  M 마켓플레이스  Esc 닫기".to_owned()),
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }

    fn plugins_view(&self, scope: &PluginScope) -> OverlayView<'_> {
        let plugins = self.visible_plugins();
        let window = visible_window(Some(self.selected), plugins.len(), PICKER_ROWS);
        let start = window.start;
        let mut lines = plugins[window]
            .iter()
            .enumerate()
            .map(|(offset, plugin)| OverlayLine {
                text: format!(
                    "{} {}  ·  {}{}",
                    plugin.glyph(),
                    plugin.display_name,
                    plugin.status(),
                    plugin
                        .description
                        .as_deref()
                        .map(|text| format!("\n{text}"))
                        .unwrap_or_default()
                ),
                selected: start + offset == self.selected,
                muted: false,
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(OverlayLine {
                text: if self.query.is_empty() {
                    "플러그인이 없습니다.".to_owned()
                } else {
                    "검색 결과가 없습니다.".to_owned()
                },
                selected: false,
                muted: true,
            });
        }
        let title = match scope {
            PluginScope::Installed => format!("Installed plugins · {}", plugins.len()),
            PluginScope::Marketplace(name) => {
                let label = self
                    .catalog
                    .marketplaces
                    .iter()
                    .find(|marketplace| &marketplace.name == name)
                    .map_or(name.as_str(), |marketplace| {
                        marketplace.display_name.as_str()
                    });
                format!("{label} · {}", plugins.len())
            }
        };
        OverlayView {
            closable: false,
            title,
            lines,
            slider: None,
            hint: self
                .notice
                .clone()
                .unwrap_or_else(|| "↑↓ 이동  Enter 상세  Esc 뒤로".to_owned()),
            style: OverlayStyle::Panel,
            input: Some(&self.query),
            input_label: "",
            input_placeholder: "플러그인 검색…",
        }
    }

    /// The scrollable body of the detail page, built once so scrolling and
    /// rendering agree on its length.
    fn detail_body(&self) -> Vec<OverlayLine> {
        let PluginView::Detail(view) = &self.view else {
            return Vec::new();
        };
        let Some(plugin) = self.detail_plugin() else {
            return Vec::new();
        };
        let mut lines = vec![OverlayLine {
            text: format!("Status: {}", plugin.status()),
            selected: false,
            muted: false,
        }];
        let mut meta = |label: &str, value: Option<&str>| {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                lines.push(OverlayLine {
                    text: format!("{label}: {value}"),
                    selected: false,
                    muted: true,
                });
            }
        };
        meta("Marketplace", Some(&plugin.marketplace_name));
        meta("Developer", plugin.developer.as_deref());
        meta(
            "Capabilities",
            (!plugin.capabilities.is_empty())
                .then(|| plugin.capabilities.join(", "))
                .as_deref(),
        );
        meta("Website", plugin.website_url.as_deref());
        meta(
            "Shared",
            view.detail
                .as_ref()
                .and_then(|detail| detail.share_url.as_deref()),
        );

        let description = view
            .detail
            .as_ref()
            .and_then(|detail| {
                detail
                    .description
                    .clone()
                    .or_else(|| detail.summary.clone())
            })
            .or_else(|| plugin.description.clone());
        if let Some(description) = description {
            lines.push(OverlayLine {
                text: String::new(),
                selected: false,
                muted: true,
            });
            lines.push(OverlayLine {
                text: description,
                selected: false,
                muted: false,
            });
        }

        if let Some(detail) = view.detail.as_ref() {
            lines.push(OverlayLine {
                text: String::new(),
                selected: false,
                muted: true,
            });
            lines.push(OverlayLine {
                text: "Contents".to_owned(),
                selected: false,
                muted: false,
            });
            let mut listed = false;
            for (label, names) in [
                ("Skills", &detail.skills),
                ("MCP servers", &detail.mcp_servers),
                ("Apps", &detail.apps),
            ] {
                if names.is_empty() {
                    continue;
                }
                listed = true;
                lines.push(OverlayLine {
                    text: format!("  {label} ({}): {}", names.len(), names.join(", ")),
                    selected: false,
                    muted: true,
                });
            }
            for (label, count) in [
                ("Hooks", detail.hooks),
                ("Scheduled tasks", detail.scheduled_tasks),
            ] {
                if count > 0 {
                    listed = true;
                    lines.push(OverlayLine {
                        text: format!("  {label}: {count}"),
                        selected: false,
                        muted: true,
                    });
                }
            }
            if !listed {
                lines.push(OverlayLine {
                    text: "  (없음)".to_owned(),
                    selected: false,
                    muted: true,
                });
            }
        }
        lines
    }

    fn detail_view(&self, offset: usize) -> OverlayView<'_> {
        let Some(plugin) = self.detail_plugin() else {
            return OverlayView {
                closable: false,
                title: "Plugin".to_owned(),
                lines: vec![OverlayLine {
                    text: "플러그인을 찾을 수 없습니다.".to_owned(),
                    selected: false,
                    muted: true,
                }],
                slider: None,
                hint: "Esc 뒤로".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            };
        };
        let body = self.detail_body();
        let end = (offset + DETAIL_ROWS).min(body.len());
        let mut lines = body[offset.min(end)..end].to_vec();
        if end < body.len() {
            lines.push(OverlayLine {
                text: format!("… +{}", body.len() - end),
                selected: false,
                muted: true,
            });
        }

        // Only advertise the actions this plugin's policy actually allows.
        let mut actions = Vec::new();
        if !plugin.installed && plugin.available {
            actions.push("I 설치");
        }
        if plugin.uninstall_allowed {
            actions.push("X 제거");
        }
        if plugin.toggle_allowed {
            actions.push(if plugin.enabled {
                "D 비활성화"
            } else {
                "E 활성화"
            });
        }
        if plugin.website_url.is_some() {
            actions.push("O 웹사이트");
        }
        actions.push("Esc 뒤로");
        OverlayView {
            closable: false,
            title: format!("Plugin · {}", plugin.display_name),
            lines,
            slider: None,
            hint: self.notice.clone().unwrap_or_else(|| actions.join("  ")),
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }
}

enum ScopeRow {
    Installed,
    Marketplace(String),
    Marketplaces,
}

// ---------------------------------------------------------------------------
// Marketplaces
// ---------------------------------------------------------------------------

pub enum MarketplacePickerResult {
    None,
    /// Leaves the marketplace list and returns to the plugin picker.
    Back,
    Cancel,
    Add(String),
    Remove(String),
    /// Upgrades every configured git marketplace. Codex 0.145's
    /// `marketplace/upgrade` ignores its `selectedMarketplaces` selector — a
    /// bogus selector is accepted and every git marketplace is refreshed anyway
    /// — so offering a per-marketplace upgrade would be a promise the server
    /// does not keep.
    UpgradeAll,
}

pub struct MarketplacePicker {
    marketplaces: Vec<MarketplaceInfo>,
    selected: usize,
    /// `Some` while the add field is open; holds the source being typed.
    source: Option<Editor>,
    notice: Option<String>,
}

impl MarketplacePicker {
    pub fn new(marketplaces: Vec<MarketplaceInfo>) -> Self {
        Self {
            marketplaces,
            selected: 0,
            source: None,
            notice: None,
        }
    }

    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MarketplacePickerResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return MarketplacePickerResult::None;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.source.is_some() {
            return self.handle_add_key(key, ctrl);
        }
        match key.code {
            KeyCode::Esc => MarketplacePickerResult::Back,
            KeyCode::Char('c') if ctrl => MarketplacePickerResult::Cancel,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                MarketplacePickerResult::None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.marketplaces.len().saturating_sub(1));
                MarketplacePickerResult::None
            }
            KeyCode::Char('a') => {
                self.source = Some(Editor::default());
                MarketplacePickerResult::None
            }
            KeyCode::Char('x') | KeyCode::Delete => match self.selected_marketplace() {
                Some(marketplace) if marketplace.is_configurable() => {
                    MarketplacePickerResult::Remove(marketplace.name.clone())
                }
                Some(_) => {
                    self.notice = Some("원격 카탈로그는 제거할 수 없습니다.".to_owned());
                    MarketplacePickerResult::None
                }
                None => MarketplacePickerResult::None,
            },
            KeyCode::Char('u') | KeyCode::Enter => MarketplacePickerResult::UpgradeAll,
            _ => MarketplacePickerResult::None,
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent, ctrl: bool) -> MarketplacePickerResult {
        let Some(editor) = self.source.as_mut() else {
            return MarketplacePickerResult::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.source = None;
                MarketplacePickerResult::None
            }
            KeyCode::Char('c') if ctrl => MarketplacePickerResult::Cancel,
            KeyCode::Enter => {
                let source = editor.text().trim().to_owned();
                if source.is_empty() {
                    self.notice = Some("추가할 소스를 입력하세요.".to_owned());
                    return MarketplacePickerResult::None;
                }
                self.source = None;
                MarketplacePickerResult::Add(source)
            }
            KeyCode::Char('u') if ctrl => {
                editor.clear();
                MarketplacePickerResult::None
            }
            KeyCode::Backspace if ctrl => {
                editor.delete_word_left();
                MarketplacePickerResult::None
            }
            KeyCode::Backspace => {
                editor.backspace();
                MarketplacePickerResult::None
            }
            KeyCode::Delete => {
                editor.delete();
                MarketplacePickerResult::None
            }
            KeyCode::Left => {
                editor.move_left();
                MarketplacePickerResult::None
            }
            KeyCode::Right => {
                editor.move_right();
                MarketplacePickerResult::None
            }
            KeyCode::Home => {
                editor.move_home();
                MarketplacePickerResult::None
            }
            KeyCode::End => {
                editor.move_end();
                MarketplacePickerResult::None
            }
            KeyCode::Char(ch) if !ctrl => {
                editor.insert(ch);
                MarketplacePickerResult::None
            }
            _ => MarketplacePickerResult::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(editor) = self.source.as_mut() {
            editor.insert_str(text);
        }
    }

    fn selected_marketplace(&self) -> Option<&MarketplaceInfo> {
        self.marketplaces.get(self.selected)
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        if let Some(source) = self.source.as_ref() {
            return OverlayView {
                closable: false,
                title: "Add marketplace".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "로컬 경로, owner/repo, HTTPS 또는 SSH Git URL".to_owned(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: "owner/repo@ref 형식으로 브랜치를 지정할 수 있습니다.".to_owned(),
                        selected: false,
                        muted: true,
                    },
                ],
                slider: None,
                hint: self
                    .notice
                    .clone()
                    .unwrap_or_else(|| "Enter 추가  Esc 취소".to_owned()),
                style: OverlayStyle::Panel,
                input: Some(source),
                input_label: "Source",
                input_placeholder: "owner/repo 또는 ./path",
            };
        }

        let window = visible_window(Some(self.selected), self.marketplaces.len(), PICKER_ROWS);
        let start = window.start;
        let mut lines = self.marketplaces[window]
            .iter()
            .enumerate()
            .map(|(offset, marketplace)| OverlayLine {
                text: format!(
                    "{} {}  ·  {} plugins\n{}",
                    if marketplace.is_configurable() {
                        "•"
                    } else {
                        "☁"
                    },
                    marketplace.name,
                    marketplace.plugin_count,
                    marketplace
                        .path
                        .as_deref()
                        .unwrap_or("remote catalog (Codex managed)")
                ),
                selected: start + offset == self.selected,
                muted: false,
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(OverlayLine {
                text: "설정된 마켓플레이스가 없습니다.".to_owned(),
                selected: false,
                muted: true,
            });
        }
        OverlayView {
            closable: false,
            title: format!("Marketplaces · {}", self.marketplaces.len()),
            lines,
            slider: None,
            hint: self.notice.clone().unwrap_or_else(|| {
                "A 추가  X 제거  U 모든 Git 마켓플레이스 갱신  Esc 뒤로".to_owned()
            }),
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn mcp_response() -> Value {
        json!({
            "data": [
                {
                    "name": "chrome-devtools",
                    "serverInfo": { "title": "Chrome DevTools", "version": "1.6.0" },
                    "tools": { "click": {}, "fill": {} },
                    "resources": [],
                    "authStatus": "unsupported"
                },
                {
                    "name": "github",
                    "serverInfo": { "version": "2.0.0" },
                    "tools": {},
                    "resources": [],
                    "authStatus": "notLoggedIn"
                }
            ]
        })
    }

    fn plugin_response() -> Value {
        json!({
            "marketplaces": [
                {
                    "name": "openai-bundled",
                    "path": "C:/codex/bundled/marketplace.json",
                    "interface": { "displayName": "OpenAI Bundled" },
                    "plugins": [
                        {
                            "id": "browser@openai-bundled",
                            "name": "browser",
                            "installed": true,
                            "enabled": true,
                            "installPolicy": "AVAILABLE",
                            "availability": "AVAILABLE",
                            "interface": {
                                "displayName": "Browser",
                                "shortDescription": "Drive a browser",
                                "developerName": "OpenAI",
                                "capabilities": ["Write"]
                            }
                        },
                        {
                            "id": "locked@openai-bundled",
                            "name": "locked",
                            "installed": true,
                            "enabled": true,
                            "installPolicy": "INSTALLED_BY_DEFAULT",
                            "availability": "AVAILABLE",
                            "interface": { "displayName": "Locked" }
                        }
                    ]
                },
                {
                    "name": "openai-curated-remote",
                    "path": null,
                    "interface": { "displayName": "OpenAI Curated Remote" },
                    "plugins": [
                        {
                            "id": "slack@openai-curated-remote",
                            "name": "slack",
                            "installed": false,
                            "enabled": false,
                            "installPolicy": "AVAILABLE",
                            "availability": "AVAILABLE",
                            "interface": { "displayName": "Slack" }
                        }
                    ]
                }
            ],
            "marketplaceLoadErrors": [],
            "featuredPluginIds": []
        })
    }

    #[test]
    fn mcp_servers_sort_by_name_and_report_login_state() {
        let servers = McpServerInfo::list_from_value(&mcp_response());

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "chrome-devtools");
        assert_eq!(servers[0].label(), "Chrome DevTools");
        assert_eq!(servers[0].tools, vec!["click", "fill"]);
        assert!(!servers[0].needs_login());
        assert!(servers[1].needs_login());
        assert_eq!(servers[1].status(), "needs login");
    }

    #[test]
    fn mcp_detail_offers_login_only_when_the_server_needs_it() {
        let mut picker = McpPicker::new(McpServerInfo::list_from_value(&mcp_response()));

        // chrome-devtools is first and authenticates by other means.
        picker.handle_key(press(KeyCode::Enter));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('l'))),
            McpPickerResult::None
        ));
        assert!(picker.overlay_view().hint.contains("로그인이 필요하지 않"));

        picker.handle_key(press(KeyCode::Esc));
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(press(KeyCode::Enter));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('l'))),
            McpPickerResult::Login(ref name) if name == "github"
        ));
    }

    #[test]
    fn mcp_search_filters_by_tool_name_without_leaving_a_stale_selection() {
        let mut picker = McpPicker::new(McpServerInfo::list_from_value(&mcp_response()));
        picker.handle_key(press(KeyCode::Down));
        assert_eq!(picker.selected, 1);

        for ch in "fill".chars() {
            picker.handle_key(press(KeyCode::Char(ch)));
        }

        assert_eq!(picker.selected, 0);
        assert_eq!(picker.filtered().len(), 1);
        assert_eq!(picker.filtered()[0].name, "chrome-devtools");
    }

    #[test]
    fn mcp_reconnect_is_reachable_from_both_levels() {
        let mut picker = McpPicker::new(McpServerInfo::list_from_value(&mcp_response()));
        assert!(matches!(
            picker.handle_key(ctrl(KeyCode::Char('r'))),
            McpPickerResult::Reconnect
        ));
        picker.handle_key(press(KeyCode::Enter));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('r'))),
            McpPickerResult::Reconnect
        ));
    }

    #[test]
    fn startup_failures_are_read_from_the_notification_and_land_on_the_server() {
        assert_eq!(
            parse_startup_failure(&json!({
                "name": "github",
                "status": "failed",
                "error": "spawn failed"
            })),
            Some(("github".to_owned(), "spawn failed".to_owned()))
        );

        // Expired credentials get the actionable message instead of the error.
        let (name, detail) = parse_startup_failure(&json!({
            "name": "github",
            "status": "failed",
            "failureReason": "reauthenticationRequired",
            "error": "401"
        }))
        .expect("failure");
        assert_eq!(name, "github");
        assert!(detail.contains("/mcp login github"));

        assert!(parse_startup_failure(&json!({ "name": "github", "status": "running" })).is_none());
        assert!(parse_startup_failure(&json!({ "unrelated": true })).is_none());

        let mut picker = McpPicker::new(McpServerInfo::list_from_value(&mcp_response()));
        picker.apply_failure("github", Some("spawn failed".to_owned()));
        assert_eq!(picker.filtered()[1].status(), "failed");
        assert_eq!(picker.filtered()[1].glyph(), "✗");
    }

    #[test]
    fn catalog_flattens_marketplaces_and_marks_remote_ones() {
        let catalog = PluginCatalog::from_value(&plugin_response());

        assert_eq!(catalog.marketplaces.len(), 2);
        assert_eq!(catalog.marketplaces[0].installed_count, 2);
        assert!(catalog.marketplaces[0].is_configurable());
        assert!(!catalog.marketplaces[1].is_configurable());
        assert_eq!(catalog.plugins.len(), 3);
        assert_eq!(
            catalog
                .plugins
                .iter()
                .find(|plugin| plugin.name == "slack")
                .and_then(|plugin| plugin.remote_marketplace_name.as_deref()),
            Some("openai-curated-remote")
        );
    }

    #[test]
    fn catalog_resolution_prefers_exact_names_and_rejects_ambiguity() {
        let catalog = PluginCatalog::from_value(&plugin_response());

        assert_eq!(catalog.resolve("Browser").expect("exact").name, "browser");
        assert_eq!(
            catalog.resolve("browser@openai-bundled").expect("id").name,
            "browser"
        );
        assert_eq!(
            catalog.resolve("sla").expect("single partial").name,
            "slack"
        );
        // `o` appears in every display name, so it must not resolve.
        assert!(catalog.resolve("o").is_none());
        assert!(catalog.resolve("").is_none());
    }

    #[test]
    fn plugin_policy_blocks_illegal_actions_with_an_inline_reason() {
        let catalog = PluginCatalog::from_value(&plugin_response());
        let locked = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.name == "locked")
            .expect("locked plugin")
            .clone();
        let mut picker = PluginPicker::new(
            PluginCatalog::from_value(&plugin_response()),
            Some(PluginScope::Installed),
        );

        assert!(matches!(
            picker.uninstall(locked.clone()),
            PluginPickerResult::None
        ));
        assert!(picker.overlay_view().hint.contains("제거할 수 없습니다"));
        assert!(matches!(
            picker.set_enabled(locked, false),
            PluginPickerResult::None
        ));
        assert!(picker.overlay_view().hint.contains("관리자 정책"));
    }

    #[test]
    fn installed_plugins_lead_the_list_and_search_narrows_it() {
        let mut picker = PluginPicker::new(
            PluginCatalog::from_value(&plugin_response()),
            Some(PluginScope::Marketplace("openai-bundled".to_owned())),
        );

        let names = picker
            .visible_plugins()
            .iter()
            .map(|plugin| plugin.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["browser", "locked"]);

        for ch in "lock".chars() {
            picker.handle_key(press(KeyCode::Char(ch)));
        }
        assert_eq!(picker.visible_plugins().len(), 1);
        assert_eq!(picker.visible_plugins()[0].name, "locked");
    }

    #[test]
    fn scope_list_drills_into_a_marketplace_and_back_out() {
        let mut picker = PluginPicker::new(PluginCatalog::from_value(&plugin_response()), None);

        // Row 0 is Installed, rows 1..n the marketplaces, last one is admin.
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(press(KeyCode::Enter));
        assert_eq!(
            picker.scope(),
            Some(PluginScope::Marketplace("openai-bundled".to_owned()))
        );

        picker.handle_key(press(KeyCode::Esc));
        assert_eq!(picker.scope(), None);

        // The trailing row always opens marketplace management.
        for _ in 0..picker.scope_rows().len() {
            picker.handle_key(press(KeyCode::Down));
        }
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            PluginPickerResult::OpenMarketplaces
        ));
    }

    #[test]
    fn plugin_detail_reports_contents_from_plugin_read() {
        let detail = PluginDetail::from_value(&json!({
            "plugin": {
                "summary": "Drive a browser",
                "description": "Longer description",
                "skills": [{ "name": "browser:navigate" }],
                "mcpServers": [{ "name": "chrome" }],
                "apps": [],
                "hooks": [{}, {}],
                "scheduledTasks": null
            }
        }));

        assert_eq!(detail.skills, vec!["browser:navigate"]);
        assert_eq!(detail.mcp_servers, vec!["chrome"]);
        assert_eq!(detail.hooks, 2);
        assert_eq!(detail.scheduled_tasks, 0);

        let catalog = PluginCatalog::from_value(&plugin_response());
        let target = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.name == "browser")
            .expect("browser")
            .target();
        let picker = PluginPicker::new(PluginCatalog::from_value(&plugin_response()), None)
            .into_detail(target, detail, None);
        let body = picker
            .detail_body()
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(body.contains("Skills (1): browser:navigate"));
        assert!(body.contains("MCP servers (1): chrome"));
        assert!(body.contains("Hooks: 2"));
        assert!(!body.contains("Scheduled tasks"));
    }

    #[test]
    fn detail_hint_only_lists_actions_the_policy_allows() {
        let catalog = PluginCatalog::from_value(&plugin_response());
        let slack = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.name == "slack")
            .expect("slack")
            .target();
        let picker = PluginPicker::new(PluginCatalog::from_value(&plugin_response()), None)
            .into_detail(slack, PluginDetail::default(), None);

        let hint = picker.overlay_view().hint;
        assert!(hint.contains("I 설치"));
        assert!(!hint.contains("X 제거"));
        assert!(!hint.contains("D 비활성화"));
    }

    #[test]
    fn marketplace_picker_guards_the_remote_catalog_and_collects_a_source() {
        let catalog = PluginCatalog::from_value(&plugin_response());
        let mut picker = MarketplacePicker::new(catalog.marketplaces.clone());

        // Upgrade is all-or-nothing because the server ignores its selector, so
        // both keys have to mean the same thing on any row.
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            MarketplacePickerResult::UpgradeAll
        ));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('u'))),
            MarketplacePickerResult::UpgradeAll
        ));

        picker.handle_key(press(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            MarketplacePickerResult::UpgradeAll
        ));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('x'))),
            MarketplacePickerResult::None
        ));
        assert!(picker.overlay_view().hint.contains("원격 카탈로그는 제거"));

        picker.handle_key(press(KeyCode::Char('a')));
        // An empty source must not be submitted.
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            MarketplacePickerResult::None
        ));
        for ch in "owner/repo".chars() {
            picker.handle_key(press(KeyCode::Char(ch)));
        }
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            MarketplacePickerResult::Add(ref source) if source == "owner/repo"
        ));
    }

    #[test]
    fn marketplace_remove_targets_the_selected_local_marketplace() {
        let catalog = PluginCatalog::from_value(&plugin_response());
        let mut picker = MarketplacePicker::new(catalog.marketplaces.clone());

        assert!(matches!(
            picker.handle_key(press(KeyCode::Char('x'))),
            MarketplacePickerResult::Remove(ref name) if name == "openai-bundled"
        ));
    }

    #[test]
    fn esc_leaves_the_marketplace_list_without_closing_the_overlay() {
        let mut picker = MarketplacePicker::new(Vec::new());
        assert!(matches!(
            picker.handle_key(press(KeyCode::Esc)),
            MarketplacePickerResult::Back
        ));
    }
}
