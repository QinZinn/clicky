//! Persistent user settings.
//!
//! Everything the user configures lives in one JSON file in the OS config
//! directory. API keys are stored in that file (owner-readable only on Unix)
//! and can also come from environment variables, so they never have to touch
//! disk. Keys are wrapped in [`Secret`] so they cannot leak through `{:?}`
//! formatting or log lines by accident.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::{self, PRESETS};

/// A string that must never be printed. `Debug` shows `<redacted>`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        self.0.trim()
    }

    /// Mutable access for text-edit widgets.
    pub fn expose_mut(&mut self) -> &mut String {
        &mut self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            formatter.write_str("Secret(<empty>)")
        } else {
            formatter.write_str("Secret(<redacted>)")
        }
    }
}

/// Returns the stored secret, or the first non-empty environment variable.
pub fn resolve_secret(stored_secret: &Secret, environment_variable_names: &[&str]) -> String {
    if !stored_secret.is_empty() {
        return stored_secret.expose().to_string();
    }
    for variable_name in environment_variable_names {
        if let Ok(value) = std::env::var(variable_name)
            && !value.trim().is_empty()
        {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// Which wire protocol to speak. `Auto` picks one from the provider and model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiStyle {
    #[default]
    Auto,
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

impl ApiStyle {
    pub const ALL: [ApiStyle; 4] = [
        ApiStyle::Auto,
        ApiStyle::OpenAiChat,
        ApiStyle::OpenAiResponses,
        ApiStyle::AnthropicMessages,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ApiStyle::Auto => "Auto-detect",
            ApiStyle::OpenAiChat => "OpenAI chat completions",
            ApiStyle::OpenAiResponses => "OpenAI responses",
            ApiStyle::AnthropicMessages => "Anthropic messages",
        }
    }
}

/// Settings for one LLM provider (one entry per preset, plus "custom").
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: Secret,
    pub model: String,
    pub api_style: ApiStyle,
    /// Attach screenshots of the user's screen(s) to each request. Turn this
    /// off for text-only models.
    pub send_screenshots: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: Secret::default(),
            model: String::new(),
            api_style: ApiStyle::Auto,
            send_screenshots: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SttProvider {
    /// No speech input; the user types instead.
    Disabled,
    /// AssemblyAI real-time streaming over a websocket.
    #[default]
    AssemblyAi,
    /// Upload the recording to any OpenAI-compatible `/audio/transcriptions`.
    OpenAiCompatible,
}

impl SttProvider {
    pub const ALL: [SttProvider; 3] = [
        SttProvider::AssemblyAi,
        SttProvider::OpenAiCompatible,
        SttProvider::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SttProvider::Disabled => "Off (type instead)",
            SttProvider::AssemblyAi => "AssemblyAI (streaming)",
            SttProvider::OpenAiCompatible => "OpenAI-compatible (Whisper upload)",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SttSettings {
    pub provider: SttProvider,
    pub assemblyai_api_key: Secret,
    pub whisper_base_url: String,
    pub whisper_api_key: Secret,
    pub whisper_model: String,
}

impl Default for SttSettings {
    fn default() -> Self {
        Self {
            provider: SttProvider::default(),
            assemblyai_api_key: Secret::default(),
            whisper_base_url: "https://api.openai.com/v1".to_string(),
            whisper_api_key: Secret::default(),
            whisper_model: "whisper-1".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsProvider {
    /// Show replies as text only.
    #[default]
    Off,
    ElevenLabs,
}

impl TtsProvider {
    pub const ALL: [TtsProvider; 2] = [TtsProvider::Off, TtsProvider::ElevenLabs];

    pub fn label(self) -> &'static str {
        match self {
            TtsProvider::Off => "Off (text only)",
            TtsProvider::ElevenLabs => "ElevenLabs",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsSettings {
    pub provider: TtsProvider,
    pub elevenlabs_api_key: Secret,
    pub voice_id: String,
    pub model_id: String,
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            provider: TtsProvider::Off,
            elevenlabs_api_key: Secret::default(),
            // "Rachel", one of ElevenLabs' premade voices available to every account.
            voice_id: "21m00Tcm4TlvDq8ikWAM".to_string(),
            model_id: "eleven_flash_v2_5".to_string(),
        }
    }
}

/// Which modifier combination is the global push-to-talk shortcut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyChoice {
    #[default]
    CtrlAlt,
    CtrlShift,
    AltShift,
    Disabled,
}

impl HotkeyChoice {
    pub const ALL: [HotkeyChoice; 4] = [
        HotkeyChoice::CtrlAlt,
        HotkeyChoice::CtrlShift,
        HotkeyChoice::AltShift,
        HotkeyChoice::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HotkeyChoice::CtrlAlt => "Hold Ctrl + Alt/Option",
            HotkeyChoice::CtrlShift => "Hold Ctrl + Shift",
            HotkeyChoice::AltShift => "Hold Alt/Option + Shift",
            HotkeyChoice::Disabled => "Disabled (use the on-screen button)",
        }
    }
}

/// Where documentation searches are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocsSearchBackend {
    /// No key needed, but DuckDuckGo blocks automated searches from some
    /// networks with a CAPTCHA, so this is best effort.
    #[default]
    DuckDuckGo,
    /// Brave Search API. Needs a key (there is a free tier).
    Brave,
    /// A SearXNG server you run yourself, with its JSON output enabled.
    SearxNg,
}

impl DocsSearchBackend {
    pub const ALL: [DocsSearchBackend; 3] = [
        DocsSearchBackend::DuckDuckGo,
        DocsSearchBackend::Brave,
        DocsSearchBackend::SearxNg,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DocsSearchBackend::DuckDuckGo => "DuckDuckGo (no key, best effort)",
            DocsSearchBackend::Brave => "Brave Search (API key)",
            DocsSearchBackend::SearxNg => "SearXNG (your own server)",
        }
    }
}

/// Settings for looking up real documentation before answering.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DocsSettings {
    /// Off by default: it adds a model call and a web search to every question,
    /// and sends a short search query to the search service.
    pub enabled: bool,
    pub backend: DocsSearchBackend,
    pub brave_api_key: Secret,
    /// Base URL of your SearXNG server, e.g. `http://localhost:8080`.
    pub searxng_url: String,
    /// How many pages to read and give the model (1 to 5).
    pub max_pages: u8,
}

impl Default for DocsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: DocsSearchBackend::default(),
            brave_api_key: Secret::default(),
            searxng_url: "http://localhost:8080".to_string(),
            max_pages: 3,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Preset id of the provider currently in use (see [`providers::PRESETS`]).
    pub active_provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub max_output_tokens: u32,
    pub history_exchanges: usize,
    pub request_timeout_secs: u64,
    pub stt: SttSettings,
    pub tts: TtsSettings,
    pub docs: DocsSettings,
    pub hotkey: HotkeyChoice,
    /// Fly a blue cursor to UI elements the model points at.
    pub show_pointer_overlay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let mut settings = Self {
            active_provider: "anthropic".to_string(),
            providers: BTreeMap::new(),
            max_output_tokens: 2048,
            history_exchanges: 10,
            request_timeout_secs: 90,
            stt: SttSettings::default(),
            tts: TtsSettings::default(),
            docs: DocsSettings::default(),
            hotkey: HotkeyChoice::default(),
            show_pointer_overlay: true,
        };
        settings.fill_missing_providers();
        settings
    }
}

