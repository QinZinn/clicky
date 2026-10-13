//! The settings screen: provider, voice, and general options.

use eframe::egui::{self, RichText, TextEdit};

use crate::app::{ClickyApp, ModelList, palette};
use crate::config::{ApiStyle, HotkeyChoice, Secret, SttProvider, TtsProvider};
use crate::engine::Command;
use crate::providers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    Provider,
    Voice,
    General,
}

impl SettingsTab {
    const ALL: [SettingsTab; 3] = [
        SettingsTab::Provider,
        SettingsTab::Voice,
        SettingsTab::General,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Provider => "AI model",
            SettingsTab::Voice => "Voice",
            SettingsTab::General => "General",
        }
    }
}

pub fn show(app: &mut ClickyApp, ui: &mut egui::Ui) {
    egui::Panel::bottom("settings_actions")
        .frame(
            egui::Frame::new()
                .fill(palette::PANEL)
                .inner_margin(egui::Margin::same(14)),
        )
        .show(ui, |ui| {
            if let Some((is_warning, message)) = app.settings_message.clone() {
                let color = if is_warning {
                    palette::DANGER
                } else {
                    palette::SUCCESS
                };
                ui.label(RichText::new(message).color(color));
            }
            ui.horizontal(|ui| {
                let save = ui
                    .add(
                        egui::Button::new(
                            RichText::new("Save").strong().color(egui::Color32::WHITE),
                        )
                        .fill(palette::ACCENT),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if save.clicked() {
                    app.save_settings();
                }
                if ui
                    .button("Cancel")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    app.cancel_settings();
                }
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(palette::BACKGROUND)
                .inner_margin(egui::Margin::same(14)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for tab in SettingsTab::ALL {
                    ui.selectable_value(&mut app.settings_tab, tab, tab.label());
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match app.settings_tab {
                    SettingsTab::Provider => provider_tab(app, ui),
                    SettingsTab::Voice => voice_tab(app, ui),
                    SettingsTab::General => general_tab(app, ui),
                });
        });
}

/// A labelled form field: small caption above the widget. The caption's id is
/// passed to `add_widget` so the main widget can be marked as labelled by it,
/// which is what screen readers (and UI tests) use to find it.
fn field(ui: &mut egui::Ui, caption: &str, add_widget: impl FnOnce(&mut egui::Ui, egui::Id)) {
    let label = ui.label(
        RichText::new(caption)
            .small()
            .strong()
            .color(palette::MUTED),
    );
    add_widget(ui, label.id);
    ui.add_space(4.0);
}

fn text_field(ui: &mut egui::Ui, value: &mut String, hint: &str, label_id: egui::Id) {
    ui.add(
        TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    )
    .labelled_by(label_id);
}

fn secret_field(ui: &mut egui::Ui, secret: &mut Secret, hint: &str, label_id: egui::Id) {
    ui.add(
        TextEdit::singleline(secret.expose_mut())
            .password(true)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    )
    .labelled_by(label_id);
}

fn note(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(palette::MUTED));
}

fn provider_tab(app: &mut ClickyApp, ui: &mut egui::Ui) {
    field(ui, "Provider", |ui, label| {
        let selected_name = providers::preset(&app.draft.active_provider)
            .map(|preset| preset.name)
            .unwrap_or("Unknown");
        egui::ComboBox::from_id_salt("provider_combo")
            .width(ui.available_width())
            .selected_text(selected_name)
            .show_ui(ui, |ui| {
                for preset in providers::PRESETS {
                    ui.selectable_value(
                        &mut app.draft.active_provider,
                        preset.id.to_string(),
                        preset.name,
                    );
                }
            })
            .response
            .labelled_by(label);
    });

    let active = app.draft.active_provider.clone();
    let Some(preset) = providers::preset(&active) else {
        return;
    };
    note(ui, preset.notes);
    ui.add_space(6.0);

    let mut refresh_clicked = false;
    {
        let config = app.draft.providers.entry(active.clone()).or_default();

        field(ui, "Base URL", |ui, label| {
            text_field(ui, &mut config.base_url, preset.base_url, label);
            if providers::sends_key_over_plain_http_to_remote_host(&config.base_url) {
                ui.label(
                    RichText::new(
                        "This address uses plain http://, so your API key would be sent unencrypted. \
                         Use https:// unless this server is on your own network.",
                    )
                    .small()
                    .color(palette::DANGER),
                );
            }
        });

        field(ui, "API key", |ui, label| {
            secret_field(
                ui,
                &mut config.api_key,
                if preset.needs_api_key {
                    "Paste your key"
                } else {
                    "Not needed for local servers"
                },
                label,
            );
            let using_environment = config.api_key.is_empty()
                && preset
                    .key_env_vars
                    .iter()
                    .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
            if using_environment {
                note(
                    ui,
                    format!(
                        "Using {} from the environment.",
                        preset.key_env_vars.join(" / ")
                    ),
                );
            } else if !preset.key_env_vars.is_empty() {
                note(
                    ui,
                    format!(
                        "Or set the {} environment variable.",
                        preset.key_env_vars[0]
                    ),
                );
            }
        });

        field(ui, "Model", |ui, label| {
            text_field(ui, &mut config.model, preset.default_model, label);
            if !preset.suggested_models.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for suggestion in preset.suggested_models {
                        if ui
                            .small_button(*suggestion)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            config.model = suggestion.to_string();
                        }
                    }
                });
            }

            ui.horizontal(|ui| {
                if ui
                    .button("Refresh models")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    refresh_clicked = true;
                }
                match app.models.get(&active) {
                    Some(ModelList::Loading) => {
                        ui.spinner();
                    }
                    Some(ModelList::Failed(message)) => {
                        ui.label(RichText::new(message).small().color(palette::DANGER));
                    }
                    Some(ModelList::Loaded(models)) => {
                        note(ui, format!("{} models", models.len()));
                    }
                    _ => {}
                }
            });

            if let Some(ModelList::Loaded(models)) = app.models.get(&active) {
                ui.add(
                    TextEdit::singleline(&mut app.model_filter)
                        .hint_text("Filter models…")
                        .desired_width(f32::INFINITY),
                );
                let filter = app.model_filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .id_salt("model_list")
                    .show(ui, |ui| {
                        for model in models
                            .iter()
                            .filter(|model| model.id.to_lowercase().contains(&filter))
                            .take(400)
                        {
                            let suffix = match model.accepts_images {
                                Some(true) => "   · images",
                                Some(false) => "   · text only",
                                None => "",
                            };
                            let selected = config.model == model.id;
                            if ui
                                .selectable_label(selected, format!("{}{suffix}", model.id))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                config.model = model.id.clone();
                                // Keep screenshots in step with what the model can read.
                                if let Some(accepts_images) = model.accepts_images {
                                    config.send_screenshots = accepts_images;
                                }
                            }
                        }
                    });
            }
        });

        ui.checkbox(
            &mut config.send_screenshots,
            "Send screenshots of my screen with each question",
        );
        note(
            ui,
            "Needed for 'what is this?' and pointing. Turn it off for text-only models. If a model \
             refuses images, Clicky retries without them automatically.",
        );
        ui.add_space(6.0);

        field(ui, "API style", |ui, label| {
            egui::ComboBox::from_id_salt("api_style_combo")
                .width(ui.available_width())
                .selected_text(config.api_style.label())
                .show_ui(ui, |ui| {
                    for style in ApiStyle::ALL {
                        ui.selectable_value(&mut config.api_style, style, style.label());
                    }
                })
                .response
                .labelled_by(label);
            match providers::resolve_wire_format(&active, config.api_style, &config.model) {
                Ok(format) => note(ui, format!("Will use: {}", format.path())),
                Err(error) => {
                    ui.label(
                        RichText::new(error.to_string())
                            .small()
                            .color(palette::DANGER),
                    );
                }
            }
        });
    }

    if refresh_clicked {
        app.request_model_refresh();
    }
}

