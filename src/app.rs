//! The main window.
//!
//! [`ClickyApp`] is built from plain parts (settings, a command channel, an
//! event receiver) so tests can drive it without a window, a real engine or
//! any hardware. [`ClickyApp::launch_parts`] assembles the real thing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, RichText, ViewportBuilder, ViewportId,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::AudioLevel;
use crate::config::{HotkeyChoice, Settings, SttProvider};
use crate::docs::DocSource;
use crate::engine::{self, Command, EngineDependencies, UiEvent, VoiceState};
use crate::input::{self, HotkeyEvent, SharedHotkeyChoice};
use crate::llm::ModelInfo;
use crate::overlay::{self, PointerFlight};
use crate::providers;
use crate::settings_ui;

pub mod palette {
    use super::Color32;

    pub const BACKGROUND: Color32 = Color32::from_rgb(0x0f, 0x11, 0x15);
    pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x1a, 0x21);
    pub const CARD: Color32 = Color32::from_rgb(0x1d, 0x22, 0x2b);
    pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe9, 0xef);
    pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa1);
    pub const ACCENT: Color32 = Color32::from_rgb(0x25, 0x63, 0xeb);
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x1d, 0x4e, 0xd8);
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0x60, 0xa5, 0xfa);
    pub const SUCCESS: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
    pub const DANGER: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    Notice,
    Error,
    /// The documentation pages an answer was based on.
    Sources,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatEntry {
    pub role: ChatRole,
    pub text: String,
    /// Pages to link to (only for [`ChatRole::Sources`]).
    pub links: Vec<DocSource>,
}

/// The state of the "Test search" button on the Docs settings tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DocsTestState {
    #[default]
    NotRun,
    Running,
    Finished(Result<String, String>),
}

/// Models fetched from a provider, per provider id.
#[derive(Clone, Debug, Default)]
pub enum ModelList {
    #[default]
    NotFetched,
    Loading,
    Loaded(Vec<ModelInfo>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Chat,
    Settings,
}

/// Everything the window needs from the outside.
pub struct AppParts {
    pub settings: Settings,
    /// Where Save writes. `None` disables persistence (used in tests).
    pub config_path: Option<PathBuf>,
    pub commands: UnboundedSender<Command>,
    pub events: mpsc::Receiver<UiEvent>,
    pub audio_level: AudioLevel,
    pub hotkey: SharedHotkeyChoice,
    pub hotkey_error: Option<String>,
}

pub struct ClickyApp {
    // --- state shared with settings_ui ---
    pub(crate) settings: Settings,
    pub(crate) draft: Settings,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) commands: UnboundedSender<Command>,
    pub(crate) models: HashMap<String, ModelList>,
    pub(crate) model_filter: String,
    pub(crate) settings_tab: settings_ui::SettingsTab,
    pub(crate) settings_message: Option<(bool, String)>,
    pub(crate) docs_test: DocsTestState,
    pub(crate) hotkey: SharedHotkeyChoice,
    pub(crate) hotkey_error: Option<String>,

    // --- chat state ---
    events: mpsc::Receiver<UiEvent>,
    audio_level: AudioLevel,
    pub(crate) screen: Screen,
    pub(crate) state: VoiceState,
    pub(crate) messages: Vec<ChatEntry>,
    streaming_reply: Option<String>,
    partial_transcript: String,
    input_text: String,
    holding_talk_button: bool,

    // --- pointer overlay ---
    flight: Option<PointerFlight>,
    flight_serial: u64,
    clock: Instant,

    /// Keeps the async runtime alive for the lifetime of the window.
    _runtime: Option<tokio::runtime::Runtime>,
}

const MAXIMUM_MESSAGES: usize = 200;

impl ClickyApp {
    pub fn from_parts(parts: AppParts) -> Self {
        let first_run_needs_setup = needs_setup(&parts.settings);
        let mut app = Self {
            draft: parts.settings.clone(),
            settings: parts.settings,
            config_path: parts.config_path,
            commands: parts.commands,
            models: HashMap::new(),
            model_filter: String::new(),
            settings_tab: settings_ui::SettingsTab::Provider,
            settings_message: None,
            docs_test: DocsTestState::NotRun,
            hotkey: parts.hotkey,
            hotkey_error: parts.hotkey_error,
            events: parts.events,
            audio_level: parts.audio_level,
            screen: Screen::Chat,
            state: VoiceState::Idle,
            messages: Vec::new(),
            streaming_reply: None,
            partial_transcript: String::new(),
            input_text: String::new(),
            holding_talk_button: false,
            flight: None,
            flight_serial: 0,
            clock: Instant::now(),
            _runtime: None,
        };
        if first_run_needs_setup {
            app.screen = Screen::Settings;
        }
        app
    }

