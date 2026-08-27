//! OpenCode provider connection picker and credential form.

use std::collections::{BTreeMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

use crate::{
    editor::Editor,
    renderer::{EffortSlider, OverlayLine, OverlayStyle, OverlayView},
};

const API_KEY_FIELD: &str = "__api_key";
const CUSTOM_PROVIDER_FIELD: &str = "__provider_id";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAuthKind {
    Api,
    OAuth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderAuthRequest {
    pub provider_id: String,
    pub provider_name: String,
    pub method_index: usize,
    pub kind: ProviderAuthKind,
    pub inputs: BTreeMap<String, String>,
    pub api_key: Option<String>,
}

pub enum ProviderPickerResult {
    None,
    Cancel,
    Submit(Box<ProviderAuthRequest>),
}

#[derive(Clone)]
struct Provider {
    id: String,
    name: String,
    connected: bool,
    methods: Vec<AuthMethod>,
}

#[derive(Clone)]
struct AuthMethod {
    index: usize,
    kind: ProviderAuthKind,
    label: String,
    prompts: Vec<AuthPrompt>,
}

#[derive(Clone)]
enum AuthPrompt {
    Text {
        key: String,
        message: String,
        placeholder: String,
        when: Option<When>,
    },
    Select {
        key: String,
        message: String,
        options: Vec<SelectOption>,
        when: Option<When>,
    },
}

#[derive(Clone)]
struct SelectOption {
    label: String,
    value: String,
    hint: Option<String>,
}

#[derive(Clone)]
struct When {
    key: String,
    neq: bool,
    value: String,
}

enum Phase {
    Providers,
    Methods {
        provider: usize,
        selected: usize,
    },
    Input {
        provider: usize,
        method: usize,
        current: usize,
        selected: usize,
        values: BTreeMap<String, String>,
        editor: Box<Editor>,
        validation: Option<String>,
    },
}

enum Transition {
    Phase(Box<Phase>),
    Submit(Box<ProviderAuthRequest>),
}

pub struct ProviderPicker {
    providers: Vec<Provider>,
    selected: usize,
    phase: Phase,
}

impl ProviderPicker {
    pub fn from_value(value: &Value) -> Self {
        let connected = value
            .get("connected")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();
        let auth = value.get("auth").and_then(Value::as_object);
        let mut providers = value
            .get("all")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_owned();
                if id == "openai" {
                    return None;
                }
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned();
                let methods = auth
                    .and_then(|auth| auth.get(&id))
                    .map(parse_methods)
                    .filter(|methods| !methods.is_empty())
                    .unwrap_or_else(|| {
                        vec![AuthMethod {
                            index: 0,
                            kind: ProviderAuthKind::Api,
                            label: "API key".to_owned(),
                            prompts: Vec::new(),
                        }]
                    });
                Some(Provider {
                    connected: connected.contains(&id),
                    id,
                    name,
                    methods,
                })
            })
            .collect::<Vec<_>>();
        providers.sort_by_key(|provider| (!provider.connected, provider.name.to_lowercase()));
        providers.push(Provider {
            id: "other".to_owned(),
            name: "Other".to_owned(),
            connected: false,
            methods: vec![AuthMethod {
                index: 0,
                kind: ProviderAuthKind::Api,
                label: "Custom provider API key".to_owned(),
                prompts: vec![AuthPrompt::Text {
                    key: CUSTOM_PROVIDER_FIELD.to_owned(),
                    message: "Provider ID".to_owned(),
                    placeholder: "lowercase letters, numbers, hyphens".to_owned(),
                    when: None,
                }],
            }],
        });
        Self {
            providers,
            selected: 0,
            phase: Phase::Providers,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ProviderPickerResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ProviderPickerResult::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match &mut self.phase {
            Phase::Providers => match key.code {
                KeyCode::Esc => ProviderPickerResult::Cancel,
                KeyCode::Left | KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    ProviderPickerResult::None
                }
                KeyCode::Char('p') if ctrl => {
                    self.selected = self.selected.saturating_sub(1);
                    ProviderPickerResult::None
                }
                KeyCode::Right | KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.providers.len().saturating_sub(1));
                    ProviderPickerResult::None
                }
                KeyCode::Char('n') if ctrl => {
                    self.selected = (self.selected + 1).min(self.providers.len().saturating_sub(1));
                    ProviderPickerResult::None
                }
                KeyCode::Enter => {
                    if self.providers.is_empty() {
                        return ProviderPickerResult::None;
                    }
                    self.phase = Phase::Methods {
                        provider: self.selected,
                        selected: 0,
                    };
                    ProviderPickerResult::None
                }
                _ => ProviderPickerResult::None,
            },
            Phase::Methods { provider, selected } => {
                let method_count = self.providers[*provider].methods.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Backspace => {
                        self.phase = Phase::Providers;
                        ProviderPickerResult::None
                    }
                    KeyCode::Up | KeyCode::Left => {
                        *selected = selected.saturating_sub(1);
                        ProviderPickerResult::None
                    }
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        *selected = (*selected + 1).min(method_count.saturating_sub(1));
                        ProviderPickerResult::None
                    }
                    KeyCode::Enter => {
                        let transition = Self::begin_method(&self.providers, *provider, *selected);
                        self.apply_transition(transition)
                    }
                    _ => ProviderPickerResult::None,
                }
            }
            Phase::Input {
                provider,
                method,
                current,
                selected,
                values,
                editor,
                validation,
            } => {
                let fields = fields_for(&self.providers[*provider].methods[*method]);
                let Some(field) = fields.get(*current) else {
                    return ProviderPickerResult::None;
                };
                if key.code == KeyCode::Esc {
                    self.phase = Phase::Methods {
                        provider: *provider,
                        selected: *method,
                    };
                    return ProviderPickerResult::None;
                }
                *validation = None;
                match field {
                    AuthPrompt::Select { options, .. } => match key.code {
                        KeyCode::Up | KeyCode::Left => {
                            *selected = selected.saturating_sub(1);
                            ProviderPickerResult::None
                        }
                        KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                            *selected = (*selected + 1).min(options.len().saturating_sub(1));
                            ProviderPickerResult::None
                        }
                        KeyCode::Enter => {
                            if let Some(option) = options.get(*selected) {
                                save_field(field, option.value.clone(), values);
                                let transition = Self::advance_input(
                                    &self.providers,
                                    *provider,
                                    *method,
                                    *current,
                                    values.clone(),
                                );
                                self.apply_transition(transition)
                            } else {
                                ProviderPickerResult::None
                            }
                        }
                        _ => ProviderPickerResult::None,
                    },
                    AuthPrompt::Text { key: field_key, .. } => match key.code {
                        KeyCode::Enter => {
                            let value = editor.take_for_submit().unwrap_or_default();
                            if field_key == API_KEY_FIELD && value.trim().is_empty() {
                                *validation = Some("API key를 입력하세요.".to_owned());
                                return ProviderPickerResult::None;
                            }
                            if field_key == CUSTOM_PROVIDER_FIELD
                                && (value.is_empty()
                                    || !value.chars().all(|character| {
                                        character.is_ascii_lowercase()
                                            || character.is_ascii_digit()
                                            || character == '-'
                                    }))
                            {
                                *validation =
                                    Some("소문자, 숫자, 하이픈만 사용할 수 있습니다.".to_owned());
                                return ProviderPickerResult::None;
                            }
                            save_field(field, value, values);
                            let transition = Self::advance_input(
                                &self.providers,
                                *provider,
                                *method,
                                *current,
                                values.clone(),
                            );
                            self.apply_transition(transition)
                        }
                        KeyCode::Backspace if ctrl => {
                            editor.delete_word_left();
                            ProviderPickerResult::None
                        }
                        KeyCode::Backspace => {
                            editor.backspace();
                            ProviderPickerResult::None
                        }
                        KeyCode::Delete if ctrl => {
                            editor.delete_word_right();
                            ProviderPickerResult::None
                        }
                        KeyCode::Delete => {
                            editor.delete();
                            ProviderPickerResult::None
                        }
                        KeyCode::Left if ctrl || alt => {
                            editor.move_word_left();
                            ProviderPickerResult::None
                        }
                        KeyCode::Right if ctrl || alt => {
                            editor.move_word_right();
                            ProviderPickerResult::None
                        }
                        KeyCode::Left => {
                            editor.move_left();
                            ProviderPickerResult::None
                        }
                        KeyCode::Right => {
                            editor.move_right();
                            ProviderPickerResult::None
                        }
                        KeyCode::Char(ch) if !ctrl => {
                            editor.insert(ch);
                            ProviderPickerResult::None
                        }
                        _ => ProviderPickerResult::None,
                    },
                }
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.phase {
            Phase::Providers => {}
            Phase::Input { editor, .. } => editor.insert_str(text),
            Phase::Methods { .. } => {}
        }
    }

    pub fn select_step(&mut self, step: usize) -> ProviderPickerResult {
        if matches!(self.phase, Phase::Providers) {
            self.selected = step.min(self.providers.len().saturating_sub(1));
            return self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        ProviderPickerResult::None
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        match &self.phase {
            Phase::Providers => self.provider_view(),
            Phase::Methods { provider, selected } => self.method_view(*provider, *selected),
            Phase::Input {
                provider,
                method,
                current,
                selected,
                values,
                editor,
                validation,
            } => self.input_view(
                *provider,
                *method,
                *current,
                *selected,
                values,
                editor,
                validation.as_deref(),
            ),
        }
    }

    fn begin_method(providers: &[Provider], provider: usize, method: usize) -> Transition {
        let fields = fields_for(&providers[provider].methods[method]);
        let values = BTreeMap::new();
        let Some(current) = next_active_field(&fields, 0, &values) else {
            return Transition::Submit(Box::new(auth_request(providers, provider, method, values)));
        };
        Transition::Phase(Box::new(Phase::Input {
            provider,
            method,
            current,
            selected: 0,
            values,
            editor: Box::default(),
            validation: None,
        }))
    }

    fn advance_input(
        providers: &[Provider],
        provider: usize,
        method: usize,
        current: usize,
        values: BTreeMap<String, String>,
    ) -> Transition {
        let fields = fields_for(&providers[provider].methods[method]);
        if let Some(next) = next_active_field(&fields, current + 1, &values) {
            Transition::Phase(Box::new(Phase::Input {
                provider,
                method,
                current: next,
                selected: 0,
                values,
                editor: Box::default(),
                validation: None,
            }))
        } else {
            Transition::Submit(Box::new(auth_request(providers, provider, method, values)))
        }
    }

    fn apply_transition(&mut self, transition: Transition) -> ProviderPickerResult {
        match transition {
            Transition::Phase(phase) => {
                self.phase = *phase;
                ProviderPickerResult::None
            }
            Transition::Submit(request) => ProviderPickerResult::Submit(request),
        }
    }

    fn provider_view(&self) -> OverlayView<'_> {
        OverlayView {
            title: format!("Connect OpenCode provider · {}", self.providers.len()),
            lines: Vec::new(),
            slider: Some(EffortSlider {
                efforts: self
                    .providers
                    .iter()
                    .map(|provider| {
                        format!(
                            "{} {}",
                            if provider.connected { "✓" } else { "○" },
                            provider.name
                        )
                    })
                    .collect(),
                selected: self.selected,
                detail: None,
            }),
            hint: "←→ Move  Enter Select  Esc Close".to_owned(),
            closable: true,
            style: OverlayStyle::Picker,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }

    fn method_view(&self, provider: usize, selected: usize) -> OverlayView<'_> {
        let provider = &self.providers[provider];
        OverlayView {
            title: format!("{} · 인증 방식", provider.name),
            lines: provider
                .methods
                .iter()
                .enumerate()
                .map(|(index, method)| OverlayLine {
                    text: format!(
                        "{}  ·  {}",
                        method.label,
                        match method.kind {
                            ProviderAuthKind::Api => "API key",
                            ProviderAuthKind::OAuth => "OAuth",
                        }
                    ),
                    selected: index == selected,
                    muted: false,
                })
                .collect(),
            slider: None,
            hint: "↑↓ Move  Enter Continue  Esc Provider list".to_owned(),
            closable: true,
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn input_view(
        &self,
        provider: usize,
        method: usize,
        current: usize,
        selected: usize,
        values: &BTreeMap<String, String>,
        editor: &Editor,
        validation: Option<&str>,
    ) -> OverlayView<'_> {
        let provider = &self.providers[provider];
        let method = &provider.methods[method];
        let fields = fields_for(method);
        let field = &fields[current];
        let mut lines = values
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), API_KEY_FIELD | CUSTOM_PROVIDER_FIELD))
            .map(|(key, value)| OverlayLine {
                text: format!("✓ {key}: {value}"),
                selected: false,
                muted: true,
            })
            .collect::<Vec<_>>();
        let field_title = match field {
            AuthPrompt::Text { key, .. }
                if key == API_KEY_FIELD
                    && matches!(provider.id.as_str(), "opencode" | "opencode-go") =>
            {
                "API key · https://opencode.ai/auth"
            }
            AuthPrompt::Text { key, .. } if key == API_KEY_FIELD && provider.id == "vercel" => {
                "API key · https://vercel.link/ai-gateway-token"
            }
            _ => field_message(field),
        };
        lines.push(OverlayLine {
            text: field_title.to_owned(),
            selected: false,
            muted: false,
        });
        match field {
            AuthPrompt::Select { options, .. } => {
                lines.extend(options.iter().enumerate().map(|(index, option)| {
                    OverlayLine {
                        text: option
                            .hint
                            .as_ref()
                            .map(|hint| format!("{}  ·  {hint}", option.label))
                            .unwrap_or_else(|| option.label.clone()),
                        selected: index == selected,
                        muted: false,
                    }
                }));
            }
            AuthPrompt::Text {
                key, placeholder, ..
            } => {
                let value = if key == API_KEY_FIELD {
                    "•".repeat(editor.text().chars().count())
                } else if editor.text().is_empty() {
                    placeholder.clone()
                } else {
                    editor.text()
                };
                lines.push(OverlayLine {
                    text: if value.is_empty() {
                        "│".to_owned()
                    } else {
                        format!("│ {value}")
                    },
                    selected: true,
                    muted: editor.text().is_empty(),
                });
            }
        }
        if let Some(validation) = validation {
            lines.push(OverlayLine {
                text: validation.to_owned(),
                selected: false,
                muted: false,
            });
        }
        OverlayView {
            title: format!("{} · {}", provider.name, method.label),
            lines,
            slider: None,
            hint: "Enter Continue  Esc Auth method".to_owned(),
            closable: true,
            style: OverlayStyle::Panel,
            input: None,
            input_label: "",
            input_placeholder: "",
        }
    }
}