fn voice_tab(app: &mut ClickyApp, ui: &mut egui::Ui) {
    let draft = &mut app.draft;

    ui.label(RichText::new("Speech input").strong());
    field(ui, "Speech service", |ui, label| {
        egui::ComboBox::from_id_salt("stt_combo")
            .width(ui.available_width())
            .selected_text(draft.stt.provider.label())
            .show_ui(ui, |ui| {
                for provider in SttProvider::ALL {
                    ui.selectable_value(&mut draft.stt.provider, provider, provider.label());
                }
            })
            .response
            .labelled_by(label);
    });
    match draft.stt.provider {
        SttProvider::AssemblyAi => {
            field(ui, "AssemblyAI API key", |ui, label| {
                secret_field(
                    ui,
                    &mut draft.stt.assemblyai_api_key,
                    "Paste your key",
                    label,
                );
                note(
                    ui,
                    "Or set ASSEMBLYAI_API_KEY. Words appear live while you talk.",
                );
            });
        }
        SttProvider::OpenAiCompatible => {
            field(ui, "Base URL", |ui, label| {
                text_field(
                    ui,
                    &mut draft.stt.whisper_base_url,
                    "https://api.openai.com/v1",
                    label,
                )
            });
            field(ui, "API key", |ui, label| {
                secret_field(ui, &mut draft.stt.whisper_api_key, "Paste your key", label)
            });
            field(ui, "Model", |ui, label| {
                text_field(ui, &mut draft.stt.whisper_model, "whisper-1", label)
            });
            note(
                ui,
                "Works with OpenAI, Groq and other servers that offer /audio/transcriptions.",
            );
        }
        SttProvider::Disabled => note(ui, "You can still type your questions."),
    }

    field(ui, "Push-to-talk shortcut", |ui, label| {
        egui::ComboBox::from_id_salt("hotkey_combo")
            .width(ui.available_width())
            .selected_text(draft.hotkey.label())
            .show_ui(ui, |ui| {
                for choice in HotkeyChoice::ALL {
                    ui.selectable_value(&mut draft.hotkey, choice, choice.label());
                }
            })
            .response
            .labelled_by(label);
        note(
            ui,
            "Works while another app is focused. On macOS you may need to allow Input Monitoring \
             for Clicky; on Linux it needs an X11 session.",
        );
    });

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Voice replies").strong());
    field(ui, "Voice reply service", |ui, label| {
        egui::ComboBox::from_id_salt("tts_combo")
            .width(ui.available_width())
            .selected_text(draft.tts.provider.label())
            .show_ui(ui, |ui| {
                for provider in TtsProvider::ALL {
                    ui.selectable_value(&mut draft.tts.provider, provider, provider.label());
                }
            })
            .response
            .labelled_by(label);
    });
    if draft.tts.provider == TtsProvider::ElevenLabs {
        field(ui, "ElevenLabs API key", |ui, label| {
            secret_field(
                ui,
                &mut draft.tts.elevenlabs_api_key,
                "Paste your key",
                label,
            );
            note(ui, "Or set ELEVENLABS_API_KEY.");
        });
        field(ui, "Voice ID", |ui, label| {
            text_field(ui, &mut draft.tts.voice_id, "21m00Tcm4TlvDq8ikWAM", label)
        });
        field(ui, "Model", |ui, label| {
            text_field(ui, &mut draft.tts.model_id, "eleven_flash_v2_5", label)
        });
    }
}