    /// Builds the real app: engine, global shortcut, async runtime.
    pub fn launch(creation_context: &eframe::CreationContext<'_>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("clicky-async")
            .enable_all()
            .build()
            .expect("starting the async runtime");

        let settings = Settings::load();
        let (event_sender, event_receiver) = mpsc::channel();
        let context = creation_context.egui_ctx.clone();
        let event_sink: engine::EventSink = Arc::new({
            let context = context.clone();
            move |event| {
                let _ = event_sender.send(event);
                context.request_repaint();
            }
        });

        let audio_level = AudioLevel::default();
        let commands = engine::spawn_engine(
            runtime.handle(),
            settings.clone(),
            EngineDependencies::real(),
            event_sink,
            audio_level.clone(),
        );

        let hotkey = SharedHotkeyChoice::new(settings.hotkey);
        let hotkey_error = {
            let commands = commands.clone();
            let context = context.clone();
            input::spawn_hotkey_listener(hotkey.clone(), move |event| {
                let _ = commands.send(match event {
                    HotkeyEvent::Pressed => Command::StartListening,
                    HotkeyEvent::Released => Command::StopListening,
                });
                context.request_repaint();
            })
            .err()
        };

        apply_theme(&context);
        let mut app = Self::from_parts(AppParts {
            settings,
            config_path: Settings::default_path(),
            commands,
            events: event_receiver,
            audio_level,
            hotkey,
            hotkey_error,
        });
        app._runtime = Some(runtime);
        app
    }

    // -----------------------------------------------------------------
    // Read-only views of the state (for tests and embedding)
    // -----------------------------------------------------------------

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn voice_state(&self) -> VoiceState {
        self.state
    }

    pub fn messages(&self) -> &[ChatEntry] {
        &self.messages
    }

    /// The settings currently in effect.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The settings being edited on the settings screen (not yet saved).
    pub fn draft(&self) -> &Settings {
        &self.draft
    }

    /// Lets tests set up a draft state that would take many clicks to reach.
    #[doc(hidden)]
    pub fn draft_for_test(&mut self) -> &mut Settings {
        &mut self.draft
    }

    // -----------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------

    fn now_seconds(&self) -> f32 {
        self.clock.elapsed().as_secs_f32()
    }