fn parse_methods(value: &Value) -> Vec<AuthMethod> {
    let entries = value
        .as_array()
        .map(Vec::as_slice)
        .or_else(|| value.as_object().map(|_| std::slice::from_ref(value)))
        .unwrap_or_default();
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, method)| {
            let kind = match method.get("type").and_then(Value::as_str)? {
                "api" => ProviderAuthKind::Api,
                "oauth" => ProviderAuthKind::OAuth,
                _ => return None,
            };
            Some(AuthMethod {
                index,
                kind,
                label: method
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("Connect")
                    .to_owned(),
                prompts: method
                    .get("prompts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_prompt)
                    .collect(),
            })
        })
        .collect()
}

fn auth_request(
    providers: &[Provider],
    provider: usize,
    method: usize,
    mut values: BTreeMap<String, String>,
) -> ProviderAuthRequest {
    let provider = &providers[provider];
    let method = &provider.methods[method];
    let provider_id = values
        .remove(CUSTOM_PROVIDER_FIELD)
        .unwrap_or_else(|| provider.id.clone());
    ProviderAuthRequest {
        provider_name: if provider.id == "other" {
            provider_id.clone()
        } else {
            provider.name.clone()
        },
        provider_id,
        method_index: method.index,
        kind: method.kind.clone(),
        api_key: values.remove(API_KEY_FIELD),
        inputs: values,
    }
}

