//! Settings → AI: the model Kuana's own features use (provider, API key in
//! the keychain, model, URL, effort), the agents sessions can run (installed
//! or to install), GitHub Copilot's suggestions and the terminal agents'
//! connections (Claude Code, Gemini CLI).

use super::*;
use crate::ai::{self, PROVIDERS, providers::keys};

/// What the AI page keeps between frames.
#[derive(Default)]
pub(super) struct AiPage {
    /// The models the chosen provider lists, for `(provider, url)`; an error
    /// says why there are none.
    pub models: Option<(String, Option<String>, Result<Vec<(String, String)>, String>)>,
    pub models_task: Option<Task<()>>,
    /// What the key field last did ("Saved in the keychain").
    pub key_status: Option<String>,
}

impl Preferences {
    fn ai_settings(cx: &App) -> ai::AiSettings {
        Settings::get(cx).ai.clone().unwrap_or_default()
    }

    /// Changes the `ai` object; an empty one goes.
    /// Changes the `ai` setting. What's typed in the URL and model fields is
    /// saved first, and afterwards they show the new settings, so a later
    /// save of those fields can't write the old values over a new provider
    /// or model.
    fn write_ai(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut serde_json::Map<String, Value>)) {
        let typed = self.commit_ai_fields(cx);
        if !typed.is_empty() {
            self.applied(typed, cx);
        }
        let result = Settings::update("ai", |old| {
            let mut map = old.and_then(|v| v.as_object().cloned()).unwrap_or_default();
            f(&mut map);
            (!map.is_empty()).then_some(Value::Object(map))
        });
        self.write(result, cx);
        self.fill_ai_fields(cx);
    }

    /// Reads the provider's models in the background (when it changed).
    fn load_models(&mut self, provider: &str, url: Option<String>, force: bool, cx: &mut Context<Self>) {
        if !force && self.ai.models.as_ref().is_some_and(|(p, u, _)| p == provider && *u == url) {
            return;
        }
        let endpoint = ai::Endpoint::of(provider, url.as_deref());
        let (p, u) = (provider.to_string(), url.clone());
        self.ai.models = Some((p.clone(), u.clone(), Err("Loading models…".into())));
        let job = cx.background_executor().spawn(async move { endpoint.and_then(|e| e.models()) });
        self.ai.models_task = Some(cx.spawn(async move |this, cx| {
            let models = job.await;
            this.update(cx, |this, cx| {
                this.ai.models = Some((p, u, models));
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn render_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let settings = Self::ai_settings(cx);
        let provider = settings.provider.clone();
        let info = provider.as_deref().and_then(ai::providers::info);
        let url = provider.as_deref().and_then(|p| settings.providers.get(p)).and_then(|p| p.url.clone());
        let stacked = self.stacked.get();

        // ---- Provider
        let options: Vec<(SharedString, bool)> = std::iter::once(("None".into(), provider.is_none()))
            .chain(PROVIDERS.iter().map(|p| (p.name.into(), provider.as_deref() == Some(p.id))))
            .collect();
        let label: SharedString = info.map_or("None".into(), |i| i.name.into());
        let provider_control = self.dropdown(Menu::AiProvider, label, px(240.), options, cx, |this, ix, _, cx| {
            this.write_ai(cx, |map| {
                match ix.checked_sub(1).and_then(|i| PROVIDERS.get(i)) {
                    Some(p) => {
                        map.insert("provider".into(), json!(p.id));
                        // Another provider's model wouldn't be found there.
                        match p.default_model {
                            Some(model) => map.insert("model".into(), json!(model)),
                            None => map.remove("model"),
                        };
                    }
                    None => {
                        map.remove("provider");
                        map.remove("model");
                    }
                }
            });
            this.ai.models = None;
            this.ai.key_status = None;
        });
        let mut rows = vec![
            setting_row(
                "Provider",
                Some("Where Kuana Agent, Inline Assist and commit messages send their requests. Nothing is sent before you choose one.".into()),
                provider_control,
                None,
                stacked,
                &theme,
            )
            .into_any_element(),
        ];

        if let Some(info) = info {
            // ---- Key
            if !info.keyless || info.id == "custom" {
                let source = keys::source(info);
                let status = self.ai.key_status.clone().unwrap_or_else(|| match source {
                    keys::Source::Keychain => "A key is saved in the keychain.".into(),
                    keys::Source::Environment => format!("Using {}.", info.key_var.unwrap_or("the environment")),
                    keys::Source::Missing if info.keyless => "Optional for this server.".into(),
                    keys::Source::Missing => "No key yet.".into(),
                });
                let provider_id = info.id;
                let control = div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().w(px(220.)).flex().child(input_box(&self.ai_key, None, window, cx)))
                    .child(text_button("ai-key-save", "Save", true, cx).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let key = this.ai_key.read(cx).text(cx);
                        let result = keys::save(provider_id, &key);
                        this.ai.key_status = Some(match &result {
                            Ok(()) if key.trim().is_empty() => "Removed from the keychain.".into(),
                            Ok(()) => "Saved in the keychain.".into(),
                            Err(e) => format!("Couldn't save it: {e}"),
                        });
                        set_field(&this.ai_key, "", cx);
                        // A new key may list other models.
                        let (provider, url) = (provider_id.to_string(), Self::ai_settings(cx).providers.get(provider_id).and_then(|p| p.url.clone()));
                        this.load_models(&provider, url, true, cx);
                        cx.notify();
                    })))
                    .into_any_element();
                let mut description = status;
                if let Some(keys_url) = info.keys_url {
                    description.push_str(&format!(" Get one at {keys_url}."));
                }
                rows.push(setting_row("API key", Some(description.into()), control, None, stacked, &theme).into_any_element());
            }

            // ---- Model
            self.load_models(info.id, url.clone(), false, cx);
            let current = settings.model.clone().or_else(|| info.default_model.map(str::to_string));
            let listed = match &self.ai.models {
                Some((_, _, Ok(models))) => models.clone(),
                _ => Vec::new(),
            };
            let mut model_options: Vec<(SharedString, bool)> = listed.iter().map(|(id, name)| (SharedString::from(if id == name { id.clone() } else { format!("{name} ({id})") }), current.as_deref() == Some(id))).collect();
            let unlisted = current.as_ref().filter(|c| !listed.iter().any(|(id, _)| id == *c)).cloned();
            if let Some(model) = &unlisted {
                model_options.insert(0, (model.clone().into(), true));
            }
            let ids: Vec<String> = unlisted.iter().cloned().chain(listed.iter().map(|(id, _)| id.clone())).collect();
            let label: SharedString = current.clone().unwrap_or_else(|| "Choose a model".into()).into();
            let model_control = if ids.is_empty() {
                div().w(px(240.)).flex().child(input_box(&self.ai_model, None, window, cx)).into_any_element()
            } else {
                self.dropdown(Menu::AiModel, label, px(300.), model_options, cx, move |this, ix, _, cx| {
                    if let Some(id) = ids.get(ix).cloned() {
                        this.write_ai(cx, |map| {
                            map.insert("model".into(), json!(id));
                        });
                    }
                })
            };
            let model_note: SharedString = match &self.ai.models {
                Some((_, _, Err(error))) => format!("{error} You can type a model's id instead.").into(),
                Some((_, _, Ok(models))) if models.is_empty() => "The provider listed no models; type one's id.".into(),
                _ => "The model your requests run on.".into(),
            };
            rows.push(setting_row("Model", Some(model_note), model_control, None, stacked, &theme).into_any_element());

            // ---- URL
            let url_control = div().w(px(300.)).flex().child(input_box(&self.ai_url, None, window, cx)).into_any_element();
            rows.push(
                setting_row("URL", Some(format!("Its API's address; empty for {}.", info.url).into()), url_control, None, stacked, &theme).into_any_element(),
            );

            // ---- Effort
            let efforts = [None, Some(ai::Effort::Low), Some(ai::Effort::Medium), Some(ai::Effort::High), Some(ai::Effort::Xhigh), Some(ai::Effort::Max)];
            let effort_options: Vec<(SharedString, bool)> =
                efforts.iter().map(|e| (SharedString::from(e.map_or("Model's default", |e| effort_label(e))), settings.effort == *e)).collect();
            let effort_control = self.dropdown(Menu::AiEffort, settings.effort.map_or("Model's default", effort_label).into(), px(200.), effort_options, cx, move |this, ix, _, cx| {
                this.write_ai(cx, |map| match efforts.get(ix).copied().flatten() {
                    Some(effort) => {
                        map.insert("effort".into(), json!(effort.key()));
                    }
                    None => {
                        map.remove("effort");
                    }
                });
            });
            rows.push(
                setting_row("Effort", Some("How hard the model thinks, where it lets Kuana say: more is slower and costs more.".into()), effort_control, None, stacked, &theme)
                    .into_any_element(),
            );
        }

        // ---- MCP servers (edited in settings.json)
        let servers: Vec<String> = settings.mcp_servers.keys().cloned().collect();
        let mcp_note: SharedString = if servers.is_empty() {
            "Tools agent sessions may use, from programs speaking the Model Context Protocol. Add them under \"ai\" → \"mcp_servers\" in settings.json.".into()
        } else {
            format!("{}: given to every agent session (Kuana Agent asks before each tool call).", servers.join(", ")).into()
        };
        let mcp_control = text_button("ai-mcp-edit", "Edit in settings.json", false, cx)
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenSettingsFile)))
            .into_any_element();
        let mcp_rows = vec![setting_row("MCP Servers", Some(mcp_note), mcp_control, None, stacked, &theme).into_any_element()];

        // ---- Agents
        let agent_rows = self.render_agent_rows(&theme, cx);
        let predictions = vec![self.render_setting(Setting::EditPredictions, window, cx)];
        let terminal_agents = vec![self.render_setting(Setting::ClaudeCode, window, cx), self.render_setting(Setting::GeminiCli, window, cx)];
        self.body()
            .child(section("Model", rows, &theme))
            .child(section("Agents", agent_rows, &theme))
            .child(section("Tools", mcp_rows, &theme))
            .child(section(Setting::EditPredictions.section(), predictions, &theme))
            .child(section(Setting::ClaudeCode.section(), terminal_agents, &theme))
    }

    /// The agents sessions can run: installed, or Install.
    fn render_agent_rows(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(store) = crate::agents::store::store(cx) else { return Vec::new() };
        let choices = store.read(cx).choices();
        choices
            .into_iter()
            .enumerate()
            .map(|(ix, choice)| {
                let action: AnyElement = if choice.id == crate::agents::store::KUANA_AGENT {
                    div().text_size(px(12.)).text_color(theme.text_dim).child("Built in").into_any_element()
                } else if choice.installing {
                    text_button(("agent-install", ix), "Installing…", false, cx).into_any_element()
                } else if !choice.installed || choice.update {
                    let id = choice.id.clone();
                    text_button(("agent-install", ix), if choice.installed { "Update" } else { "Install" }, true, cx)
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            if let Some(store) = crate::agents::store::store(cx) {
                                store.update(cx, |s, cx| s.install(&id, cx));
                            }
                        }))
                        .into_any_element()
                } else {
                    let id = choice.id.clone();
                    text_button(("agent-uninstall", ix), "Uninstall", false, cx)
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            if let Some(store) = crate::agents::store::store(cx) {
                                store.update(cx, |s, cx| s.uninstall(&id, cx));
                            }
                        }))
                        .into_any_element()
                };
                let description: SharedString = match &choice.error {
                    Some(error) => error.clone().into(),
                    None => choice.description.clone().into(),
                };
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .px_3()
                    .py_2()
                    .child(icon_el(Icon::Sparkle, px(15.), if choice.installed { theme.accent.into() } else { theme.text_dim.into() }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_color(theme.text).child(choice.name.clone()))
                            .child(div().text_size(px(12.)).text_color(if choice.error.is_some() { theme.error } else { theme.text_dim }).whitespace_normal().child(description)),
                    )
                    .child(action)
                    .into_any_element()
            })
            .collect()
    }

    /// The URL and typed model fields, saved a moment after typing stops.
    pub(super) fn commit_ai_fields(&mut self, cx: &mut Context<Self>) -> Vec<std::io::Result<()>> {
        let settings = Self::ai_settings(cx);
        let Some(provider) = settings.provider.clone() else { return Vec::new() };
        let mut results = Vec::new();
        let url = self.ai_url.read(cx).text(cx).trim().to_string();
        let saved_url = settings.providers.get(&provider).and_then(|p| p.url.clone()).unwrap_or_default();
        if url != saved_url {
            results.push(Settings::update("ai", |old| {
                let mut map = old.and_then(|v| v.as_object().cloned()).unwrap_or_default();
                let providers = map.entry("providers").or_insert_with(|| json!({}));
                if let Some(providers) = providers.as_object_mut() {
                    if url.is_empty() {
                        providers.remove(&provider);
                    } else {
                        providers.insert(provider.clone(), json!({ "url": url }));
                    }
                    if providers.is_empty() {
                        map.remove("providers");
                    }
                }
                Some(Value::Object(map))
            }));
            self.ai.models = None;
        }
        let model = self.ai_model.read(cx).text(cx).trim().to_string();
        if !model.is_empty() && settings.model.as_deref() != Some(model.as_str()) {
            results.push(Settings::update("ai", |old| {
                let mut map = old.and_then(|v| v.as_object().cloned()).unwrap_or_default();
                map.insert("model".into(), json!(model));
                Some(Value::Object(map))
            }));
        }
        results
    }

    /// The fields show what the settings say (on open, and after a reset).
    pub(super) fn fill_ai_fields(&mut self, cx: &mut Context<Self>) {
        let settings = Self::ai_settings(cx);
        let url = settings.provider.as_deref().and_then(|p| settings.providers.get(p)).and_then(|p| p.url.clone()).unwrap_or_default();
        set_field(&self.ai_url, &url, cx);
        set_field(&self.ai_model, settings.model.as_deref().unwrap_or_default(), cx);
    }
}

fn effort_label(effort: ai::Effort) -> &'static str {
    match effort {
        ai::Effort::Low => "Low",
        ai::Effort::Medium => "Medium",
        ai::Effort::High => "High",
        ai::Effort::Xhigh => "Extra high",
        ai::Effort::Max => "Max",
    }
}