impl Settings {
    /// Ensures every preset has an entry, so presets added in newer versions
    /// show up for people with an existing settings file.
    pub fn fill_missing_providers(&mut self) {
        for preset in PRESETS {
            self.providers
                .entry(preset.id.to_string())
                .or_insert_with(|| ProviderConfig {
                    base_url: preset.base_url.to_string(),
                    api_key: Secret::default(),
                    model: preset.default_model.to_string(),
                    api_style: ApiStyle::Auto,
                    send_screenshots: preset.vision_by_default,
                });
        }
        if providers::preset(&self.active_provider).is_none() {
            self.active_provider = "anthropic".to_string();
        }
    }

    pub fn active_provider_config(&self) -> ProviderConfig {
        self.providers
            .get(&self.active_provider)
            .cloned()
            .unwrap_or_default()
    }

    /// The API key for the active provider, from settings or the environment.
    pub fn active_api_key(&self) -> String {
        let environment_variable_names = providers::preset(&self.active_provider)
            .map(|preset| preset.key_env_vars)
            .unwrap_or(&[]);
        resolve_secret(
            &self.active_provider_config().api_key,
            environment_variable_names,
        )
    }

    pub fn assemblyai_api_key(&self) -> String {
        resolve_secret(&self.stt.assemblyai_api_key, &["ASSEMBLYAI_API_KEY"])
    }

