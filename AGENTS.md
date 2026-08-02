# AGENTS.md

Erika is the in-house playback core for NipaPlay: an embeddable Rust media
engine that owns demux/decode → GPU presentation → audio → subtitles → danmaku,
exposed to hosts through a Rust API, a C ABI (`erika_capi`), and a Flutter
plugin (`erika_flutter`). The workspace is MPL-2.0; native dependencies are
built separately under the LGPL profile.

Base docs are English (`docs/*.md`, `README.md`, `CONTRIBUTING.md`); `*.zh.md`
/ `*.ja.md` translations mirror them and are updated in the same PR as
user-visible changes. `docs/architecture.md` and `CONTRIBUTING.md` are the
authoritative orientation docs — read them before diving into code.

## Build & test commands

Native deps are **not vendored** — you must build them once before `cargo build`
works. They stage into `third_party/dist/<target>/<profile>/` and are discovered
by `erika_ffmpeg_sys/build.rs`.

```sh
# 1. Native dependencies (FFmpeg 8.1.2 + dav1d; --all also adds libass/FreeType/HarfBuzz/FriBidi)
cargo run -p xtask -- deps status                      # what's already present
cargo run -p xtask -- deps build --all --profile lgpl  # build everything

# 2. Compile / lint / test
cargo build -p erika
cargo test --workspace
cargo test -p erika --lib <test_name>                  # single test
cargo clippy --workspace
cargo fmt --all
cargo run -p xtask -- check license                    # also checked in CI
cargo run -p xtask -- check cargo-patches

# 3. Playback-path smoke test (prints per-frame pipeline counters)
cargo run -p windows_native_demo -- --smoke-seconds 3 "$SAMPLE"   # Windows
cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"     # macOS
```

Flutter plugin (`packages/erika_flutter`): `flutter pub get`, `flutter analyze
--no-pub`, `flutter test --no-pub`. CI runs these on Ubuntu.

### Key env vars & gotchas

- `ERIKA_NATIVE_PROFILE` (`lgpl`/`gpl-full`) and `ERIKA_NATIVE_TARGET` (Rust
  target triple) must match what you passed to `xtask` and to `cargo build
  --target`. A mismatch silently links native libs for the wrong architecture.
- `xtask --target`, `ERIKA_NATIVE_TARGET`, and Cargo `--target` must be the
  same value.
- bindgen needs libclang: set `LIBCLANG_PATH` if not auto-found. Android
  cross builds need `ANDROID_NDK_HOME`, and per-target
  `CARGO_TARGET_<TRIPLE>_LINKER`/`CC_`/`CXX_`/`AR_` env vars (see
  `docs/building.md` for a complete Windows PowerShell recipe).
- Windows FFmpeg configure requires a POSIX shell (Git Bash/MSYS2) + GNU make,
  run from an MSVC environment.
- FFmpeg **8.x** is required (`libavutil >= 60`); the native core rejects older
  versions. `ERIKA_ALLOW_LEGACY_FFMPEG=1` is a local-compat escape hatch only.
- `Cargo.toml` carries a `[patch.crates-io]` for `wgpu-hal` (in-tree
  OpenHarmony WSI adaptation in `third_party/wgpu-hal`) — don't remove it.

## Architecture

### Workspace layout

```
crates/erika              Core engine (the library you mostly edit)
crates/erika_capi         C ABI export layer → include/erika.h (+ android_jni.rs, presenter_json.rs)
crates/erika_ffmpeg_sys   bindgen bindings to the staged FFmpeg dist
packages/erika_flutter    Flutter plugin (macOS/iOS/Windows/Android/ohos native + Dart)
examples/                 smoke/demo binaries; each platform has a *_native_demo
xtask/                    native dependency build orchestration + policy checks
docs/                     architecture, building, capi_reference, integration, flutter_embedding
third_party/              staged native build output (cache/src/build/dist per target+profile)
```

### `crates/erika` module map

| Module | Responsibility |
|--------|----------------|
| `core` | Public config, `Player`, the `RendererBackend` trait, `PlatformSurface`, `PlayerEvent`/`PlayerVideoFrame` etc. |
| `playback` | `PlaybackSession` (demux/decode/seek), `VideoPlaybackEngine` (clocked playback), `PlaybackClock` (audio-master discipline), `VideoFrameScheduler`, `DisplaySyncState`. |
| `ffmpeg` | Safe FFmpeg wrappers; demuxer (incl. DASH dual-stream `DemuxInput`), `DecoderBackend` selection (software/VideoToolbox/D3D11VA/MediaCodec/OHOS AVCodec). |
| `audio` | `AudioOutputBackend` trait, `AudioRingBuffer`, audio clock, volume ramps. |
| `renderer` | `metal/` (Apple), `d3d11` (Windows), `wgpu` (cross-platform/Android/OHOS), `pipeline` (backend-agnostic color/tone-map/scaler decisions), `output`, `artcnn` + `wgpu_artcnn` (neural upscaler). |
| `presenter` | `PresenterRuntime` — owns the full stack; the thing `render_tick` drives. |
| `danmaku` | Bilibili XML/JSON parsing, DFM+ collision-avoidance layout (`dfm_core/`), glyph-atlas rasterizer, render plans. |
| `subtitle` / `overlay` / `text` | SRT/WebVTT/ASS + libass; overlay timeline; font providers. |
| `source` | `MediaSource` trait — file + HTTP range, multi-segment prefetch, DASH read-ahead. |
| `apple` / `windows` / `android` / `ohos` | Platform glue (CoreAudio/AudioQueue/WASAPI/AAudio/OHAudio; VideoToolbox/D3D11VA/MediaCodec/AVCodec; Metal interop; native-window). |

