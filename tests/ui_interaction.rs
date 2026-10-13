//! Headless tests of the window: real egui widgets, driven like a user would
//! (click, type, press and hold), with the engine replaced by a channel we
//! can read from.

use std::path::PathBuf;

use clicky::app::{AppParts, ChatRole, ClickyApp, Screen};
use clicky::audio::AudioLevel;
use clicky::config::{ApiStyle, Secret, Settings, SttProvider};
use clicky::docs::DocSource;
use clicky::engine::{Command, UiEvent, VoiceState};
use clicky::input::SharedHotkeyChoice;
use clicky::llm::ModelInfo;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use tokio::sync::mpsc::UnboundedReceiver;

struct Fixture {
    harness: Harness<'static, ClickyApp>,
    commands: UnboundedReceiver<Command>,
    events: std::sync::mpsc::Sender<UiEvent>,
}

fn fixture(settings: Settings, config_path: Option<PathBuf>) -> Fixture {
    let (command_sender, commands) = tokio::sync::mpsc::unbounded_channel();
    let (events, event_receiver) = std::sync::mpsc::channel();
    let hotkey = SharedHotkeyChoice::new(settings.hotkey);
    let app = ClickyApp::from_parts(AppParts {
        settings,
        config_path,
        commands: command_sender,
        events: event_receiver,
        audio_level: AudioLevel::default(),
        hotkey,
        hotkey_error: None,
    });
    let mut harness = Harness::builder()
        .with_size(egui::vec2(460.0, 740.0))
        .build_ui_state(
            |ui, app: &mut ClickyApp| {
                app.process_events();
                app.show(ui);
            },
            app,
        );
    harness.run();
    Fixture {
        harness,
        commands,
        events,
    }
}

/// A provider that is fully configured, so the app starts on the chat screen.
fn configured_settings() -> Settings {
    let mut settings = Settings::default();
    settings.active_provider = "custom".into();
    settings.providers.get_mut("custom").unwrap().model = "some-model".into();
    settings.stt.provider = SttProvider::Disabled;
    settings
}

fn temporary_config_path(name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("clicky-ui-test-{}-{name}", std::process::id()))
        .join("config.json")
}

impl Fixture {
    fn drain_commands(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        while let Ok(command) = self.commands.try_recv() {
            commands.push(command);
        }
        commands
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.settle();
    }

    fn type_into(&mut self, label: &str, text: &str) {
        let field = self.harness.get_by_label(label);
        field.focus();
        field.type_text(text);
        self.settle();
    }

    /// Runs a few frames. `Harness::run` would refuse to finish while a spinner is
    /// on screen (it asks for repaints forever), and a few frames is all a click needs.
    fn settle(&mut self) {
        self.harness.run_steps(4);
    }

    fn app(&self) -> &ClickyApp {
        self.harness.state()
    }
}

fn anthropic_key_in_environment() -> bool {
    std::env::var("ANTHROPIC_API_KEY").is_ok_and(|value| !value.trim().is_empty())
}

