# Clicky

An AI buddy that lives on your desktop. It can **see your screen, talk, and point at things**, like having a patient teacher sitting next to you.

Ask a question by typing, or hold a shortcut and talk. Clicky looks at your screen, answers (in the window and, if you want, out loud), and when it helps it flies a blue cursor to the exact button or menu you need.

![Clicky demo (the original macOS version)](clicky-demo.gif)

This is a Rust rewrite of [farzaa/clicky](https://github.com/farzaa/clicky) (MIT). It is now a normal window app that runs on **Windows, macOS and Linux**, works with **many AI providers**, and has **no account, proxy or telemetry**: it talks straight to the provider you pick, with your own API key.

## Quick start

You need [Rust](https://rustup.rs) 1.95 or newer (the GUI toolkit requires it).

```bash
git clone <this repository>
cd clicky
cargo run --release
```

On first launch Clicky opens **Settings**: pick a provider, paste its API key, press **Save**, then type a question. That is all you need. Voice is optional (see below).

Just want to look around without a key? Run the fake provider and point Clicky at it:

```bash
cargo run --example mock_provider      # in a second terminal
```

Then choose **Custom (OpenAI-compatible)** with base URL `http://127.0.0.1:8787/v1` (any key, any model). Its replies point at a spot on your screen so you can watch the whole pipeline work.

### Linux build dependencies

On Ubuntu/Debian (built and tested on 24.04):

```bash
sudo apt install libasound2-dev libx11-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xfixes0-dev \
  libxcb-shape0-dev libxkbcommon-dev libdbus-1-dev libpipewire-0.3-dev libgbm-dev libwayland-dev \
  libegl1-mesa-dev libgl1-mesa-dev clang cmake pkg-config
```

Windows and macOS need the Rust toolchain. The first build also compiles a TLS library (`aws-lc`), which may need `cmake` and a C compiler on your platform; I could only build on Linux, so check that library's build notes if it fails elsewhere.

## AI providers

Choose one in **Settings > AI model**. Everything except the key and model is pre-filled, and **Refresh models** fetches the provider's live model list.

| Provider | Notes |
|---|---|
| Anthropic (Claude) | Messages API. |
| OpenAI | Chat completions. |
| OpenRouter | One key, hundreds of models. The refreshed list shows which accept images. |
| OpenCode Zen | Models are routed to the right endpoint automatically (Claude and some Qwen models use the messages API, GPT/Grok/Muse use the responses API, the rest chat completions). Needs a paid Zen key: its free-tier models only work inside the OpenCode app and answer other clients with HTTP 403. Gemini models on Zen use Google's native API, which Clicky does not speak; use the Gemini or OpenRouter provider for those. |
| DeepSeek | Chat completions. |
| Google Gemini | Through Google's OpenAI-compatible endpoint. |
| Groq | Chat completions. |
| Ollama / LM Studio | Local servers, no key needed. Nothing leaves your machine. |
| Custom | Any server that implements `/chat/completions` (xAI, Mistral, Together, vLLM, ...). |

- **Screenshots** are sent with each question by default. Turn that off for text-only models. If a model refuses images anyway, Clicky retries once without them and tells you.
- Model names change often. If a default is out of date, type another or use **Refresh models**.
- Reasoning models can spend your whole reply budget on thinking and return nothing; raise *Maximum reply length* in **General** if that happens.

### Keys

Keys are stored in the settings file, which is created readable by you only on Linux/macOS:

| OS | Location |
|---|---|
| Linux | `~/.config/clicky/config.json` |
| macOS | `~/Library/Application Support/clicky/config.json` |
| Windows | `%APPDATA%\clicky\config\config.json` |

Set `CLICKY_CONFIG_DIR` to use another folder. You can keep keys out of the file entirely with environment variables: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, `DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, `CLICKY_CUSTOM_API_KEY`, `ASSEMBLYAI_API_KEY`, `ELEVENLABS_API_KEY`. A key typed into Settings wins over the environment.

## Voice

Typing works with no extra setup. For voice, open **Settings > Voice**:

- **Speech input**: AssemblyAI (words appear live while you talk) or any OpenAI-compatible transcription endpoint (OpenAI Whisper, Groq, ...), uploaded when you let go.
- **Push-to-talk**: hold the on-screen button, or hold **Ctrl + Alt/Option** (changeable) in any app. The shortcut only counts after a brief hold, so ordinary shortcuts such as Ctrl+Alt+T do not trigger it.
- **Voice replies**: ElevenLabs. Off by default; replies always appear as text.

## Looking up the real docs

A model's memory of a program's menus and settings is often out of date or simply wrong. Turn on **Look up docs** (the checkbox under the talk button, or **Settings > Docs**) and Clicky grounds its answer in the software's actual documentation:

1. It asks the model one short question: does this need documentation, and if so, what should I search for? ("Rewrite my essay" does not; "how do I wrap items in Figma?" does.) The model sees the screen you are working in, so it knows which app you mean.
2. It searches the web, puts documentation sites first (`docs.*`, `help.*`, `/docs/`, Read the Docs, ...), and reads the best pages.
3. It hands the page text to the model together with your question and tells it to base the steps on the docs. The chat shows which pages were used, as links.

It works with every provider, because the lookup happens in Clicky rather than inside the model. If anything goes wrong (no results, a blocked search, a timeout), Clicky says so and answers from memory instead; it never blocks your answer. It is **off by default**, because it costs one extra short model call and a few seconds, and sends a search query to a third party.

Pick a search service in **Settings > Docs**, and use **Test search** to check it:

| Service | Notes |
|---|---|
| DuckDuckGo | No setup, but best effort: DuckDuckGo answers automated searches from some networks with a CAPTCHA (it did on the network I tested from). Clicky detects this and tells you. |
| Brave Search | Reliable. Needs an API key (there is a free tier): [brave.com/search/api](https://brave.com/search/api), or set `BRAVE_API_KEY`. |
| SearXNG | A server you run yourself, with the `json` format enabled under `search.formats` in its `settings.yml`. Public instances do not allow this. |

What leaves your computer when it is on:

- A **short search query** goes to the search service. The model writes it from your question and screen, so Clicky filters it first: web addresses, e-mail addresses, long numbers, file paths and search operators are removed.
- The **pages are downloaded straight from their websites** by your computer, then their text goes to **your AI provider** with your question.
- Page text is **untrusted**. It is fenced off with a random marker and the model is told never to follow instructions found in it. That reduces, but cannot eliminate, the risk of a malicious page influencing an answer, so do not treat a documented command as safe just because it appears in the excerpt.

Because Clicky fetches pages from *your* machine, it only reads `https://` pages on public internet addresses. It refuses `localhost`, private and link-local addresses (including cloud metadata addresses), internal-looking names, odd ports and URLs with credentials, re-checks every redirect, and drops DNS answers that point at private addresses (behind an HTTP proxy the proxy does the lookup, so that last check cannot apply, though the others still do). This also means documentation on your own network (a company wiki on a private address) is not read.

## Pointing

When the model decides pointing would help, it appends a coordinate tag. Clicky maps it to the right monitor and flies a blue cursor from your mouse to the target, shows a short label, then flies back. Turn it off in **General** if you prefer.

## Platform notes and limits

Be aware of these; they are the honest edges of the current version.

- **Verified**: everything builds, lints clean and passes 209 tests on Linux, and the Windows target (`x86_64-pc-windows-gnu`) compiles. The app was driven for real under X11 (typing, streaming replies, the pointer flight, settings). The provider clients were tested against mock servers and, where no key is needed, live endpoints (OpenRouter and OpenCode Zen model lists, AssemblyAI's TLS handshake and bad-key handling). For the docs lookup, page fetching and text extraction were run against live pages (Figma's help center, the Rust Book, GitHub Docs), and the request-safety checks were tested against local mock servers, including redirects into an internal service.
- **Not verified**: a successful Brave Search, and DuckDuckGo from a network that is not challenged (from mine DuckDuckGo only served a CAPTCHA, so its result parser was tested on markup shaped like the real page, not on a live one). I had no API keys, microphone, speakers or Windows/macOS machine, so real chat replies from each provider, real microphone capture, ElevenLabs audio, and running on Windows or macOS are untested. macOS was not compiled at all. Please report what breaks.
- **The pointer overlay needs a compositing desktop** (every Windows and macOS desktop, most Linux desktops). Without compositing, the overlay window cannot be transparent. Turn the overlay off in **General** in that case.
- **The pointer overlay and global shortcut need X11 on Linux.** Wayland does not let apps position windows or read global keys; use the on-screen button there. (XWayland sessions work.)
- **Keep the Clicky window open** (behind other windows is fine). A minimized window may stop processing events on some systems, so replies and pointing would not appear until you restore it.
- On **macOS**, allow *Screen Recording* (for screenshots), *Microphone*, and *Input Monitoring* (for the global shortcut) for the app or terminal that runs Clicky.
- On setups with monitors of **different display scaling**, the pointer can land slightly off; it is exact for a single scale.
- The AssemblyAI websocket does not use HTTP proxy settings. Behind a proxy, use the OpenAI-compatible upload option for speech.
- The always-visible blue cursor from the macOS version (which followed your mouse) is not ported; the cursor appears only when it points.
- Clicky's own window is visible in the screenshots it takes.

## Privacy and security

(See also *What leaves your computer* under *Looking up the real docs*.)

Compared with the original app, these were removed because they were not needed to use it:

- The **Cloudflare Worker proxy**. It had no authentication, so anyone who found its URL could spend the owner's Anthropic, ElevenLabs and AssemblyAI credit. Clicky now calls providers directly with *your* keys.
- **Analytics** that sent every question and every answer to PostHog, **email collection**, the **auto-update feed** that pointed at someone else's repository, the **onboarding video stream and music**, the **camera entitlement**, and the hard-coded third-party endpoints.

What Clicky does now:

- Screenshots and questions go only to the provider you chose (and your voice only to the speech service you chose). Screenshots are taken only when you ask something, and responses requests set `store: false`.
- API keys are redacted from debug output and the settings file is owner-only on Unix. Tests check that provider, speech and voice errors do not echo the key.
- The settings screen warns if a base URL would send your key over plain `http://` to a remote host (local servers are fine).
- TLS uses the operating system's certificate store.

## Development

See [AGENTS.md](AGENTS.md) for the architecture, file map and conventions.

```bash
cargo test                                                              # no display needed
xvfb-run -a cargo test --test display_integration -- --test-threads=1   # real X11 screen capture + hotkey
cargo clippy --all-targets
```

## License

MIT. Original Clicky by [Farza](https://github.com/farzaa/clicky).
