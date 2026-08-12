//! Optional ONNX Runtime backend, for running a real model instead of the
//! built-in RNNoise — DeepFilterNet3 being the obvious one.
//!
//! Build with `--features onnx`. You'll need the ONNX Runtime DLL
//! (onnxruntime.dll); we use `load-dynamic` so it doesn't have to ship
//! inside the binary. Put it **next to noisegate.exe**, or point
//! `ORT_DYLIB_PATH` at it explicitly — see [`pin_dylib_path`] for why we
//! don't let the OS search for it.
//!
//! ## Expected model signature
//! Streaming, frame-at-a-time models that carry their own recurrent state,
//! which is what every usable real-time denoiser looks like:
//!
//! | tensor         | direction | shape       |
//! |----------------|-----------|-------------|
//! | `input_frame`  | in        | `[480]` f32 |
//! | `states`       | in        | `[S]` f32   |
//! | `atten_lim_db` | in        | `[1]` f32   |
//! | enhanced audio | out       | `[480]` f32 |
//! | new states     | out       | `[S]` f32   |
//!
//! Inputs are matched by name so export naming quirks don't matter; outputs
//! are positional (enhanced audio, then new state; anything after is ignored).
//! The state width `S` is read from the model, so exports with different state
//! sizes work without a recompile.
//!
//! Models with a different contract — spectral input, explicit STFT/iSTFT, a
//! multi-file encoder/decoder split — need their own front-end and aren't
//! handled here.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorRef;

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

pub struct OnnxDenoiser {
    session: Session,
    /// Input tensor names, resolved once at load so the hot path doesn't
    /// re-query them.
    frame_input: String,
    states_input: String,
    atten_input: Option<String>,
    /// Recurrent state, handed back to the model on every frame.
    states: Vec<f32>,
    /// Attenuation limit in dB. 0 = let the model suppress as much as it
    /// wants; a positive value deliberately leaves some noise in.
    atten_lim_db: f32,
    model_label: String,
    scratch_in: [f32; FRAME_SAMPLES],
}

/// Pin `onnxruntime.dll` to our own install directory unless the operator has
/// already chosen a path.
///
/// `ort`'s `load-dynamic` otherwise hands the bare filename to the OS loader,
/// whose search order includes the current working directory. Launch
/// NoiseGate from Downloads with a hostile `onnxruntime.dll` sitting there and
/// it executes inside a process that holds the microphone open. Naming the
/// full path removes the search entirely.
fn pin_dylib_path() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return; // Explicit operator choice wins.
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
    {
        std::env::set_var("ORT_DYLIB_PATH", dir.join("onnxruntime.dll"));
    }
}

/// Pick the input whose name contains `needle`, falling back to positional
/// order. Exports disagree on naming (`input_frame` vs `frame` vs `input`),
/// and guessing wrong here produces a baffling runtime error rather than a
/// clear one.
fn find_input(session: &Session, needle: &str, fallback_index: usize) -> Option<String> {
    session
        .inputs
        .iter()
        .find(|i| i.name.to_ascii_lowercase().contains(needle))
        .or_else(|| session.inputs.get(fallback_index))
        .map(|i| i.name.clone())
}

/// Number of elements a fixed-size input expects, if the model declares it.
fn declared_len(session: &Session, name: &str) -> Option<usize> {
    let input = session.inputs.iter().find(|i| i.name == name)?;
    let dims = input.input_type.tensor_shape()?;
    let total: i64 = dims.iter().filter(|d| **d > 0).product();
    (total > 0).then_some(total as usize)
}