### Runtime model (the non-obvious core)

- **`PresenterRuntime` owns the stack.** The host calls `render_tick(time_seconds)`
  once per display frame; the runtime pumps decoded frames, updates the overlay
  (subtitle + danmaku), renders through the active `RendererBackend`, and
  presents.
- **Audio is the master clock.** Audio output runs on its own backend thread
  (WASAPI render thread, CoreAudio callback, …); the playback scheduler
  quantizes video presentation to vsync against the audio clock. Danmaku/subtitle
  frame queries and rendering all read the same playback clock, gated by
  `generation + media_time` so stale plans can't reappear after reopen/seek.
- **Everything async is epoch/generation-gated.** `Player::open` is **async** and
  returns immediately; work happens on a background thread with an epoch counter
  (last open wins). `play` is queued, not blocking — observe `StateChanged`/`Error`
  events for the authoritative result. Decoder-changing operations (seek reopen,
  Surface→ByteBuffer fallback) use a quiesce/ACK barrier, discard renderer/receiver
  state, transition, then resume. `Player::playback_snapshot()` reads
  clock+generation+state under one lock — never sample those three separately.
- **Events flow over a channel.** Player state changes publish on a crossbeam
  channel; hosts poll via `poll_event` / `subscribe`.

### Three pluggable seams

- `RendererBackend` (`core.rs`) — `attach_surface`/`resize_surface`,
  `upload_player_frame` (own the GPU representation), `render_current_frame`
  (composite overlay), plus `capture_current_frame`, `runtime_stats`,
  `set_luma_upscaler`. `RendererBackendPreference` (`PlatformNative` vs
  `WgpuFallback`) picks the concrete backend. `renderer::pipeline` keeps
  color/tone-map decisions backend-agnostic so all backends behave identically.
- `AudioOutputBackend` (`audio.rs`) — `configure`/`start`/`pause`/`stop`/
  `push`/`set_volume`/`state`/`stats` + `clock_snapshot` for A/V sync. Reuse
  `BufferedAudioOutput`/`AudioRingBuffer`.
- `DecoderBackend` selection (`ffmpeg.rs`) — software plus hardware paths that
  preserve color metadata for zero-copy import. If hardware interop is
  unavailable, fall back to software decode explicitly (never silently).

Adding a platform = decode path + `RendererBackend` impl + `AudioOutputBackend`
impl, each `#[cfg(target_os = …)]`-gated, then wire selection and add a
`<platform>_native_demo`. When you touch a `cfg` branch, keep every arm
(macos/ios/windows/android/fallback) compiling — CI builds the ones you can't
test locally.

### C ABI contract (hard rules)

`crates/erika_capi` is the stable surface for every non-Rust host:

- **Never let a panic cross the boundary.** Every export wraps its body in
  `catch_unwind` and maps to `ErikaStatus_Panic`.
- **Ownership:** strings handed out are caller-owned, freed via the matching
  `*_free`; document new functions in `docs/capi_reference.md` and keep
  `include/erika.h` in sync.
- **⚠️ Swift mirror structs.** The macOS/iOS plugins hand-mirror C structs
  (e.g. `ErikaPresenterStats`) in Swift. Change a struct in `erika.h` → update
  **both** Swift mirror files in `packages/erika_flutter`; a mismatch corrupts
  the stack and surfaces as a misleading autorelease-pool crash.
- **Surface dimensions are physical pixels.** `attach_*`/`resize_surface` take
  exact drawable extent; the `scale` arg affects logical UI content (danmaku),
  never the surface extent.

## Gotchas & invariants worth knowing

- **Unit tests live inline** under `#[cfg(test)]` in the source files
  (`playback.rs` has the most); integration tests are `crates/erika/tests/`
  (`artcnn_upscaler.rs`, `wgpu_artcnn.rs` — the neural upscaler weights are
  verified against onnxruntime references; don't change kernels without
  re-checking **both** paths).
- **Playback fixture is deterministic and checksummed.** CI verifies
  `crates/erika/testdata/playback/SHA256SUMS`. Regenerate only intentionally via
  `./generate.sh --update` (pinned to FFmpeg 8.1.2, bitexact) and review the
  binary diff.
- **Android extended-linear ≠ HDR10/PQ.** It's FP16 extended-linear scRGB
  (`Rgba16Float` + `SCRGB_LINEAR`), gated behind a dozen activation conditions
  with stable fallback reason codes `0..8`. Never infer active output from the
  requested mode — read `erika_presenter_get_output_status()`/`getOutputStatus()`.
  Active-path is only "device-validated" after the API 35 HDR-device acceptance
  run; keep that caveat in docs.
- **Screenshots are always SDR RGBA8** (tone-mapped), including on
  extended-linear surfaces, and exclude danmaku and the debug HUD.
- **Conventions:** Edition 2024, Rust 1.92+. No `unwrap`/`panic` on the hot
  path or across FFI. Platform glue stays in `apple.rs`/`windows.rs`/
  `android.rs`/`ohos.rs`, not spread through the engine. Match surrounding
  comment density and naming.

## Active development focus

Recent work (see `git log`) is on network-streaming robustness: DASH dual-stream
demux (separate video/audio `DemuxInput` with a unified global stream index via
`stream_offset`), multi-segment/multi-connection prefetch in `source.rs`, and
`preload` (buffer the media head without playing, then `play()` starts from the
cached window; queued during async open as `pending_preload`, cancelled by
play/stop). `source.rs`, `ffmpeg.rs`, `playback.rs`, and `core.rs` are the
hot files for this area.
