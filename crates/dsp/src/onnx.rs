//! Optional ONNX Runtime backend for experimenting with arbitrary
//! noise-suppression models — including ones exported from Hugging Face
//! (e.g. `microsoft/dns_challenge`-style baselines, or your own training
//! pipeline output).
//!
//! Build with `--features onnx`. You'll need the ONNX Runtime DLL
//! (onnxruntime.dll); we use `load-dynamic` so it doesn't have to ship
//! inside the binary. Put it **next to noisegate.exe**, or point
//! `ORT_DYLIB_PATH` at it explicitly — see [`pin_dylib_path`] for why we
//! don't let the OS search for it.
//!
//! ## Expected model signature
//! For simplicity this loader supports raw-audio in/out models:
//! - input  shape: `[1, N]` or `[1, 1, N]`, where N is divisible by 480
//!   (typically N = 480, processing one frame at a time)
//! - output shape: same as input
//! - dtype:  float32
//!
//! Spectral models (e.g. DNS-style with explicit STFT/iSTFT inputs) need a
//! custom front-end and aren't supported by this generic wrapper. Plug in
//! the STFT yourself if needed.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorRef;

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

pub struct OnnxDenoiser {
    session: Session,
    /// Cached input/output tensor names (avoid re-querying per frame).
    input_name: String,
    output_name: String,
    model_label: String,
    /// Reusable input tensor backing storage so we don't allocate per frame.
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

impl OnnxDenoiser {
    /// Load an ONNX model from disk. The path is captured for diagnostics.
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

        let input_name = session
            .inputs
            .first()
            .ok_or_else(|| DspError::Load("model has no inputs".into()))?
            .name
            .clone();
        let output_name = session
            .outputs
            .first()
            .ok_or_else(|| DspError::Load("model has no outputs".into()))?
            .name
            .clone();

        Ok(Self {
            session,
            input_name,
            output_name,
            model_label: label,
            scratch_in: [0.0; FRAME_SAMPLES],
        })
    }
}

impl Denoiser for OnnxDenoiser {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        // Copy into the reusable input buffer.
        self.scratch_in.copy_from_slice(frame);

        // Borrow the scratch buffer rather than handing ownership over, so the
        // hot path stays allocation-free.
        let input_tensor = TensorRef::from_array_view((
            [1_i64, FRAME_SAMPLES as i64],
            &self.scratch_in[..],
        ))
        .map_err(|e| DspError::Inference(format!("from_array_view: {e}")))?;

        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input_tensor])
            .map_err(|e| DspError::Inference(format!("session.run: {e}")))?;

        let out = outputs
            .get(self.output_name.as_str())
            .ok_or_else(|| DspError::Inference(format!("missing output '{}'", self.output_name)))?;
        let (_shape, slice) = out
            .try_extract_tensor::<f32>()
            .map_err(|e| DspError::Inference(format!("extract: {e}")))?;
        if slice.len() != FRAME_SAMPLES {
            return Err(DspError::Inference(format!(
                "expected {} output samples, got {}",
                FRAME_SAMPLES,
                slice.len()
            )));
        }
        frame.copy_from_slice(slice);
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