#[test]
fn first_run_opens_settings_and_saving_a_key_persists_it_and_applies_it() {
    if anthropic_key_in_environment() {
        eprintln!("skipping: ANTHROPIC_API_KEY is set, so the app would not start in setup");
        return;
    }
    let path = temporary_config_path("first-run");
    let mut fixture = fixture(Settings::default(), Some(path.clone()));
    assert_eq!(
        fixture.app().screen(),
        Screen::Settings,
        "no key yet, so it should ask for one"
    );

    fixture.type_into("API key", "sk-ant-test-123");
    fixture.click("Save");

    assert_eq!(fixture.app().screen(), Screen::Chat);
    let commands = fixture.drain_commands();
    let applied = commands
        .iter()
        .find_map(|command| match command {
            Command::UpdateSettings(settings) => Some(settings),
            _ => None,
        })
        .expect("saving must hand the new settings to the engine");
    assert_eq!(applied.active_api_key(), "sk-ant-test-123");

    let saved = Settings::load_from(&path);
    assert_eq!(saved.active_api_key(), "sk-ant-test-123");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn cancel_discards_unsaved_edits() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    assert_eq!(fixture.app().screen(), Screen::Settings);

    fixture.type_into("API key", "typed-but-never-saved");
    assert_eq!(
        fixture.app().draft().providers["custom"].api_key.expose(),
        "typed-but-never-saved"
    );
    fixture.click("Cancel");

    assert_eq!(fixture.app().screen(), Screen::Chat);
    assert!(
        fixture.app().settings().providers["custom"]
            .api_key
            .is_empty()
    );
    assert!(
        fixture.app().draft().providers["custom"].api_key.is_empty(),
        "the draft resets too"
    );
    assert!(
        fixture
            .drain_commands()
            .iter()
            .all(|command| !matches!(command, Command::UpdateSettings(_))),
        "cancel must not change the engine's settings"
    );
}

#[test]
fn a_save_that_cannot_reach_the_disk_still_applies_and_says_so() {
    // The "directory" is a regular file, so creating the config file fails.
    let blocker =
        std::env::temp_dir().join(format!("clicky-ui-test-blocker-{}", std::process::id()));
    std::fs::write(&blocker, "not a directory").unwrap();
    let mut fixture = fixture(configured_settings(), Some(blocker.join("config.json")));

    fixture.click("Settings");
    fixture.click("Save");

    assert_eq!(
        fixture.app().screen(),
        Screen::Settings,
        "stay so the user sees the message"
    );
    fixture
        .harness
        .get_by_label_contains("could not save to disk");
    assert!(
        fixture
            .drain_commands()
            .iter()
            .any(|command| matches!(command, Command::UpdateSettings(_)))
    );
    let _ = std::fs::remove_file(blocker);
}

#[test]
fn choosing_another_provider_shows_its_defaults() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    fixture.click("Provider");
    fixture.click("OpenRouter");

    assert_eq!(fixture.app().draft().active_provider, "openrouter");
    let base_url = fixture.harness.get_by_label("Base URL");
    assert_eq!(
        base_url.accesskit_node().value().as_deref(),
        Some("https://openrouter.ai/api/v1")
    );
    let model = fixture.harness.get_by_label("Model");
    assert_eq!(
        model.accesskit_node().value().as_deref(),
        Some("anthropic/claude-sonnet-4.6")
    );
}

#[test]
fn zen_shows_which_endpoint_a_model_will_use_and_explains_unsupported_ones() {
    let mut settings = configured_settings();
    settings.active_provider = "opencode-zen".into();
    settings.providers.get_mut("opencode-zen").unwrap().api_key = Secret::new("zen-key");
    let mut fixture = fixture(settings, None);
    fixture.click("Settings");

    // claude-* models go to Zen's messages endpoint.
    fixture.harness.get_by_label_contains("Will use: messages");

    // (type_text appends to the field, so set the model on the draft directly.)
    fixture
        .harness
        .state_mut()
        .draft_for_test()
        .providers
        .get_mut("opencode-zen")
        .unwrap()
        .model = "gemini-3.5-flash".into();
    fixture.settle();
    fixture.harness.get_by_label_contains("Google's native API");

    // A manual override wins over auto-detection.
    fixture
        .harness
        .state_mut()
        .draft_for_test()
        .providers
        .get_mut("opencode-zen")
        .unwrap()
        .api_style = ApiStyle::OpenAiChat;
    fixture.settle();
    fixture
        .harness
        .get_by_label_contains("Will use: chat/completions");
}

