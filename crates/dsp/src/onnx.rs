//! Optional ONNX Runtime backend for experimenting with arbitrary
//! noise-suppression models — including ones exported from Hugging Face
//! (e.g. `microsoft/dns_challenge`-style baselines, or your own training
//! pipeline output).
//!
//! Build with `--features onnx`. You'll need the ONNX Runtime DLL
//! (onnxruntime.dll) on PATH or in the working directory; we use
//! `load-dynamic` so the DLL doesn't have to ship inside the binary.
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

use ndarray::Array2;
use ort::{session::Session, value::Value};

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

pub struct OnnxDenoiser {
    session: Session,
    /// Cached input/output tensor names (avoid re-querying per frame).
    input_name: String,
    output_name: String,
    model_label: String,
    /// Reusable input tensor backing storage so we don't allocate per frame.
    scratch_in: Array2<f32>,
    /// Reusable output backing.
    scratch_out: Vec<f32>,
}

impl OnnxDenoiser {
    /// Load an ONNX model from disk. The path is captured for diagnostics.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
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
            scratch_in: Array2::<f32>::zeros((1, FRAME_SAMPLES)),
            scratch_out: vec![0.0; FRAME_SAMPLES],
        })
    }
}

impl Denoiser for OnnxDenoiser {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        // Copy into the reusable input view.
        self.scratch_in
            .as_slice_mut()
            .ok_or_else(|| DspError::Inference("input slice not contiguous".into()))?
            .copy_from_slice(frame);

        let input_tensor = Value::from_array(self.scratch_in.view())
            .map_err(|e| DspError::Inference(format!("from_array: {e}")))?;

        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input_tensor]
                .map_err(|e| DspError::Inference(format!("inputs!: {e}")))?)
            .map_err(|e| DspError::Inference(format!("session.run: {e}")))?;

        let out = outputs
            .get(self.output_name.as_str())
            .ok_or_else(|| DspError::Inference(format!("missing output '{}'", self.output_name)))?;
        let view = out
            .try_extract_tensor::<f32>()
            .map_err(|e| DspError::Inference(format!("extract: {e}")))?;
        let slice = view
            .as_slice()
            .ok_or_else(|| DspError::Inference("output not contiguous".into()))?;
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