fn parse_prompt(value: &Value) -> Option<AuthPrompt> {
    let key = value.get("key")?.as_str()?.to_owned();
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(&key)
        .to_owned();
    let when = value.get("when").and_then(parse_when);
    match value.get("type").and_then(Value::as_str)? {
        "text" => Some(AuthPrompt::Text {
            key,
            message,
            placeholder: value
                .get("placeholder")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            when,
        }),
        "select" => Some(AuthPrompt::Select {
            key,
            message,
            options: value
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    Some(SelectOption {
                        label: option.get("label")?.as_str()?.to_owned(),
                        value: option.get("value")?.as_str()?.to_owned(),
                        hint: option
                            .get("hint")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    })
                })
                .collect(),
            when,
        }),
        _ => None,
    }
}

fn parse_when(value: &Value) -> Option<When> {
    Some(When {
        key: value.get("key")?.as_str()?.to_owned(),
        neq: value.get("op").and_then(Value::as_str) == Some("neq"),
        value: value.get("value")?.as_str()?.to_owned(),
    })
}

fn fields_for(method: &AuthMethod) -> Vec<AuthPrompt> {
    let mut fields = method.prompts.clone();
    if method.kind == ProviderAuthKind::Api {
        fields.push(AuthPrompt::Text {
            key: API_KEY_FIELD.to_owned(),
            message: "API key".to_owned(),
            placeholder: String::new(),
            when: None,
        });
    }
    fields
}