#[test]
fn refreshing_models_asks_the_engine_and_picking_a_model_follows_its_image_support() {
    let mut settings = configured_settings();
    settings.active_provider = "openrouter".into();
    settings.providers.get_mut("openrouter").unwrap().api_key = Secret::new("or-key");
    let mut fixture = fixture(settings, None);
    fixture.click("Settings");
    fixture.drain_commands();

    fixture.click("Refresh models");
    let commands = fixture.drain_commands();
    match &commands[..] {
        [
            Command::RefreshModels {
                provider_id,
                base_url,
                api_key,
            },
        ] => {
            assert_eq!(provider_id, "openrouter");
            assert_eq!(base_url, "https://openrouter.ai/api/v1");
            assert_eq!(api_key, "or-key");
        }
        other => panic!("expected one RefreshModels, got {other:?}"),
    }

    fixture
        .events
        .send(UiEvent::Models {
            provider_id: "openrouter".into(),
            result: Ok(vec![
                ModelInfo {
                    id: "acme/text-only".into(),
                    accepts_images: Some(false),
                },
                ModelInfo {
                    id: "acme/vision".into(),
                    accepts_images: Some(true),
                },
            ]),
        })
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label_contains("2 models");

    fixture.click("acme/text-only   · text only");
    let config = &fixture.app().draft().providers["openrouter"];
    assert_eq!(config.model, "acme/text-only");
    assert!(
        !config.send_screenshots,
        "a text-only model must switch screenshots off"
    );

    fixture.click("acme/vision   · images");
    let config = &fixture.app().draft().providers["openrouter"];
    assert_eq!(config.model, "acme/vision");
    assert!(config.send_screenshots);
}

#[test]
fn a_failed_model_refresh_shows_the_reason() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    fixture
        .events
        .send(UiEvent::Models {
            provider_id: "custom".into(),
            result: Err("connection refused".into()),
        })
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label_contains("connection refused");
}

#[test]
fn typing_a_question_and_pressing_enter_sends_it_once() {
    let mut fixture = fixture(configured_settings(), None);
    assert_eq!(fixture.app().screen(), Screen::Chat);

    let input = fixture.harness.get_by_role(Role::TextInput);
    input.focus();
    input.type_text("where is the save button?");
    fixture.settle();
    fixture.harness.key_press(egui::Key::Enter);
    fixture.settle();

    let commands = fixture.drain_commands();
    assert!(
        matches!(&commands[..], [Command::SendText(text)] if text == "where is the save button?"),
        "{commands:?}"
    );
    // The box was cleared, so another Enter sends nothing.
    fixture.harness.key_press(egui::Key::Enter);
    fixture.settle();
    assert!(fixture.drain_commands().is_empty());
}

#[test]
fn engine_events_become_chat_bubbles() {
    let mut fixture = fixture(configured_settings(), None);
    for event in [
        UiEvent::UserMessage("what is this?".into()),
        UiEvent::StateChanged(VoiceState::Thinking),
        UiEvent::AssistantStreaming("it looks like".into()),
    ] {
        fixture.events.send(event).unwrap();
    }
    fixture.settle();
    fixture.harness.get_by_label("what is this?");
    fixture.harness.get_by_label_contains("it looks like");
    assert_eq!(fixture.app().voice_state(), VoiceState::Thinking);
    // While busy there is a way to stop.
    fixture.harness.get_by_label("Stop");

    for event in [
        UiEvent::AssistantMessage("a login form.".into()),
        UiEvent::Notice("Answering without looking at your screen.".into()),
        UiEvent::Error("the provider returned HTTP 401: Invalid API key".into()),
        UiEvent::StateChanged(VoiceState::Idle),
    ] {
        fixture.events.send(event).unwrap();
    }
    fixture.settle();
    fixture.harness.get_by_label("a login form.");
    fixture
        .harness
        .get_by_label("Answering without looking at your screen.");
    fixture.harness.get_by_label_contains("Invalid API key");
    fixture.harness.get_by_label("Problem");

    let roles: Vec<ChatRole> = fixture
        .app()
        .messages()
        .iter()
        .map(|entry| entry.role)
        .collect();
    assert_eq!(
        roles,
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Notice,
            ChatRole::Error
        ]
    );
    // Streaming text was replaced by the final message, not duplicated.
    assert!(
        fixture
            .harness
            .query_by_label_contains("it looks like")
            .is_none()
    );
    assert!(fixture.harness.query_by_label("Stop").is_none());
}

