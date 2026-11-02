# FilmCraft project context for AI contributors

This is a living handoff for future AI assistants and contributors. It records important recent
work, decisions, and user workflow context that may not be obvious from the current request. It is
not a replacement for the code, tests, or detailed documentation linked below.

## Project at a glance

FilmCraft is a pure-Rust, open-source non-linear video editor. Read [AGENTS.md](AGENTS.md) and
[CLAUDE.md](CLAUDE.md) before making changes; their safety, clean-room, architecture, and testing
rules take precedence over this summary. The full architecture and engineering workflow are in
[docs/architecture.md](docs/architecture.md), [docs/agents.md](docs/agents.md), and
[docs/testing.md](docs/testing.md).

The user's public repository for this project is
[mutass/filmcraft](https://github.com/mutass/filmcraft). This checkout's existing `origin` points
to `storytold/filmcraft`; preserve it and use a separate remote when interacting with the user's
repository.

## Recent work

### Windows multi-vendor H.264 hardware encoding

Registered a Media Foundation hardware H.264 encoder factory for Windows, behind Export ▸ Hardware
Encoding = Auto. NVIDIA NVENC remains preferred; compatible synchronous and asynchronous hardware
MFTs are the fallback for AMD, Intel, or NVIDIA drivers. Asynchronous MFTs use bounded event-driven
input/output, and D3D-aware MFTs receive NV12 through D3D11 video-encoder surfaces. A focused
24-frame encode/flush and FilmCraft software-decode round trip passed on the available AMD hardware
MFT. The path is limited to progressive SDR BT.709 NV12 inputs and still spends CPU time converting
rendered RGBA frames; it accelerates encoding, not timeline rendering or colour conversion. See
[platform hardware-encoding details](crates/platform/README.md).

A 10-second, 250-frame export with both GPU Rendering and Hardware Encoding set to Auto initially
completed through `AMDh264Encoder` in 953.2 seconds (0.262 fps), slower than the earlier
692.4-second run. Its GPU renderer hit out-of-memory and fell back to CPU, although AMD hardware
encoding still handled all 250 frames. After reducing GPU export memory use, the same test
completed in 329.2 seconds (0.759 fps) without a GPU out-of-memory fallback. That is faster than
both previous runs, but still takes over five minutes for ten seconds of footage; it is not a
full-sequence result or a general speed guarantee.

To address the renderer OOM, export renderer concurrency is now adapter-class dependent (one
compositor for integrated/software GPUs, two for virtual GPUs, up to four for discrete GPUs).
Export-only compositors also retain four uploaded source textures instead of the interactive
compositor's 24. The memory limits have now been exercised on the Vega 6 with the successful
short export above.

### FilmCraft live-editing workflow skill

Added [`.github/skills/filmcraft-live-editing/SKILL.md`](.github/skills/filmcraft-live-editing/SKILL.md) and its
[verified lessons](.github/skills/filmcraft-live-editing/lessons.md) to guide future live FilmCraft edits.
The workflow requires discovering MCP command schemas, inspecting project/sequence state, verifying
actual rendered frames and track state, and confirming the project is saved. The lessons record the
opacity-keyframe, overlay stacking/placement, and audio-mute checks verified during the Nyange
showcase work. Update the lessons file when future FilmCraft sessions yield additional verified,
reusable findings.

### Timeline-based transcription

The Text panel's **Transcribe** action now transcribes audio represented by enabled clips on enabled
audio tracks in the active sequence, rather than analyzing each entire source song. The UI requests
`transcript.generate` with `timeline: true`; **Sequence ▸ Transcribe Sequence** uses the same path
and can target a selected track. Direct `transcript.generate` calls without `timeline: true` retain
their prior whole-media behavior.

Timeline transcription decodes each clip's source in/out range, maps recognized word times back to
source-media time, replaces existing words in the ranges being transcribed, and preserves words
outside those ranges. Reverse and frame-hold clips are skipped, and an individual clip range is
limited to 30 minutes to bound memory use. Decoding or unresolved-media errors are returned rather
than silently ignored.

The feature is implemented in:

- `crates/engine/src/transcript.rs` — timeline audio selection, decoding, time mapping, and merge.
- `crates/engine/src/sequence_extras.rs` — Sequence ▸ Transcribe Sequence command routing.
- `crates/ui-egui/src/panels/text.rs` — Text panel button requests timeline mode.
- `crates/engine/src/transcript_tests.rs` — regression test for trimmed audio, mapped times, and
  preserving words outside the re-transcribed range.
- `docs/transcripts.md` — command behavior, model/build details, and current limitations.

Whisper is an optional, pure-Rust CPU recognizer. Model weights are not bundled. Recognition of
lyrics may be inaccurate; review results before creating captions or saving them into a project.
Transcription is currently synchronous and has no progress UI. See
[docs/transcripts.md](docs/transcripts.md) for the complete behavior and limitations.

### Whisper tokenizer hardening

The Whisper tokenizer's byte-to-Unicode table no longer uses `expect()` for character conversion.
It returns explicit model errors for invalid table values, and a unit test checks that all 256 byte
values map uniquely. This was done so the Whisper-enabled engine can pass the project's Clippy
no-panic checks.

## Working context and preferences

- The user has been developing and testing a Gospel Mix promotional sequence and asked for the
  debug app to be launched for hands-on checks. Do not overwrite or save user project files unless
  the user asks.
- Do not trigger transcription, create captions, or make other destructive edits in the user's
  project just to validate code. Use focused tests or a disposable fixture unless asked to operate
  on the open project.
- Recognition quality is media-dependent; clearly distinguish successful operation from accurate
  lyric recognition.

## Validation completed for the recent transcription work

- `cargo fmt --check` passed.
- Focused Whisper tokenizer byte-table test passed.
- Focused timeline transcription regression test passed with Whisper and speech-download features.
- Whisper-enabled engine Clippy passed with `--all-targets -- -D warnings`.
- `cargo xtask ci` was attempted: formatting and workspace Clippy passed, but workspace test
  compilation ran out of memory on Windows; the later CI gates did not complete.

## Keeping this file current

Whenever a task changes tracked project files, update this handoff in the same change. Keep even
small updates concise; record what changed and relevant checks, and use dated entries for significant
work. Revise stale information and link to code and detailed docs instead of copying them. Do not
record secrets, personal paths, machine identifiers, or transient process details.
