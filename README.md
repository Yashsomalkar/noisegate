# NoiseGate

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%2F11-0078d4.svg)](#install)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Model: DeepFilterNet3 + RNNoise](https://img.shields.io/badge/model-DeepFilterNet3%20%2B%20RNNoise-purple.svg)](#noise-suppression-models)
[![Latest release](https://img.shields.io/github/v/release/Yashsomalkar/noisegate?include_prereleases&label=download)](https://github.com/Yashsomalkar/noisegate/releases)

Real-time microphone noise cancellation for Windows. Pure-Rust inference, WASAPI low-latency capture/render, a system-tray UI. NoiseGate writes your cleaned voice into a virtual microphone so any app (TeamSpeak, Discord, Zoom, Teams, OBS, browser calls) sees a noise-free mic.

> Status: alpha. Builds on Windows 10/11 with the MSVC toolchain.

## Why

Existing noise-cancellation tools either cost money (Krisp), require an RTX GPU (NVIDIA Broadcast), or are Linux-only (NoiseTorch). NoiseGate is a free, open-source alternative for Windows that runs on any CPU and ships a state-of-the-art model (DeepFilterNet3) out of the box.

## Stack

- **Audio I/O:** WASAPI shared mode, event-driven, MMCSS Pro Audio scheduling. Recovers on its own when a device resets or is unplugged.
- **Routing:** any supported virtual cable, detected automatically: [VB-Cable](https://vb-audio.com/Cable/), VoiceMeeter, or the Steam Streaming Microphone.
- **Denoisers:** **DeepFilterNet3** by default (via [`deep-filter-rt`](https://crates.io/crates/deep-filter-rt), pure Rust on `tract`, model embedded), **RNNoise** as a lightweight alternative (via [`nnnoiseless`](https://github.com/jneem/nnnoiseless)). Switchable from the tray.
- **UI:** `tray-icon` + `winit`; the settings panel is drawn with [`egui`](https://github.com/emilk/egui) on OpenGL. No console window.
- **Lang:** Rust 2021, a single self-contained `.exe` (no DLLs).

## Architecture

```
physical mic ─► WASAPI capture ─► ring A ─► DSP thread (denoiser) ─► ring B ─► WASAPI render ─► virtual cable (speaker side)
                                                                                                        │
                                                    other apps pick the cable's microphone side as mic ◄┘
```

Three dedicated MMCSS-priority threads. Lock-free SPSC ring buffers (8 frames ≈ 80 ms headroom). 480-sample (10 ms) frames end-to-end, the native frame size of both RNNoise and DeepFilterNet. Capture and render convert to and from the devices' own formats (any rate, any channel count, 32-bit float).

## Noise suppression models

Pick the model and strength in the tray menu; the choice is saved.

| Model | Default? | Quality | CPU (one core) | Latency |
|---|---|---|---|---|
| **DeepFilterNet3** | ✅ | ★★★★★ Removes fans, keyboards, traffic, music and other voices in the background. Full-band 48 kHz. | ~3% | +30 ms |
| **RNNoise** | | ★★★ Good on steady noise (fans, hum), weaker on everything else. | <1% | +10 ms |

DeepFilterNet3 has three strengths: **Maximum** (no limit), **Strong** (up to 24 dB less noise), and **Natural** (up to 12 dB, keeps some room tone).

## Install

### Option A — Download a prebuilt `.exe` (once releases are cut)

Grab the latest binary from the **[Releases page](https://github.com/Yashsomalkar/noisegate/releases)**, then continue at [Set up a virtual microphone](#set-up-a-virtual-microphone).

### Option B — Build from source

You need, on Windows 10/11:

| # | Component | Download | Notes |
|---|---|---|---|
| 1 | **Rust toolchain** | [rustup-init.exe](https://win.rustup.rs/x86_64) or [rustup.rs](https://rustup.rs/) | The default (`stable`, `x86_64-pc-windows-msvc`). |
| 2 | **MSVC C++ build tools** | [Build Tools for Visual Studio 2022](https://visualstudio.microsoft.com/visual-cpp-build-tools/) | Check **"Desktop development with C++"**. |
| 3 | **Git** | [git-scm.com/download/win](https://git-scm.com/download/win) | To clone the repo. |

Then in PowerShell:

```powershell
git clone https://github.com/Yashsomalkar/noisegate.git
cd noisegate
cargo build --release
.\target\release\noisegate.exe
```

The first build downloads the crates and takes a while (the release profile uses full LTO). For faster local builds use `cargo build --profile fast` (thin LTO, output in `target\fast`).

### Set up a virtual microphone

NoiseGate needs a virtual cable to hand your cleaned voice to other apps. It picks the first one it finds:

- **VB-Cable** (recommended, free): [vb-audio.com/Cable](https://vb-audio.com/Cable/). Unzip, right-click `VBCABLE_Setup_x64.exe` → *Run as administrator*, reboot if asked. In other apps, pick **CABLE Output (VB-Audio Virtual Cable)** as the mic.
- **VoiceMeeter**: pick **VoiceMeeter Output** as the mic.
- **Steam Streaming Microphone** (installed with Steam): pick **Microphone (Steam Streaming Microphone)** as the mic. This cable doesn't convert formats, so its microphone side must use the same format as its speaker side (usually 48 kHz stereo). NoiseGate detects a mismatch, shows a warning, and walks you through fixing it in the Sound control panel.

If none is installed, NoiseGate offers to open the VB-Cable download page. After installing one, choose **Refresh devices** in the tray menu. The menu always shows what to select in other apps.

Then, in TeamSpeak / Discord / Zoom / Teams / OBS, choose the virtual mic as your microphone, and turn off the app's own noise suppression so the two don't fight.

> **Sanity test**: open the Windows **Voice Recorder**, set its mic to the virtual mic, and record 10 seconds with a fan or typing in the background. Turn noise removal off with the switch in the NoiseGate panel and record again. The difference should be obvious.

## The tray

The tray icon shows the state, and morphs smoothly between states:

| Icon | Meaning |
|---|---|
| Teal, wiggly line turning straight | Removing noise. |
| Grey, wiggly line | Noise removal off: your mic passes through unfiltered. |
| Amber, "!" | Needs attention: no virtual mic, a cable format mismatch, or the audio device stopped (NoiseGate retries every few seconds). |

**Left-click** the icon to open the settings panel above the taskbar. It stays open while you change things and closes when you click elsewhere or press Esc:

- **On/off switch** and the current status.
- **Microphone**: which physical mic to clean (system default or a specific one).
- **Virtual mic (output)**: which cable to write to (automatic, a specific cable, or any output device), with what to select in other apps, or a button to fix or install a virtual mic.
- **Noise suppression**: model and strength, see [Noise suppression models](#noise-suppression-models).
- **Start with Windows**: start NoiseGate when you log in (a per-user entry, visible in Task Manager → Startup apps).
- **Refresh devices** (after plugging in a mic or installing a cable), **Logs**, **Quit**, and the CPU use.

**Right-click** opens a menu with the same settings for quick access; clicking its header opens the panel.

The panel and menu follow Windows' light or dark app mode. Hovering the icon shows the status, model, strength and CPU use.

## Command line

NoiseGate normally runs without a console. When started from a terminal it prints its log there.

```powershell
.\noisegate.exe --list-devices       # list inputs and outputs, mark virtual cables
.\noisegate.exe --mic "USB"          # use the mic whose name contains "USB" (this run only)
.\noisegate.exe --out "CABLE"        # write to a specific output device (this run only)
.\noisegate.exe --bypass             # start with noise removal off
.\noisegate.exe --console            # open a window with the live log
```

> **Bluetooth headsets**: opening a Bluetooth headset's mic switches it into Hands-Free mode (16 kHz mono, sounds bad). That's Windows behaviour; pick a USB or built-in mic instead.

## Configuration

Settings changed in the tray are saved to `%APPDATA%\NoiseGate\config.toml`:

```toml
input_device_id = ""          # empty = system default mic
output_device_id = ""         # empty = first virtual cable found
enabled = true                # noise removal on/off
denoiser = "deepfilter"       # "deepfilter" or "rnnoise"
attenuation_db = 24.0         # DeepFilterNet3 limit: 100 = Maximum, 24 = Strong, 12 = Natural
vbcable_prompt_dismissed = false
auto_start = false
```

Delete the file (with NoiseGate closed) to go back to the defaults. Logs are at `%APPDATA%\NoiseGate\logs\noisegate.log`; tune verbosity with `RUST_LOG=noisegate=debug`.

**Diagnostics:** `NOISEGATE_TEST_TONE=1000` replaces the mic with a 1 kHz sine (at −40 dBFS, or `NOISEGATE_TEST_TONE_DB`) to check the output chain's pitch and continuity.

## Cargo features

Features of the `noisegate-dsp` crate:

| Flag | Default | What it does |
|---|---|---|
| `deepfilter` | ✅ on | DeepFilterNet3 via `deep-filter-rt`. Pure Rust, model embedded, no runtime DLL. |
| `rnnoise` | ✅ on | RNNoise via `nnnoiseless`. Pure Rust, model embedded. |
| `onnx` | off | ONNX Runtime backend for loading other raw-audio noise-suppression ONNX models. Needs `onnxruntime.dll` next to the exe. |

`crates/dsp/examples/bench.rs` measures each backend's speed and SI-SDR on synthetic noise (`NOISE=white|clicks|both`).

## License

Code: dual MIT / Apache-2.0, your choice.

RNNoise (via `nnnoiseless`) is BSD-licensed. DeepFilterNet and the `deep-filter-rt` crate with its embedded DeepFilterNet3 model are MIT / Apache-2.0. All are fine for any use, including commercial.

## Cost

$0. Every component is free. No driver-signing certificate needed: NoiseGate doesn't ship a driver.

## Not included (yet)

- macOS / Linux backends (the audio-io crate is Windows-only; the rest is portable).
- Far-end denoising (cleaning the audio you *hear*; only your mic is cleaned).
- Acoustic echo cancellation.
- Auto-update.

## Credits

Built on top of excellent open-source work:

- **[DeepFilterNet](https://github.com/Rikorose/DeepFilterNet)** by Hendrik Schröter et al. — the speech-enhancement model behind the default backend, and **[`deep-filter-rt`](https://crates.io/crates/deep-filter-rt)**, which runs it in pure Rust.
- **[RNNoise](https://gitlab.xiph.org/xiph/rnnoise)** by Jean-Marc Valin / Xiph.Org, and **[`nnnoiseless`](https://github.com/jneem/nnnoiseless)** by jneem, its pure-Rust port.
- **[ONNX Runtime](https://onnxruntime.ai/)** + the **[`ort`](https://github.com/pykeio/ort)** bindings — the optional `onnx` backend.
- **[VB-Cable](https://vb-audio.com/Cable/)** by VB-Audio — the free virtual audio driver.
- **[`windows`](https://github.com/microsoft/windows-rs)** by Microsoft — Win32 bindings (WASAPI, MMCSS, COM).
- **[`tray-icon`](https://github.com/tauri-apps/tray-icon)** + **[`winit`](https://github.com/rust-windowing/winit)** — tray UI and event loop.
- **[`egui`](https://github.com/emilk/egui)** + **[`glutin`](https://github.com/rust-windowing/glutin)** — the settings panel.
- **[`ringbuf`](https://github.com/agerasev/ringbuf)** — the lock-free buffers between the audio threads.
- Inspired by **[NoiseTorch](https://github.com/noisetorch/NoiseTorch)** (the Linux equivalent).

## Issues & discussion

- Bug reports / feature requests: [open an issue](https://github.com/Yashsomalkar/noisegate/issues).
- Questions and ideas: [Discussions](https://github.com/Yashsomalkar/noisegate/discussions).