    /// Applies everything the engine has reported since the last frame.
    pub fn process_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(event);
        }
    }

    fn push_message(&mut self, role: ChatRole, text: String) {
        self.push_entry(ChatEntry {
            role,
            text,
            links: Vec::new(),
        });
    }

    fn push_entry(&mut self, entry: ChatEntry) {
        self.messages.push(entry);
        if self.messages.len() > MAXIMUM_MESSAGES {
            let excess = self.messages.len() - MAXIMUM_MESSAGES;
            self.messages.drain(..excess);
        }
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::StateChanged(state) => self.state = state,
            UiEvent::PartialTranscript(text) => self.partial_transcript = text,
            UiEvent::UserMessage(text) => {
                self.streaming_reply = None;
                self.push_message(ChatRole::User, text);
            }
            UiEvent::AssistantStreaming(text) => self.streaming_reply = Some(text),
            UiEvent::AssistantMessage(text) => {
                self.streaming_reply = None;
                self.push_message(ChatRole::Assistant, text);
            }
            UiEvent::Notice(text) => self.push_message(ChatRole::Notice, text),
            UiEvent::Error(text) => {
                self.streaming_reply = None;
                self.push_message(ChatRole::Error, text);
            }
            UiEvent::PointAt(request) => {
                if self.settings.show_pointer_overlay {
                    self.flight_serial += 1;
                    self.flight = Some(PointerFlight::new(
                        &request,
                        input::cursor_position(),
                        self.now_seconds(),
                    ));
                }
            }
            UiEvent::Sources { query, sources } => self.push_entry(ChatEntry {
                role: ChatRole::Sources,
                text: format!("Looked up the docs for \"{query}\":"),
                links: sources,
            }),
            UiEvent::DocsSearchTested(result) => {
                self.docs_test = DocsTestState::Finished(result);
            }
            UiEvent::Models {
                provider_id,
                result,
            } => {
                let list = match result {
                    Ok(models) => ModelList::Loaded(models),
                    Err(message) => ModelList::Failed(message),
                };
                self.models.insert(provider_id, list);
            }
        }
    }

    // -----------------------------------------------------------------
    // Settings actions (used by settings_ui)
    // -----------------------------------------------------------------

    pub(crate) fn open_settings(&mut self) {
        self.draft = self.settings.clone();
        self.settings_message = None;
        self.screen = Screen::Settings;
    }

    pub(crate) fn cancel_settings(&mut self) {
        self.draft = self.settings.clone();
        self.settings_message = None;
        self.screen = Screen::Chat;
    }

    pub(crate) fn save_settings(&mut self) {
        self.draft.fill_missing_providers();
        self.settings = self.draft.clone();
        self.hotkey.set(self.settings.hotkey);
        let _ = self
            .commands
            .send(Command::UpdateSettings(Box::new(self.settings.clone())));

        let saved = match &self.config_path {
            Some(path) => self
                .settings
                .save_to(path)
                .map_err(|error| error.to_string()),
            None => Ok(()),
        };
        match saved {
            Ok(()) => {
                self.settings_message = None;
                self.screen = Screen::Chat;
            }
            Err(error) => {
                // Still applied for this session; tell the user it won't persist.
                self.settings_message = Some((
                    true,
                    format!("Applied, but could not save to disk: {error}"),
                ));
            }
        }
    }

    /// The quick "Look up docs" switch in the chat window. Applies at once and is
    /// remembered, unlike the other settings, which wait for Save.
    pub(crate) fn set_docs_enabled(&mut self, enabled: bool) {
        self.settings.docs.enabled = enabled;
        self.draft.docs.enabled = enabled;
        let _ = self
            .commands
            .send(Command::UpdateSettings(Box::new(self.settings.clone())));
        if let Some(path) = &self.config_path
            && let Err(error) = self.settings.save_to(path)
        {
            self.push_message(
                ChatRole::Notice,
                format!("This choice is active now but could not be saved: {error}"),
            );
        }
    }

    /// The "Test search" button: one real search with the settings as edited.
    pub(crate) fn request_docs_search_test(&mut self) {
        self.docs_test = DocsTestState::Running;
        let _ = self
            .commands
            .send(Command::TestDocsSearch(Box::new(self.draft.clone())));
    }

    pub(crate) fn request_model_refresh(&mut self) {
        let provider_id = self.draft.active_provider.clone();
        let config = self.draft.active_provider_config();
        let environment_variables = providers::preset(&provider_id)
            .map(|preset| preset.key_env_vars)
            .unwrap_or(&[]);
        let api_key = crate::config::resolve_secret(&config.api_key, environment_variables);
        self.models.insert(provider_id.clone(), ModelList::Loading);
        let _ = self.commands.send(Command::RefreshModels {
            provider_id,
            base_url: config.base_url,
            api_key,
        });
    }

    // -----------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------

    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        match self.screen {
            Screen::Chat => {
                self.input_bar(ui);
                self.conversation(ui);
            }
            Screen::Settings => settings_ui::show(self, ui),
        }
        self.show_pointer_overlay(ui.ctx());
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(palette::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Clicky")
                            .size(20.0)
                            .strong()
                            .color(palette::TEXT),
                    );
                    ui.add_space(6.0);
                    let (label, color) = match self.state {
                        VoiceState::Idle => ("Ready", palette::MUTED),
                        VoiceState::Listening => ("Listening", palette::DANGER),
                        VoiceState::Transcribing => ("Transcribing", palette::ACCENT_TEXT),
                        VoiceState::LookingUpDocs => ("Looking up docs", palette::ACCENT_TEXT),
                        VoiceState::Thinking => ("Thinking", palette::ACCENT_TEXT),
                        VoiceState::Speaking => ("Speaking", palette::SUCCESS),
                    };
                    // Drawn, not a "●" character: the bundled font has no such glyph.
                    let (dot_area, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot_area.center(), 4.0, color);
                    ui.label(RichText::new(label).color(color).size(13.0));

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let in_settings = self.screen == Screen::Settings;
                        let button_label = if in_settings {
                            "Back to chat"
                        } else {
                            "Settings"
                        };
                        if ui.button(button_label).clicked() {
                            if in_settings {
                                self.cancel_settings();
                            } else {
                                self.open_settings();
                            }
                        }
                    });
                });
            });
    }

    fn input_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("input_bar")
            .frame(
                egui::Frame::new()
                    .fill(palette::PANEL)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ui, |ui| {
                if !self.partial_transcript.is_empty() {
                    ui.label(
                        RichText::new(format!("“{}”", self.partial_transcript))
                            .italics()
                            .color(palette::ACCENT_TEXT),
                    );
                    ui.add_space(6.0);
                }

                let busy = !matches!(self.state, VoiceState::Idle | VoiceState::Listening);
                ui.horizontal(|ui| {
                    let send_width = 64.0;
                    let text_response = ui.add_sized(
                        [ui.available_width() - send_width - 8.0, 30.0],
                        egui::TextEdit::singleline(&mut self.input_text)
                            .vertical_align(Align::Center)
                            .hint_text("Type a question…")
                            .id(egui::Id::new("message_input")),
                    );
                    let pressed_enter = text_response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let send_clicked = ui
                        .add_enabled(
                            !self.input_text.trim().is_empty(),
                            egui::Button::new("Send")
                                .min_size(egui::vec2(send_width, 30.0))
                                .fill(palette::ACCENT),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked();
                    if (pressed_enter || send_clicked) && !self.input_text.trim().is_empty() {
                        let text = std::mem::take(&mut self.input_text);
                        let _ = self.commands.send(Command::SendText(text));
                        text_response.request_focus();
                    }
                });
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    self.talk_button(ui);
                    if busy
                        && ui
                            .button("Stop")
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                    {
                        let _ = self.commands.send(Command::Cancel);
                    }
                });

                ui.add_space(4.0);
                let mut docs_enabled = self.settings.docs.enabled;
                let docs_toggle = ui
                    .checkbox(&mut docs_enabled, "Look up docs")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(
                        "Before answering, search the web for the software's real documentation and \
                         read it, instead of relying on the model's memory. Sends a short search query \
                         to your chosen search service. Set it up in Settings > Docs.",
                    );
                if docs_toggle.changed() {
                    self.set_docs_enabled(docs_enabled);
                }

                if self.state == VoiceState::Listening {
                    ui.add_space(6.0);
                    ui.add(
                        egui::ProgressBar::new(self.audio_level.get())
                            .desired_height(8.0)
                            .fill(palette::DANGER),
                    );
                }

                let hint = match self.settings.hotkey {
                    HotkeyChoice::Disabled => "Hold the button to talk.".to_string(),
                    choice => format!(
                        "Hold the button, or {}, to talk to Clicky from any app.",
                        choice.label().to_lowercase()
                    ),
                };
                ui.add_space(4.0);
                ui.label(RichText::new(hint).small().color(palette::MUTED));
                if let Some(error) = &self.hotkey_error {
                    ui.label(RichText::new(error).small().color(palette::DANGER));
                }
            });
    }

    fn talk_button(&mut self, ui: &mut egui::Ui) {
        let speech_available = self.settings.stt.provider != SttProvider::Disabled;
        let listening = self.state == VoiceState::Listening;
        let label = if !speech_available {
            "Speech input is off"
        } else if listening {
            "Listening… release to send"
        } else {
            "Hold to talk"
        };
        let fill = if listening {
            palette::DANGER
        } else {
            palette::ACCENT
        };
        let response = ui
            .add_enabled(
                speech_available,
                egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                    .fill(fill)
                    .min_size(egui::vec2(210.0, 36.0)),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if !speech_available {
            response.clone().on_disabled_hover_text(
                "Pick a speech service in Settings, or type your question.",
            );
        }

        // Press-and-hold: start when the press lands on the button, stop when
        // the mouse button is released anywhere.
        //
        // This deliberately does not ask the widget "are you still pressed?":
        // egui stops treating a press as a click candidate after 0.8 seconds,
        // which would end every recording that long, and it would also end it
        // if the pointer drifted off the button while talking.
        let primary_button_is_down = ui.input(|input| input.pointer.primary_down());
        // The explicit focus-lost event, not `input.focused`: some setups (a bare X11
        // session without a window manager) never report focus at all, and that must
        // not be mistaken for "the user switched away".
        let window_lost_focus = ui.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::WindowFocused(false)))
        });
        if !self.holding_talk_button {
            if response.is_pointer_button_down_on() {
                self.holding_talk_button = true;
                let _ = self.commands.send(Command::StartListening);
            }
        } else if !primary_button_is_down || window_lost_focus {
            // Losing focus counts as a release: the release event would go to
            // another window and the microphone would stay open.
            self.holding_talk_button = false;
            let _ = self.commands.send(Command::StopListening);
        }
    }

    fn conversation(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette::BACKGROUND)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.messages.is_empty() && self.streaming_reply.is_none() {
                            self.empty_state(ui);
                        }
                        for entry in &self.messages {
                            message_bubble(ui, entry.role, &entry.text, &entry.links);
                            ui.add_space(8.0);
                        }
                        if let Some(text) = &self.streaming_reply {
                            message_bubble(ui, ChatRole::Assistant, &format!("{text} …"), &[]);
                        } else if matches!(
                            self.state,
                            VoiceState::Thinking | VoiceState::Transcribing
                        ) {
                            ui.label(RichText::new("…").color(palette::MUTED));
                        }
                    });
            });
    }

    fn empty_state(&self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Ask me about anything on your screen").size(18.0).color(palette::TEXT));
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "I can see your screen, answer out loud, and fly a blue cursor to the thing you need.",
                )
                .color(palette::MUTED),
            );
        });
    }

    fn show_pointer_overlay(&mut self, ctx: &egui::Context) {
        let Some(flight) = self.flight.as_mut() else {
            return;
        };
        let monitor_rect = flight.monitor_rect();
        let now = self.clock.elapsed().as_secs_f32();
        let cursor = input::cursor_position();

        // A fresh window per flight, so it always opens on the right monitor.
        let viewport_id = ViewportId::from_hash_of(("clicky-pointer", self.flight_serial));
        let builder = ViewportBuilder::default()
            .with_title("Clicky pointer")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false)
            .with_position([monitor_rect.x, monitor_rect.y])
            .with_inner_size([monitor_rect.width, monitor_rect.height]);

        let finished = ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
            let frame = flight.frame(now, cursor);
            overlay::paint(ui.painter(), ui.max_rect().min, &frame);
            frame.finished
        });

        if finished {
            self.flight = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }
}