fn next_active_field(
    fields: &[AuthPrompt],
    start: usize,
    values: &BTreeMap<String, String>,
) -> Option<usize> {
    (start..fields.len()).find(|index| field_active(&fields[*index], values))
}

fn field_active(field: &AuthPrompt, values: &BTreeMap<String, String>) -> bool {
    let when = match field {
        AuthPrompt::Text { when, .. } | AuthPrompt::Select { when, .. } => when,
    };
    when.as_ref().is_none_or(|when| {
        let Some(actual) = values.get(&when.key) else {
            return false;
        };
        let equal = actual == &when.value;
        if when.neq { !equal } else { equal }
    })
}

fn field_message(field: &AuthPrompt) -> &str {
    match field {
        AuthPrompt::Text { message, .. } | AuthPrompt::Select { message, .. } => message,
    }
}

fn save_field(field: &AuthPrompt, value: String, values: &mut BTreeMap<String, String>) {
    let key = match field {
        AuthPrompt::Text { key, .. } | AuthPrompt::Select { key, .. } => key,
    };
    if !value.is_empty() {
        values.insert(key.clone(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use serde_json::json;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn provider_catalog_excludes_openai_and_marks_connections() {
        let picker = ProviderPicker::from_value(&json!({
            "all": [
                { "id": "openai", "name": "OpenAI" },
                { "id": "xai", "name": "xAI" }
            ],
            "connected": ["xai"],
            "auth": {
                "xai": [
                    { "type": "oauth", "label": "SuperGrok" },
                    { "type": "api", "label": "API key" }
                ]
            }
        }));
        assert_eq!(picker.providers.len(), 2);
        assert_eq!(picker.providers[0].id, "xai");
        assert!(picker.providers[0].connected);
        assert_eq!(picker.providers[0].methods.len(), 2);
    }

    #[test]
    fn provider_picker_uses_a_connected_step_slider() {
        let mut picker = ProviderPicker::from_value(&json!({
            "all": [
                { "id": "xai", "name": "xAI" },
                { "id": "anthropic", "name": "Anthropic" }
            ],
            "connected": ["xai"]
        }));

        let view = picker.overlay_view();
        assert!(view.lines.is_empty());
        let slider = view.slider.expect("provider steps");
        assert_eq!(slider.efforts, ["✓ xAI", "○ Anthropic", "○ Other"]);
        assert_eq!(slider.selected, 0);
        assert!(matches!(
            picker.handle_key(press(KeyCode::Right)),
            ProviderPickerResult::None
        ));
        let view = picker.overlay_view();
        assert_eq!(view.slider.expect("provider steps").selected, 1);
    }

    #[test]
    fn conditional_prompt_uses_previous_selection() {
        let field = AuthPrompt::Text {
            key: "url".to_owned(),
            message: "URL".to_owned(),
            placeholder: String::new(),
            when: Some(When {
                key: "deployment".to_owned(),
                neq: false,
                value: "enterprise".to_owned(),
            }),
        };
        let mut values = BTreeMap::new();
        assert!(!field_active(&field, &values));
        values.insert("deployment".to_owned(), "enterprise".to_owned());
        assert!(field_active(&field, &values));
    }

    #[test]
    fn api_key_is_masked_and_submitted() {
        let mut picker = ProviderPicker::from_value(&json!({
            "all": [{ "id": "xai", "name": "xAI" }],
            "connected": [],
            "auth": { "xai": [{ "type": "api", "label": "API key" }] }
        }));
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::Enter));
        for character in "secret".chars() {
            picker.handle_key(press(KeyCode::Char(character)));
        }
        assert!(
            picker
                .overlay_view()
                .lines
                .iter()
                .all(|line| !line.text.contains("secret"))
        );
        let ProviderPickerResult::Submit(request) = picker.handle_key(press(KeyCode::Enter)) else {
            panic!("API key submission expected");
        };
        assert_eq!(request.provider_id, "xai");
        assert_eq!(request.api_key.as_deref(), Some("secret"));
    }
}