impl OnnxDenoiser {
    /// Load an ONNX model from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        pin_dylib_path();
        let path = path.as_ref();
        let label = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "onnx-model".into());

        let session = Session::builder()
            .map_err(|e| DspError::Load(format!("ort builder: {e}")))?
            .with_intra_threads(1)
            .map_err(|e| DspError::Load(format!("with_intra_threads: {e}")))?
            .commit_from_file(path)
            .map_err(|e| DspError::Load(format!("commit_from_file({}): {e}", path.display())))?;

        let frame_input = find_input(&session, "frame", 0)
            .ok_or_else(|| DspError::Load("model has no inputs".into()))?;
        let states_input = find_input(&session, "state", 1).ok_or_else(|| {
            DspError::Load(
                "model has no state input; this loader handles streaming models that carry \
                 recurrent state (e.g. DeepFilterNet3 exports)"
                    .into(),
            )
        })?;
        // Optional — not every export exposes an attenuation limit.
        let atten_input = session
            .inputs
            .iter()
            .find(|i| i.name.to_ascii_lowercase().contains("atten"))
            .map(|i| i.name.clone());

        let state_len = declared_len(&session, &states_input).ok_or_else(|| {
            DspError::Load(format!(
                "could not determine the size of state input '{states_input}'"
            ))
        })?;

        tracing::info!(
            model = %label,
            frame_input,
            states_input,
            state_len,
            atten_input = atten_input.as_deref().unwrap_or("<none>"),
            "loaded ONNX denoiser"
        );

        Ok(Self {
            session,
            frame_input,
            states_input,
            atten_input,
            states: vec![0.0; state_len],
            atten_lim_db: 0.0,
            model_label: label,
            scratch_in: [0.0; FRAME_SAMPLES],
        })
    }

    /// Attenuation limit in dB, if the model takes one. 0 means "suppress as
    /// much as you like"; a value like 12 leaves some noise in, which some
    /// people prefer on a call.
    pub fn set_attenuation_db(&mut self, db: f32) {
        self.atten_lim_db = db.max(0.0);
    }
}

impl Denoiser for OnnxDenoiser {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.scratch_in.copy_from_slice(frame);

        let frame_tensor =
            TensorRef::from_array_view(([FRAME_SAMPLES as i64], &self.scratch_in[..]))
                .map_err(|e| DspError::Inference(format!("input_frame: {e}")))?;
        let states_tensor =
            TensorRef::from_array_view(([self.states.len() as i64], &self.states[..]))
                .map_err(|e| DspError::Inference(format!("states: {e}")))?;
        let atten = [self.atten_lim_db];
        let atten_tensor = TensorRef::from_array_view(([1_i64], &atten[..]))
            .map_err(|e| DspError::Inference(format!("atten_lim_db: {e}")))?;

        // The macro fixes the (name, value) types; the optional third input
        // is pushed onto the same vec.
        let mut inputs = ort::inputs![
            self.frame_input.as_str() => frame_tensor,
            self.states_input.as_str() => states_tensor,
        ];
        if let Some(name) = &self.atten_input {
            inputs.push((name.as_str().into(), atten_tensor.into()));
        }

        let outputs = self
            .session
            .run(inputs)
            .map_err(|e| DspError::Inference(format!("session.run: {e}")))?;

        // Outputs are positional: enhanced audio first, new state second.
        let (_, enhanced) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| DspError::Inference(format!("enhanced output: {e}")))?;
        if enhanced.len() != FRAME_SAMPLES {
            return Err(DspError::Inference(format!(
                "expected {FRAME_SAMPLES} output samples, got {}",
                enhanced.len()
            )));
        }
        frame.copy_from_slice(enhanced);

        let (_, new_states) = outputs[1]
            .try_extract_tensor::<f32>()
            .map_err(|e| DspError::Inference(format!("new states output: {e}")))?;
        if new_states.len() != self.states.len() {
            return Err(DspError::Inference(format!(
                "model returned {} state values, expected {}",
                new_states.len(),
                self.states.len()
            )));
        }
        self.states.copy_from_slice(new_states);
        Ok(())
    }

    fn name(&self) -> &'static str {
        // `&'static str` from a runtime-loaded label requires leaking the
        // string once at load. Cheap (a few bytes once per process) and
        // makes the trait simple. If you load multiple models, leak each
        // label once.
        Box::leak(self.model_label.clone().into_boxed_str())
    }
}
