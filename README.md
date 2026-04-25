# NoiseGate

Real-time microphone noise cancellation for Windows. DeepFilterNet3 inference in pure Rust, wired to WASAPI capture/render and a system-tray UI. Pipes cleaned audio into VB-Cable so any app (Zoom, Teams, Discord, OBS, browser calls) sees a noise-free mic.

> Status: pre-alpha scaffold. Builds on Windows 10/11 with the MSVC toolchain. Tested on (none yet — you're the first).

## Why

Existing noise-cancellation tools either cost money (Krisp), require an RTX GPU (NVIDIA Broadcast), are Linux-only (NoiseTorch), or use a dated model (RNNoise). This is a free, open-source, lightweight alternative for Windows using a state-of-the-art real-time model.

## Stack

- **Audio I/O:** WASAPI shared low-latency, event-driven, MMCSS Pro Audio scheduling.
- **Routing:** VB-Cable (free virtual audio cable, [vb-audio.com](https://vb-audio.com/Cable/)).
- **DSP model:** DeepFilterNet3 via the `df` Rust crate (pure-Rust inference through `tract`).
- **UI:** `tray-icon` + `winit` event loop.
- **Lang:** Rust 2021, single static binary, ~15 MB stripped.

## Architecture

```
physical mic ─► WASAPI capture ─► ring A ─► DSP thread (DfTract) ─► ring B ─► WASAPI render ─► VB-Cable Input
                                                                                                       │
                                                              other apps choose "CABLE Output" as mic ◄┘
```

Three dedicated MMCSS-priority threads. Lock-free SPSC ring buffers (8 frames ≈ 80 ms headroom). 480-sample (10 ms) frames end-to-end — DeepFilterNet's native hop, so no reblocking inside the DSP path.

## Build & run (Windows)

Prereqs:
- Rust stable (`rustup install stable`)
- MSVC build tools (`Visual Studio Build Tools 2022` with the C++ workload)
- VB-Cable installed: <https://vb-audio.com/Cable/>

```powershell
git clone https://github.com/Yashsomalkar/noisegate.git
cd noisegate
cargo build --release
.\target\release\noisegate.exe
```

A tray icon appears. Right-click it for the menu (Enable, Open log folder, Quit).

In Windows Sound Settings, set **CABLE Output** as the input device for the app you care about (Zoom, Teams, Discord). NoiseGate captures from your real mic and writes the cleaned signal into **CABLE Input**, which appears to other apps as **CABLE Output**.

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

## Features

| Flag | Default | What it does |
|---|---|---|
| `dfnet3` | on | DeepFilterNet3 backend (research weights, fine for personal use). |
| `onnx` | off | Optional ONNX Runtime path for swapping in arbitrary HF-exported models. Requires `onnxruntime.dll` on PATH. |

Build with experimental ONNX:
```powershell
cargo build --release -p noisegate --no-default-features -F dfnet3,onnx
```

## License

Code: dual MIT / Apache-2.0 — your choice.

DeepFilterNet3 model weights ship under the `df` crate and are licensed for **research / non-commercial** use. If you want to use this commercially, either retrain on your own data or switch to the (slightly older) DeepFilterNet2 weights which are MIT.

## Cost

$0 for personal use. Every component is free. No driver-signing certs required (we don't ship a driver — VB-Cable does).

## Not included (yet)

- macOS / Linux backends (the audio-io crate is Windows-only; the rest is portable).
- Far-end denoising (cleaning the audio you *hear* from the call — only your mic is cleaned).
- Acoustic echo cancellation (combine with an AEC frontend if needed).
- Auto-update.