fn general_tab(app: &mut ClickyApp, ui: &mut egui::Ui) {
    field(ui, "Maximum reply length (tokens)", |ui, label| {
        ui.add(
            egui::DragValue::new(&mut app.draft.max_output_tokens)
                .range(64..=16_000)
                .speed(32),
        )
        .labelled_by(label);
        note(
            ui,
            "Reasoning models spend part of this on thinking. Raise it if replies come back empty.",
        );
    });
    field(ui, "Conversation memory (exchanges)", |ui, label| {
        ui.add(egui::DragValue::new(&mut app.draft.history_exchanges).range(1..=50))
            .labelled_by(label);
    });
    ui.checkbox(
        &mut app.draft.show_pointer_overlay,
        "Fly the blue cursor to things Clicky points at",
    );
    ui.add_space(8.0);

    if ui
        .button("Clear conversation")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        app.messages.clear();
        let _ = app.commands.send(Command::ClearConversation);
    }

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Privacy").strong());
    note(
        ui,
        "Clicky has no account, analytics or telemetry. Your questions and screenshots go only to \
         the AI provider you pick above (and your voice to the speech service, if you use one). \
         Keys are stored on this computer in the settings file.",
    );
    if let Some(path) = crate::config::Settings::default_path() {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!("Settings file: {}", path.display()))
                    .small()
                    .color(palette::MUTED),
            )
            .selectable(true),
        );
    }
}