fn needs_setup(settings: &Settings) -> bool {
    let needs_key =
        providers::preset(&settings.active_provider).is_some_and(|preset| preset.needs_api_key);
    needs_key && settings.active_api_key().is_empty()
}

fn message_bubble(ui: &mut egui::Ui, role: ChatRole, text: &str, links: &[DocSource]) {
    let (title, fill, text_color) = match role {
        ChatRole::User => ("You", palette::ACCENT, Color32::WHITE),
        ChatRole::Assistant => ("Clicky", palette::CARD, palette::TEXT),
        ChatRole::Notice => ("", Color32::TRANSPARENT, palette::MUTED),
        ChatRole::Sources => ("Docs", Color32::from_rgb(0x14, 0x24, 0x3d), palette::TEXT),
        ChatRole::Error => (
            "Problem",
            Color32::from_rgb(0x3b, 0x1a, 0x1d),
            Color32::from_rgb(0xfc, 0xa5, 0xa5),
        ),
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(RichText::new(title).small().strong().color(match role {
                    ChatRole::User => Color32::from_rgb(0xdb, 0xea, 0xfe),
                    ChatRole::Error => Color32::from_rgb(0xf8, 0x71, 0x71),
                    _ => palette::ACCENT_TEXT,
                }));
            }
            let mut rich = RichText::new(text).color(text_color);
            if role == ChatRole::Notice {
                rich = rich.italics().small();
            }
            ui.add(egui::Label::new(rich).selectable(true));
            for link in links {
                let shown: String = if link.title.chars().count() > 80 {
                    format!("{}…", link.title.chars().take(80).collect::<String>())
                } else {
                    link.title.clone()
                };
                ui.hyperlink_to(shown, &link.url)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(&link.url);
            }
        });
}