fn pointer_button(fixture: &mut Fixture, position: egui::Pos2, pressed: bool) {
    fixture.harness.event(egui::Event::PointerButton {
        pos: position,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    });
    fixture.settle();
}

fn speech_enabled_fixture() -> Fixture {
    let mut settings = configured_settings();
    settings.stt.provider = SttProvider::AssemblyAi;
    fixture(settings, None)
}

#[test]
fn hold_to_talk_starts_on_press_and_stops_on_release() {
    let mut fixture = speech_enabled_fixture();
    let button_centre = fixture.harness.get_by_label("Hold to talk").rect().center();
    fixture
        .harness
        .event(egui::Event::PointerMoved(button_centre));
    fixture.settle();

    pointer_button(&mut fixture, button_centre, true);
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StartListening]
    ));

    pointer_button(&mut fixture, button_centre, false);
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StopListening]
    ));
}

#[test]
fn a_long_hold_keeps_recording_for_as_long_as_the_button_is_down() {
    // Regression: egui forgets a press after ~0.8 s, which used to end every
    // recording that long. The harness advances a quarter second per frame.
    let mut fixture = speech_enabled_fixture();
    let button_centre = fixture.harness.get_by_label("Hold to talk").rect().center();
    fixture
        .harness
        .event(egui::Event::PointerMoved(button_centre));
    fixture.settle();
    pointer_button(&mut fixture, button_centre, true);
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StartListening]
    ));

    fixture.harness.run_steps(40); // about ten seconds of holding
    let during_hold = fixture.drain_commands();
    assert!(
        during_hold.is_empty(),
        "recording stopped while still held: {during_hold:?}"
    );

    pointer_button(&mut fixture, button_centre, false);
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StopListening]
    ));
}

#[test]
fn sliding_off_the_button_while_holding_does_not_end_the_recording() {
    let mut fixture = speech_enabled_fixture();
    let button_centre = fixture.harness.get_by_label("Hold to talk").rect().center();
    fixture
        .harness
        .event(egui::Event::PointerMoved(button_centre));
    fixture.settle();
    pointer_button(&mut fixture, button_centre, true);
    fixture.drain_commands();

    let far_away = egui::pos2(button_centre.x + 200.0, button_centre.y - 400.0);
    fixture.harness.event(egui::Event::PointerMoved(far_away));
    fixture.harness.run_steps(8);
    assert!(
        fixture.drain_commands().is_empty(),
        "drifting off the button must not stop recording"
    );

    // Releasing anywhere ends it.
    pointer_button(&mut fixture, far_away, false);
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StopListening]
    ));
}

#[test]
fn losing_window_focus_while_holding_stops_the_recording() {
    let mut fixture = speech_enabled_fixture();
    let button_centre = fixture.harness.get_by_label("Hold to talk").rect().center();
    fixture
        .harness
        .event(egui::Event::PointerMoved(button_centre));
    fixture.settle();
    pointer_button(&mut fixture, button_centre, true);
    fixture.drain_commands();

    // The release would be delivered to another window; don't leave the mic open.
    fixture.harness.event(egui::Event::WindowFocused(false));
    fixture.settle();
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::StopListening]
    ));
}

