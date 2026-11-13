//! Offline quality/speed check for the denoiser backends.
//!
//! `cargo run --release -p noisegate-dsp --example bench -- clean.wav [out_dir]`
//!
//! Takes a clean 48 kHz mono 16-bit WAV of speech, mixes in white noise and
//! keyboard-like clicks, runs every compiled-in backend over it frame by
//! frame (exactly like the pipeline does), and reports:
//! - SI-SDR of the noisy input and of each output against the clean speech
//!   (higher = better; the gain is what the denoiser bought you),
//! - residual noise level in the non-speech gaps,
//! - average / peak time per 10 ms frame.
//! With `out_dir`, writes noisy.wav and one WAV per backend for listening.

use std::time::Instant;

use noisegate_dsp::{denoiser_by_key, BACKENDS, FRAME_SAMPLES};

fn read_wav_mono16(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).expect("read wav");
    assert_eq!(&b[0..4], b"RIFF");
    let mut i = 12;
    let (mut ch, mut bits, mut rate) = (0u16, 0u16, 0u32);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " {
            ch = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
        } else if id == b"data" {
            assert!(ch == 1 && bits == 16 && rate == 48_000, "need 48 kHz mono 16-bit");
            return body
                .chunks_exact(2)
                .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)
                .collect();
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn write_wav_mono16(path: &std::path::Path, x: &[f32]) {
    let data: Vec<u8> = x
        .iter()
        .flat_map(|&s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&96_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::fs::write(path, out).expect("write wav");
}

/// Scale-invariant SDR of `est` against `reference`, in dB.
fn si_sdr(reference: &[f32], est: &[f32]) -> f64 {
    let dot: f64 = reference.iter().zip(est).map(|(&r, &e)| r as f64 * e as f64).sum();
    let rr: f64 = reference.iter().map(|&r| (r as f64).powi(2)).sum();
    let alpha = dot / rr.max(1e-12);
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&r, &e) in reference.iter().zip(est) {
        let t = alpha * r as f64;
        sig += t * t;
        err += (e as f64 - t).powi(2);
    }
    10.0 * (sig / err.max(1e-12)).log10()
}

/// Output lag (in samples) that best lines `est` up with `reference`.
fn best_lag(reference: &[f32], est: &[f32], max_lag: usize) -> usize {
    (0..=max_lag)
        .max_by(|&a, &b| {
            let c = |lag: usize| -> f64 {
                reference
                    .iter()
                    .zip(&est[lag..])
                    .map(|(&r, &e)| r as f64 * e as f64)
                    .sum()
            };
            c(a).partial_cmp(&c(b)).unwrap()
        })
        .unwrap()
}

fn db(x: f64) -> f64 {
    10.0 * x.max(1e-20).log10()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let clean = read_wav_mono16(&args[1]);
    let out_dir = args.get(2).map(std::path::PathBuf::from);

    // Deterministic noise: white noise at -38 dBFS RMS plus a sharp "key
    // click" every 170 ms.
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    // NOISE=white | clicks | both (default both)
    let kind = std::env::var("NOISE").unwrap_or_else(|_| "both".into());
    println!("noise: {kind}");
    let white_rms = if kind == "clicks" { 0.0 } else { 10f32.powf(-38.0 / 20.0) };
    let mut noise: Vec<f32> = (0..clean.len()).map(|_| rnd() * white_rms * 3f32.sqrt()).collect();
    let click_every = if kind == "white" { usize::MAX } else { 8160 };
    for start in (0..noise.len()).step_by(click_every) {
        for k in 0..240.min(noise.len() - start) {
            noise[start + k] += 0.25 * (-(k as f32) / 30.0).exp() * rnd();
        }
    }
    let noisy: Vec<f32> = clean.iter().zip(&noise).map(|(c, n)| c + n).collect();

    // Non-speech regions: 20 ms windows where the clean signal is below -50 dBFS.
    let win = 960;
    let quiet: Vec<bool> = clean
        .chunks(win)
        .map(|w| db(w.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / w.len() as f64) < -50.0)
        .collect();
    let residual_db = |sig: &[f32]| {
        let (mut e, mut n) = (0.0f64, 0usize);
        for (w, &q) in sig.chunks(win).zip(&quiet) {
            if q {
                e += w.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
                n += w.len();
            }
        }
        db(e / n.max(1) as f64)
    };

    let n_frames = clean.len() / FRAME_SAMPLES;
    let len = n_frames * FRAME_SAMPLES;
    println!(
        "{:.1} s of speech, {} frames, {:.0}% non-speech",
        clean.len() as f64 / 48_000.0,
        n_frames,
        100.0 * quiet.iter().filter(|&&q| q).count() as f64 / quiet.len() as f64
    );
    println!(
        "{:<32} {:>9} {:>14} {:>10} {:>10}",
        "", "SI-SDR dB", "gap noise dBFS", "avg ms", "peak ms"
    );
    println!(
        "{:<32} {:>9.1} {:>14.1}",
        "noisy input",
        si_sdr(&clean[..len], &noisy[..len]),
        residual_db(&noisy[..len])
    );
    if let Some(d) = &out_dir {
        std::fs::create_dir_all(d).unwrap();
        write_wav_mono16(&d.join("noisy.wav"), &noisy[..len]);
    }

    for b in BACKENDS {
        for &atten in if b.key == "deepfilter" { &[100.0f32, 24.0][..] } else { &[100.0][..] } {
            let mut den = denoiser_by_key(b.key, atten).expect("load backend");
            let mut out = vec![0f32; len];
            let mut times = Vec::with_capacity(n_frames);
            for f in 0..n_frames {
                let mut frame = [0f32; FRAME_SAMPLES];
                frame.copy_from_slice(&noisy[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES]);
                let t = Instant::now();
                den.process_frame(&mut frame).expect("process");
                times.push(t.elapsed().as_secs_f64() * 1e3);
                out[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES].copy_from_slice(&frame);
            }
            // Sanity check: the same backend on the clean speech alone should
            // leave it nearly untouched.
            let mut den_clean = denoiser_by_key(b.key, atten).expect("load backend");
            let mut out_clean = vec![0f32; len];
            for f in 0..n_frames {
                let mut frame = [0f32; FRAME_SAMPLES];
                frame.copy_from_slice(&clean[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES]);
                den_clean.process_frame(&mut frame).expect("process");
                out_clean[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES].copy_from_slice(&frame);
            }
            let lag_c = best_lag(&clean[..len - 4800], &out_clean, 4800);
            println!(
                "  {:<30} clean-in SI-SDR {:.1} dB (lag {})",
                "",
                si_sdr(&clean[..len - lag_c], &out_clean[lag_c..]),
                lag_c
            );
            let lag = best_lag(&clean[..len - 4800], &out, 4800);
            let aligned = &out[lag..];
            let n = aligned.len();
            let label = if b.key == "deepfilter" {
                format!("{} @{}dB", b.label, atten)
            } else {
                b.label.to_string()
            };
            println!(
                "{:<32} {:>9.1} {:>14.1} {:>10.2} {:>10.2}   (latency {} samples = {:.0} ms)",
                label,
                si_sdr(&clean[..n], aligned),
                residual_db(aligned),
                times.iter().sum::<f64>() / times.len() as f64,
                times.iter().cloned().fold(0.0, f64::max),
                lag,
                lag as f64 / 48.0
            );
            if let Some(d) = &out_dir {
                write_wav_mono16(&d.join(format!("{}_{}.wav", b.key, atten as u32)), aligned);
            }
        }
    }
}
