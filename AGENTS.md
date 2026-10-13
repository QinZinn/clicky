# Clicky - Agent Instructions

## Overview

Clicky is a cross-platform desktop app (Windows, macOS, Linux) written in Rust. It is an AI buddy that can **see your screen, talk, and point at things**. You ask a question by typing or by holding a push-to-talk shortcut. Clicky takes a screenshot of every monitor, sends it with your question to the LLM provider you chose, streams the answer into the window, optionally speaks it aloud, and flies a blue cursor to whatever the model points at.

It started as a macOS-only Swift menu bar app (`farzaa/clicky`). It was rewritten in Rust with a normal window. There is no proxy server, no account and no telemetry: the app calls the AI providers directly with the user's own API keys.

## Architecture

- **UI**: `eframe`/`egui` (glow renderer), a normal window. The blue pointer is a second, transparent, click-through, always-on-top window (an egui immediate viewport) created per flight and closed when the flight ends.
- **Async runtime**: `tokio` (multi-thread). The UI thread never blocks; it talks to the engine over channels.
- **Engine** (`engine.rs`): one async task owning the conversation. Receives `Command`s (start/stop listening, send text, cancel, update settings, refresh models), runs a *turn* as a cancellable task, and reports `UiEvent`s. All hardware (screen capture, speakers) is injected, so a whole turn is testable without any.
- **LLM layer** (`llm.rs`, `providers.rs`, `sse.rs`): one streaming client for three wire formats: OpenAI chat completions, OpenAI responses, Anthropic messages. Presets for Anthropic, OpenAI, OpenRouter, OpenCode Zen, DeepSeek, Gemini, Groq, Ollama, LM Studio and a custom OpenAI-compatible server. OpenCode Zen serves different model families on different endpoints, so the format is chosen per model (`providers::zen_wire_format`). If a model rejects images, the request is retried once without them.
- **Speech in** (`stt.rs`, `audio.rs`): `cpal` microphone capture, converted to 16 kHz mono PCM16 in 100 ms chunks. AssemblyAI v3 streaming over a websocket (token fetched with the user's key), or an upload to any OpenAI-compatible `/audio/transcriptions`.
- **Speech out** (`tts.rs`, `playback.rs`): ElevenLabs, played with `rodio` on a dedicated audio thread.
- **Screens** (`screen.rs`): `xcap` captures every monitor, downscaled to at most 1280 px and JPEG-encoded. Each image gets a caption with its exact pixel size, because the model answers pointing requests in that pixel space.
- **Pointing** (`pointing.rs`, `geometry.rs`, `overlay.rs`): the model appends `[POINT:x,y:label:screenN]`. The tag is parsed and stripped, the pixel coordinate is mapped onto the right monitor, and `overlay::PointerFlight` animates the cursor (a pure state machine: bezier arc, easing, bubble typing, return flight).
- **Global shortcut** (`input.rs`): polls modifier keys with `device_query` (no OS hooks). A pure `HotkeyDetector` debounces so shortcuts like Ctrl+Alt+T do not start a recording. The same module reads the mouse position.
- **Settings** (`config.rs`): one JSON file in the OS config directory (owner-only on Unix), overridable with `CLICKY_CONFIG_DIR`. API keys can also come from environment variables. Keys are wrapped in `Secret` so they never appear in `Debug` output.

### Key decisions

- **Direct provider calls, no proxy.** The old Cloudflare Worker existed so keys would not ship in a distributed binary. A desktop app that asks the user for their own key does not need it, and an unauthenticated proxy was an open door to the owner's credits.
- **Push-to-talk lasts until the mouse button is released.** egui forgets a "pressed" widget after ~0.8 s, so the on-screen talk button tracks the global pointer state instead (`ClickyApp::talk_button`). A regression test covers it.
- **Polling, not hooks, for the shortcut.** Works while other apps have focus, with no event-loop plumbing.
- **TLS roots come from the OS** (`rustls-tls-native-roots`), so corporate/inspection CAs work.

## Key Files

| File | Lines | Purpose |
|------|-------|---------|
| `src/main.rs` | ~140 | Entry point: window options, generated icon, starts `ClickyApp`. |
| `src/app.rs` | ~740 | The main window: chat, input bar, hold-to-talk button, pointer overlay window. Built from injectable `AppParts` so tests can drive it headlessly. |
| `src/settings_ui.rs` | ~490 | Settings screen (AI model / Voice / General). Captions are linked to inputs with `labelled_by`. |
| `src/engine.rs` | ~440 | Command loop and the turn pipeline. |
| `src/llm.rs` | ~860 | Request builders, streaming decoder, text-only retry, model-list parsing. |
| `src/providers.rs` | ~420 | Provider presets, wire-format resolution (incl. Zen routing), URL helpers, plain-http warning. |
| `src/sse.rs` | ~150 | Server-Sent Events decoder (handles chunks split mid-character). |
| `src/config.rs` | ~490 | Settings types, `Secret`, load/save, environment-variable fallbacks. |
| `src/stt.rs` | ~720 | `Recording`, AssemblyAI session and transcript assembly, Whisper-style upload. |
| `src/audio.rs` | ~480 | Microphone capture thread, resampler, PCM16 and WAV helpers. |
| `src/tts.rs` | ~120 | ElevenLabs client. |
| `src/playback.rs` | ~135 | `SpeechPlayer` trait and the `rodio` implementation. |
| `src/screen.rs` | ~320 | Multi-monitor capture, scaling, captions, monitor geometry. |
| `src/input.rs` | ~410 | Hotkey detector and polling thread, mouse position. |
| `src/pointing.rs` | ~210 | `[POINT:...]` parsing and streaming-safe display text. |
| `src/geometry.rs` | ~130 | `Point`, `Rect`, screenshot-pixel to desktop mapping. |
| `src/overlay.rs` | ~585 | Pointer flight animation (pure) and its painter. |
| `src/prompt.rs` | ~35 | The system prompt. |
| `examples/mock_provider.rs` | ~105 | A fake provider to try the whole app without API keys. |
| `tests/` | ~2400 | Mock-server, engine pipeline, headless UI and X11 display tests. |

## Build & Run

```bash
cargo run --release          # the app
cargo run --example mock_provider   # fake provider on 127.0.0.1:8787, no keys needed
```

Linux build dependencies (Ubuntu/Debian): `libasound2-dev libx11-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xfixes0-dev libxcb-shape0-dev libxkbcommon-dev libdbus-1-dev libpipewire-0.3-dev libgbm-dev libwayland-dev libegl1-mesa-dev libgl1-mesa-dev clang cmake pkg-config`. Windows and macOS need a Rust toolchain; the first build of `aws-lc-sys` may also need cmake and a C compiler (only verified on Linux).

## Testing

```bash
cargo test                                   # everything that needs no display
xvfb-run -a cargo test --test display_integration -- --test-threads=1   # real X11: capture + hotkey
cargo clippy --all-targets                   # must stay warning-free
cargo fmt
```

- Unit tests live next to the code. `tests/llm_mock_server.rs`, `stt_mock_server.rs` and `tts_mock_server.rs` run the real clients against local mock servers (including a mock AssemblyAI websocket).
- `tests/engine_pipeline.rs` drives the real engine end to end: only the screen and speakers are fakes.
- `tests/ui_interaction.rs` uses `egui_kittest` to click, type and hold-to-talk in the real widgets, headlessly.
- `tests/display_integration.rs` skips itself without `DISPLAY`; under `xvfb-run` it captures a real screen and checks the shortcut with real key events (`xdotool`).
- When you fix a bug, add a regression test, then check it fails against the old behavior.

## Code Style & Conventions

### Variable and Method Naming

IMPORTANT: Follow these naming rules strictly. Clarity is the top priority.

- Be as clear and specific with variable and method names as possible
- **Optimize for clarity over concision.** A developer with zero context on the codebase should immediately understand what a variable or method does just from reading its name
- Use longer names when it improves clarity. Do NOT use single-character variable names (closure parameters included)
- Example: use `originalQuestionLastAnsweredDate` instead of `originalAnswered`
- The one exception: the `x` and `y` *fields* of `geometry::Point` and `Rect`, the universal names for screen coordinates (egui and the OS APIs use them too)
- When passing arguments to functions, keep the same names as the original variable. Do not shorten or abbreviate parameter names

### Code Clarity

- **Clear is better than clever.** Do not write functionality in fewer lines if it makes the code harder to understand
- Comments explain "why", especially for platform quirks and non-obvious decisions (several bugs here came from egui and OS behavior that is not obvious)
- Keep hardware behind traits or injected functions so logic stays testable
- All buttons show a pointer cursor on hover (`on_hover_cursor(egui::CursorIcon::PointingHand)`)
- Never log or format API keys; keep them in `Secret`. Error messages must not include request URLs with credentials

### Do NOT

- Do not add features, refactor code, or make "improvements" beyond what was asked
- Do not add telemetry, analytics or any network call the user did not configure
- Do not add a proxy server that holds keys; keys belong to the user
- Do not leave `cargo clippy --all-targets` warnings

## Git Workflow

- Branch naming: `feature/description` or `fix/description`
- Commit messages: imperative mood, concise, explain the "why" not the "what"
- Do not force-push to main

## Self-Update Instructions

When you make changes to this project that affect the information in this file, update this file to reflect those changes. Specifically:

1. **New files**: Add new source files to the "Key Files" table with their purpose and approximate line count
2. **Deleted files**: Remove entries for files that no longer exist
3. **Architecture changes**: Update the architecture section if you introduce new patterns, frameworks, or significant structural changes
4. **Build changes**: Update build commands if the build process changes
5. **New conventions**: If the user establishes a new coding convention during a session, add it to the appropriate conventions section
6. **Line count drift**: If a file's line count changes significantly (>50 lines), update the approximate count in the Key Files table

Do NOT update this file for minor edits, bug fixes, or changes that don't affect the documented architecture or conventions.