/// Dark theme with the blue accent from the original design.
pub fn apply_theme(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = palette::BACKGROUND;
    visuals.window_fill = palette::PANEL;
    // Text inputs fill with `extreme_bg_color`: darker than the panels, with a
    // visible outline, so fields read as fields.
    visuals.extreme_bg_color = Color32::from_rgb(0x0a, 0x0c, 0x10);
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(0x2a, 0x31, 0x40));
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette::ACCENT_TEXT);
    visuals.faint_bg_color = palette::CARD;
    visuals.selection.bg_fill = palette::ACCENT;
    visuals.hyperlink_color = palette::ACCENT_TEXT;
    visuals.widgets.inactive.bg_fill = palette::CARD;
    visuals.widgets.inactive.weak_bg_fill = palette::CARD;
    visuals.widgets.hovered.weak_bg_fill = palette::ACCENT_HOVER;
    visuals.widgets.hovered.bg_fill = palette::ACCENT_HOVER;
    visuals.override_text_color = None;
    context.set_visuals(visuals);
    context.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
    });
}

impl eframe::App for ClickyApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();
        // Keep the level meter and spinner moving while something is happening.
        if self.state != VoiceState::Idle {
            context.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    /// The pointer overlay window is transparent, so the default (opaque
    /// grey) clear colour must not be used. Every other window paints its own
    /// background.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}