    pub fn whisper_api_key(&self) -> String {
        resolve_secret(&self.stt.whisper_api_key, &["OPENAI_API_KEY"])
    }

    pub fn elevenlabs_api_key(&self) -> String {
        resolve_secret(&self.tts.elevenlabs_api_key, &["ELEVENLABS_API_KEY"])
    }

    pub fn brave_api_key(&self) -> String {
        resolve_secret(
            &self.docs.brave_api_key,
            &["BRAVE_API_KEY", "BRAVE_SEARCH_API_KEY"],
        )
    }

    /// Default location: `<config dir>/clicky/config.json`. The
    /// `CLICKY_CONFIG_DIR` environment variable overrides the directory.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(directory) = std::env::var("CLICKY_CONFIG_DIR")
            && !directory.trim().is_empty()
        {
            return Some(PathBuf::from(directory).join("config.json"));
        }
        directories::ProjectDirs::from("", "", "clicky")
            .map(|dirs| dirs.config_dir().join("config.json"))
    }

    /// Loads settings, falling back to defaults when the file is missing or
    /// unreadable. A corrupt file is moved aside rather than overwritten.
    pub fn load_from(path: &Path) -> Settings {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Settings::default();
        };
        match serde_json::from_str::<Settings>(&contents) {
            Ok(mut settings) => {
                settings.fill_missing_providers();
                settings
            }
            Err(error) => {
                tracing::warn!("could not parse {}: {error}", path.display());
                let backup_path = path.with_extension("json.bak");
                let _ = std::fs::rename(path, backup_path);
                Settings::default()
            }
        }
    }

    pub fn load() -> Settings {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => Settings::default(),
        }
    }

    /// Writes the settings atomically. On Unix the file is created readable
    /// by the owner only, because it can contain API keys.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        use std::io::Write;

        let directory = path.parent().context("settings path has no parent")?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;

        let temporary_path = path.with_extension("json.tmp");
        let serialized = serde_json::to_string_pretty(self)?;

        let mut open_options = std::fs::OpenOptions::new();
        open_options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }
        let mut file = open_options
            .open(&temporary_path)
            .with_context(|| format!("writing {}", temporary_path.display()))?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);

        std::fs::rename(&temporary_path, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::default_path().context("no config directory on this system")?;
        self.save_to(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_secret() {
        let secret = Secret::new("sk-super-secret-value");
        assert!(!format!("{secret:?}").contains("super-secret"));

        let mut settings = Settings::default();
        settings
            .providers
            .get_mut("anthropic")
            .unwrap()
            .api_key
            .expose_mut()
            .push_str("sk-ant-leaky");
        assert!(!format!("{settings:?}").contains("sk-ant-leaky"));
    }

    #[test]
    fn defaults_cover_every_preset() {
        let settings = Settings::default();
        for preset in PRESETS {
            let config = settings.providers.get(preset.id).expect(preset.id);
            assert_eq!(config.base_url, preset.base_url);
            assert_eq!(config.model, preset.default_model);
        }
    }

    #[test]
    fn save_and_load_round_trip_and_restrict_permissions() {
        let directory = std::env::temp_dir().join(format!("clicky-test-{}", std::process::id()));
        let path = directory.join("config.json");

        let mut settings = Settings::default();
        settings.active_provider = "deepseek".into();
        settings
            .providers
            .get_mut("deepseek")
            .unwrap()
            .api_key
            .expose_mut()
            .push_str("sk-test");
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.active_provider, "deepseek");
        assert_eq!(loaded.active_api_key(), "sk-test");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "config must not be group/world readable");
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn corrupt_file_is_moved_aside_not_destroyed() {
        let directory = std::env::temp_dir().join(format!("clicky-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        std::fs::write(&path, "{ not json").unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.active_provider, "anthropic");
        assert!(directory.join("config.json.bak").exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn new_presets_are_added_to_old_settings_files() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"active_provider":"openai","providers":{}}"#).unwrap();
        settings.fill_missing_providers();
        assert_eq!(settings.active_provider, "openai");
        assert!(settings.providers.contains_key("opencode-zen"));
    }

    #[test]
    fn stored_key_wins_over_environment() {
        let stored = Secret::new("stored-key");
        assert_eq!(
            resolve_secret(&stored, &["CLICKY_TEST_VARIABLE_THAT_IS_NOT_SET"]),
            "stored-key"
        );
    }
}
