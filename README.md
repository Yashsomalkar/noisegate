# NoiseGate

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%2F11-0078d4.svg)](#install)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Model: RNNoise + ONNX](https://img.shields.io/badge/model-RNNoise%20%2B%20ONNX-purple.svg)](#noise-suppression-model)
[![Latest release](https://img.shields.io/github/v/release/Yashsomalkar/noisegate?include_prereleases&label=download)](https://github.com/Yashsomalkar/noisegate/releases)

Real-time microphone noise cancellation for Windows. Pure-Rust inference, WASAPI low-latency capture/render, system-tray UI. Pipes cleaned audio into VB-Cable so any app (Zoom, Teams, Discord, OBS, browser calls) sees a noise-free mic.

> Status: pre-alpha scaffold. Builds on Windows 10/11 with the MSVC toolchain. Tested on (none yet — you're the first).

## Why

Existing noise-cancellation tools either cost money (Krisp), require an RTX GPU (NVIDIA Broadcast), or are Linux-only (NoiseTorch). NoiseGate is a free, open-source, lightweight alternative for Windows that runs on any CPU, with a swappable model so you can opt into a state-of-the-art network when you want one.

## Stack

- **Audio I/O:** WASAPI shared low-latency, event-driven, MMCSS Pro Audio scheduling.
- **Routing:** VB-Cable (free virtual audio cable, [vb-audio.com](https://vb-audio.com/Cable/)).
- **DSP model:** **RNNoise** by default (via the [`nnnoiseless`](https://github.com/jneem/nnnoiseless) crate — pure-Rust port). Optional **ONNX** path for newer models like DeepFilterNet3 from Hugging Face.
- **UI:** `tray-icon` + `winit` event loop.
- **Lang:** Rust 2021, single static binary, ~10 MB stripped (RNNoise default), ~25 MB with ONNX runtime.

## Architecture

```
physical mic ─► WASAPI capture ─► ring A ─► DSP thread (denoiser) ─► ring B ─► WASAPI render ─► VB-Cable Input
                                                                                                        │
                                                               other apps choose "CABLE Output" as mic ◄┘
```

Three dedicated MMCSS-priority threads. Lock-free SPSC ring buffers (8 frames ≈ 80 ms headroom). 480-sample (10 ms) frames end-to-end — the native frame size for both RNNoise and DeepFilterNet, so no reblocking inside the DSP path.

## Noise suppression model

Two supported backends, both real-time:

| Backend | Default? | Quality | Install size | How |
|---|---|---|---|---|
| **RNNoise** (via `nnnoiseless`) | ✅ | ★★★ Good. Excellent for stationary noise (fans, hum). Older classical-RNN architecture. | +0 MB (model embedded) | Just `cargo build`. |
| **ONNX (DeepFilterNet3 etc.)** | opt-in (`--features onnx`) | ★★★★★ State of the art. | +15 MB (`onnxruntime.dll`) + ~12 MB model file you supply | Build with `--features onnx`, point config at an ONNX file. See below. |

For most users, RNNoise is what shipping software did until ~2022 and is still good enough for clean voice calls. Step up to ONNX/DFN3 when you have non-stationary noise (kids, traffic, music) and the extra CPU is worth it (typically 3-7%).

## Install

### Option A — Download a prebuilt `.exe` (easiest, once releases are cut)

Grab the latest binary from the **[Releases page](https://github.com/Yashsomalkar/noisegate/releases)**, then jump to [Set up audio routing](#set-up-audio-routing) below.

> No releases yet? Build from source (Option B). A GitHub Actions workflow will start producing prebuilt binaries on every tagged release.

### Option B — Build from source

You need three things on your Windows 10/11 machine:

| # | Component | Download | Notes |
|---|---|---|---|
| 1 | **Rust toolchain** | [rustup-init.exe](https://win.rustup.rs/x86_64) — or visit [rustup.rs](https://rustup.rs/) | Pick the default (`stable`, `x86_64-pc-windows-msvc`). |
| 2 | **MSVC C++ build tools** | [Build Tools for Visual Studio 2022](https://visualstudio.microsoft.com/visual-cpp-build-tools/) | In the installer, check **"Desktop development with C++"** (gives you `link.exe` + the Windows SDK). |
| 3 | **VB-Cable virtual audio driver** | [vb-audio.com/Cable](https://vb-audio.com/Cable/) ([direct ZIP](https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip)) | Extract, run `VBCABLE_Setup_x64.exe` as Administrator, reboot. |
| 4 | **Git** *(probably already installed)* | [git-scm.com/download/win](https://git-scm.com/download/win) | Used to clone the repo. |

Then in PowerShell:

```powershell
git clone https://github.com/Yashsomalkar/noisegate.git
cd noisegate
cargo build --release
.\target\release\noisegate.exe
```

First build pulls ~300 MB of crates and takes 5-10 minutes. Subsequent builds are seconds.

A tray icon appears in the system tray (bottom-right corner of your taskbar). Right-click it for the menu: **Enable**, **Open log folder**, **Quit**.

### Set up audio routing

Once NoiseGate is running, point your communication apps at VB-Cable instead of your real microphone:

1. Open **Windows Sound Settings** → **Sound** → **Input**.
2. Verify **CABLE Output (VB-Audio Virtual Cable)** is in the device list. If not, the VB-Cable install didn't finish — re-run step 3 above.
3. In Zoom / Teams / Discord / OBS / browser settings, choose **CABLE Output** as the microphone.

NoiseGate captures from your real mic, denoises, and writes the cleaned signal into **CABLE Input**. Apps that listen to **CABLE Output** then receive your noise-free voice.

> **Sanity test**: open the Windows **Voice Recorder** app, set its mic to **CABLE Output**, record 10 seconds with a fan / typing in the background. Toggle NoiseGate's tray Enable off and re-record. The difference should be obvious.

## Configuration

`%APPDATA%\NoiseGate\config.toml` — created on first run:

```toml
input_device_id = ""        # empty = default mic
output_device_id = ""        # empty = auto-detect VB-Cable
enabled = true
attenuation_db = 100.0       # 6.0 = subtle, 100.0 = max
auto_start = false
```

Logs at `%APPDATA%\NoiseGate\logs\noisegate.log`. Tune verbosity with `RUST_LOG=noisegate=debug`.

## Cargo features

| Flag | Default | What it does |
|---|---|---|
| `rnnoise` | ✅ on | RNNoise backend via `nnnoiseless`. Pure-Rust, model embedded, no extra runtime deps. |
| `onnx` | off | Adds ONNX Runtime as a dependency so you can load any raw-audio noise-suppression ONNX model (e.g. DFN3). Needs `onnxruntime.dll` on PATH. |

Build with the ONNX backend in addition to RNNoise:

```powershell
cargo build --release -p noisegate -F onnx
```

To get DeepFilterNet3 quality:
1. Build with `-F onnx`.
2. Download the DFN3 ONNX export from Hugging Face: <https://huggingface.co/Rikorose/DeepFilterNet3>.
3. Drop `onnxruntime.dll` next to `noisegate.exe` (download from <https://github.com/microsoft/onnxruntime/releases> — pick the `win-x64` zip).
4. Point `model_path` in `config.toml` at the ONNX file.

## License

Code: dual MIT / Apache-2.0 — your choice.

The bundled RNNoise model (via `nnnoiseless`) is BSD-licensed — fine for any use including commercial. If you opt into the ONNX backend with DeepFilterNet3 weights, those are research / non-commercial; for commercial use, retrain on your own data or pick a different ONNX model.

## Cost

$0 for personal use. Every component is free. No driver-signing certs required (we don't ship a driver — VB-Cable does).

## Not included (yet)

- macOS / Linux backends (the audio-io crate is Windows-only; the rest is portable).
- Far-end denoising (cleaning the audio you *hear* from the call — only your mic is cleaned).
- Acoustic echo cancellation (combine with an AEC frontend if needed).
- Auto-update.

## Credits

Built on top of excellent open-source work:

- **[RNNoise](https://gitlab.xiph.org/xiph/rnnoise)** by Jean-Marc Valin / Xiph.Org — the recurrent-network noise-suppression model that powers the default backend.
- **[`nnnoiseless`](https://github.com/jneem/nnnoiseless)** by jneem — pure-Rust port of RNNoise; this is the actual crate doing the work.
- **[DeepFilterNet](https://github.com/Rikorose/DeepFilterNet)** by Hendrik Schröter et al. — the modern speech-enhancement model that the optional ONNX backend can run.
- **[ONNX Runtime](https://onnxruntime.ai/)** + the **[`ort`](https://github.com/pykeio/ort)** Rust bindings — power the optional `onnx` backend.
- **[VB-Cable](https://vb-audio.com/Cable/)** by VB-Audio — the free virtual audio driver every Windows audio-routing app depends on.
- **[`windows`](https://github.com/microsoft/windows-rs)** crate by Microsoft — official Win32 bindings for Rust (WASAPI, MMCSS, COM).
- **[`tray-icon`](https://github.com/tauri-apps/tray-icon)** + **[`winit`](https://github.com/rust-windowing/winit)** — system-tray UI and event loop.
- **[`ringbuf`](https://github.com/agerasev/ringbuf)** — the lock-free SPSC buffers that connect the audio threads.
- Inspired by **[NoiseTorch](https://github.com/noisetorch/NoiseTorch)** (Linux-only equivalent that uses RNNoise).

## Issues & discussion

- Bug reports / feature requests: [open an issue](https://github.com/Yashsomalkar/noisegate/issues).
- Questions, ideas, "is X possible?" — [Discussions](https://github.com/Yashsomalkar/noisegate/discussions).