#[test]
fn the_talk_button_is_disabled_and_explains_itself_when_speech_input_is_off() {
    let mut fixture = fixture(configured_settings(), None);
    let button = fixture.harness.get_by_label("Speech input is off");
    assert!(button.accesskit_node().is_disabled());

    let centre = button.rect().center();
    fixture.harness.event(egui::Event::PointerMoved(centre));
    fixture.harness.event(egui::Event::PointerButton {
        pos: centre,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    fixture.settle();
    assert!(
        fixture.drain_commands().is_empty(),
        "a disabled button must not start recording"
    );
}

#[test]
fn clearing_the_conversation_empties_the_chat_and_tells_the_engine() {
    let mut fixture = fixture(configured_settings(), None);
    fixture
        .events
        .send(UiEvent::UserMessage("remember this".into()))
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label("remember this");

    fixture.click("Settings");
    fixture.click("General");
    fixture.drain_commands();
    fixture.click("Clear conversation");

    assert!(fixture.app().messages().is_empty());
    assert!(matches!(
        &fixture.drain_commands()[..],
        [Command::ClearConversation]
    ));
}

#[test]
fn the_voice_tab_shows_only_the_fields_for_the_chosen_service() {
    let mut settings = configured_settings();
    settings.stt.provider = SttProvider::AssemblyAi;
    let mut fixture = fixture(settings, None);
    fixture.click("Settings");
    fixture.click("Voice");

    fixture.harness.get_by_label("AssemblyAI API key");
    assert!(
        fixture.harness.query_by_label("Voice ID").is_none(),
        "ElevenLabs is off by default"
    );

    fixture.click("Speech service");
    fixture.click("OpenAI-compatible (Whisper upload)");
    assert!(
        fixture
            .harness
            .query_by_label("AssemblyAI API key")
            .is_none()
    );
    assert_eq!(
        fixture.app().draft().stt.provider,
        SttProvider::OpenAiCompatible
    );
}

#[test]
fn an_unencrypted_remote_address_gets_a_warning_but_local_ones_do_not() {
    let mut settings = configured_settings();
    settings.providers.get_mut("custom").unwrap().base_url = "http://api.example.com/v1".into();
    let mut fixture = fixture(settings, None);
    fixture.click("Settings");
    fixture.harness.get_by_label_contains("sent unencrypted");

    fixture
        .harness
        .state_mut()
        .draft_for_test()
        .providers
        .get_mut("custom")
        .unwrap()
        .base_url = "http://localhost:8000/v1".into();
    fixture.settle();
    assert!(
        fixture
            .harness
            .query_by_label_contains("sent unencrypted")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Looking up documentation
// ---------------------------------------------------------------------------

#[test]
fn the_look_up_docs_switch_applies_at_once_and_is_remembered() {
    let path = temporary_config_path("docs-toggle");
    let mut fixture = fixture(configured_settings(), Some(path.clone()));
    assert!(!fixture.app().settings().docs.enabled, "off by default");

    fixture.click("Look up docs");

    assert!(fixture.app().settings().docs.enabled);
    assert!(
        fixture.app().draft().docs.enabled,
        "the settings screen must agree"
    );
    let commands = fixture.drain_commands();
    assert!(
        commands.iter().any(|command| matches!(
            command,
            Command::UpdateSettings(settings) if settings.docs.enabled
        )),
        "the engine must be told: {commands:?}"
    );
    assert!(
        Settings::load_from(&path).docs.enabled,
        "the choice survives a restart"
    );

    fixture.click("Look up docs");
    assert!(!fixture.app().settings().docs.enabled);
    assert!(!Settings::load_from(&path).docs.enabled);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_docs_tab_shows_only_the_fields_for_the_chosen_search_service() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    fixture.click("Docs");

    // DuckDuckGo is the default: no key or address, but an honest warning.
    fixture
        .harness
        .get_by_label_contains("blocks automated searches");
    assert!(
        fixture
            .harness
            .query_by_label("Brave Search API key")
            .is_none()
    );
    assert!(fixture.harness.query_by_label("SearXNG address").is_none());
    fixture
        .harness
        .get_by_label_contains("What leaves your computer");

    fixture.click("Search service");
    fixture.click("Brave Search (API key)");
    fixture.harness.get_by_label("Brave Search API key");
    assert!(fixture.harness.query_by_label("SearXNG address").is_none());

    fixture.type_into("Brave Search API key", "brave-key-123");
    assert_eq!(
        fixture.app().draft().docs.brave_api_key.expose(),
        "brave-key-123"
    );

    fixture.click("Search service");
    fixture.click("SearXNG (your own server)");
    fixture.harness.get_by_label("SearXNG address");
}

#[test]
fn saving_the_docs_tab_applies_the_choices() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    fixture.click("Docs");
    fixture.click("Look up documentation before answering");
    fixture.click("Save");

    let commands = fixture.drain_commands();
    let applied = commands
        .iter()
        .find_map(|command| match command {
            Command::UpdateSettings(settings) => Some(settings),
            _ => None,
        })
        .expect("saving hands settings to the engine");
    assert!(applied.docs.enabled);
}

#[test]
fn test_search_asks_the_engine_with_the_settings_as_edited_and_shows_the_outcome() {
    let mut fixture = fixture(configured_settings(), None);
    fixture.click("Settings");
    fixture.click("Docs");
    fixture
        .harness
        .state_mut()
        .draft_for_test()
        .docs
        .searxng_url = "http://my-searx.example:8080".into();
    fixture.drain_commands();

    fixture.click("Test search");
    let commands = fixture.drain_commands();
    match &commands[..] {
        [Command::TestDocsSearch(settings)] => {
            assert_eq!(settings.docs.searxng_url, "http://my-searx.example:8080");
        }
        other => panic!("expected one TestDocsSearch, got {other:?}"),
    }

    fixture
        .events
        .send(UiEvent::DocsSearchTested(Ok(
            "Works: 8 results, the first is \"The Rust Programming Language\".".into(),
        )))
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label_contains("Works: 8 results");

    fixture
        .events
        .send(UiEvent::DocsSearchTested(Err(
            "DuckDuckGo asked for a CAPTCHA".into(),
        )))
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label_contains("asked for a CAPTCHA");
    assert!(
        fixture
            .harness
            .query_by_label_contains("Works: 8")
            .is_none()
    );
}

#[test]
fn the_pages_an_answer_was_based_on_appear_as_links() {
    let mut fixture = fixture(configured_settings(), None);
    for event in [
        UiEvent::StateChanged(VoiceState::LookingUpDocs),
        UiEvent::Sources {
            query: "figma auto layout wrap".into(),
            sources: vec![
                DocSource {
                    title: "Guide to auto layout".into(),
                    url: "https://help.figma.com/hc/en-us/articles/360040451373".into(),
                },
                DocSource {
                    title: "Wrapping".into(),
                    url: "https://help.figma.com/hc/en-us/articles/1".into(),
                },
            ],
        },
    ] {
        fixture.events.send(event).unwrap();
    }
    fixture.settle();

    fixture
        .harness
        .get_by_label_contains("Looked up the docs for");
    fixture.harness.get_by_label("Guide to auto layout");
    fixture.harness.get_by_label("Wrapping");

    let entry = fixture
        .app()
        .messages()
        .iter()
        .find(|entry| entry.role == ChatRole::Sources)
        .expect("a sources entry");
    assert_eq!(entry.links.len(), 2);
    assert_eq!(
        entry.links[0].url,
        "https://help.figma.com/hc/en-us/articles/360040451373"
    );
}

#[test]
fn the_header_says_when_docs_are_being_looked_up() {
    let mut fixture = fixture(configured_settings(), None);
    fixture
        .events
        .send(UiEvent::StateChanged(VoiceState::LookingUpDocs))
        .unwrap();
    fixture.settle();
    fixture.harness.get_by_label("Looking up docs");
    // There is a way out while it works.
    fixture.harness.get_by_label("Stop");
}
