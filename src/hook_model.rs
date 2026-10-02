use anyhow::{Context, Result, anyhow, bail, ensure};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokenizers::{Tokenizer, TruncationParams, TruncationStrategy};

pub(crate) const MODEL_SHA256: &str =
    "d61e7db485fdc3cae445e674194b528fa424d118320845d8a24b0c67781e4442";
const MODEL_URL: &str = "https://raw.githubusercontent.com/jeffg-dev/commentreducr/main/models/python-hook-minilm-l12-v1/model.onnx";
const MODEL_BYTES: u64 = 34_168_774;
const TOKENIZER: &[u8] = include_bytes!("model/minilm-l12-tokenizer.json");
const CONFIG: &str = include_str!("model/minilm-l12-classifier.json");

#[derive(Deserialize)]
struct ModelConfig {
    threshold: f64,
    max_length: usize,
    x64_quant_precision: String,
}

pub fn model_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("COMMENTREDUCR_MODEL_PATH") {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .context("cannot locate model cache; set COMMENTREDUCR_MODEL_PATH")?;
    Ok(base.join("commentreducr/python-hook-minilm-l12-v1/model.onnx"))
}

fn verify(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 == MODEL_BYTES,
        "unexpected MiniLM-L12 model size"
    );
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == MODEL_SHA256,
        "MiniLM-L12 model checksum mismatch"
    );
    Ok(())
}

pub fn install_model() -> Result<PathBuf> {
    let path = model_path()?;
    if path.exists() {
        verify(&std::fs::read(&path)?)?;
        return Ok(path);
    }
    println!("Downloading MiniLM-L12 (34 MB) to {}", path.display());
    let response = minreq::get(MODEL_URL)
        .with_timeout(60)
        .send()
        .context("cannot download MiniLM-L12")?;
    ensure!(
        response.status_code == 200,
        "model download returned HTTP {}",
        response.status_code
    );
    verify(response.as_bytes())?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("onnx.{}.tmp", std::process::id()));
    std::fs::write(&temporary, response.as_bytes())?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).context("cannot save downloaded model");
    }
    Ok(path)
}

pub struct Classifier {
    tokenizer: Tokenizer,
    session: Session,
    config: ModelConfig,
}

impl Classifier {
    pub fn load() -> Result<Self> {
        let path = model_path()?;
        Self::from_file(&path)
            .context("run `commentreducr install-git-hook` once to prepare the model")
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        verify(&bytes)?;
        let config: ModelConfig = serde_json::from_str(CONFIG)?;
        let mut tokenizer = Tokenizer::from_bytes(TOKENIZER).map_err(|e| anyhow!("{e}"))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow!("{e}"))?;
        ort::init().with_telemetry(false).commit();
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| anyhow!("{e}"))?
            // Avoid saturating U8S8 matrix multiplication on x86 CPUs without VNNI.
            .with_config_entry("session.x64quantprecision", &config.x64_quant_precision)?
            .with_intra_threads(4)
            .map_err(|e| anyhow!("{e}"))?
            .with_inter_threads(1)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_memory(&bytes)?;
        Ok(Self {
            tokenizer,
            session,
            config,
        })
    }

    pub fn threshold(&self) -> f64 {
        self.config.threshold
    }

    pub fn probability(&mut self, kind: &str, text: &str, context: &str) -> Result<f64> {
        let target = format!("Kind: {kind}\nText:\n{text}");
        let context = format!("Python context:\n{context}");
        self.tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow!("{e}"))?;
        let target_tokens = self
            .tokenizer
            .encode(target.as_str(), false)
            .map_err(|e| anyhow!("{e}"))?;
        if target_tokens.len() + 3 >= self.config.max_length {
            bail!(
                "block exceeds the model's {}-token limit; review it manually",
                self.config.max_length
            );
        }
        self.tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: self.config.max_length,
                strategy: TruncationStrategy::OnlySecond,
                ..Default::default()
            }))
            .map_err(|e| anyhow!("{e}"))?;
        let encoded = self
            .tokenizer
            .encode((target.as_str(), context.as_str()), true)
            .map_err(|e| anyhow!("{e}"))?;
        let shape = [1, encoded.len()];
        let tensor = |values: &[u32]| {
            Tensor::from_array((
                shape,
                values.iter().map(|&v| i64::from(v)).collect::<Vec<_>>(),
            ))
        };
        // One block per invocation preserves the frozen export's dynamic quantization policy.
        let output = self.session.run(ort::inputs![
            "input_ids" => tensor(encoded.get_ids())?,
            "attention_mask" => tensor(encoded.get_attention_mask())?,
            "token_type_ids" => tensor(encoded.get_type_ids())?,
        ])?;
        let (_, logits) = output["logits"].try_extract_tensor::<f32>()?;
        ensure!(
            logits.len() == 2 && logits.iter().all(|v| v.is_finite()),
            "invalid classifier output"
        );
        let shift = logits[0].max(logits[1]);
        let pass = (logits[0] - shift).exp();
        let flag = (logits[1] - shift).exp();
        Ok(f64::from(flag / (pass + flag)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_model_matches_reference() {
        let model = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("models/python-hook-minilm-l12-v1/model.onnx");
        let mut classifier = Classifier::from_file(&model).unwrap();
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("model/minilm-l12-reference.json")).unwrap();
        for row in fixtures {
            let score = classifier
                .probability(
                    row["kind"].as_str().unwrap(),
                    row["text"].as_str().unwrap(),
                    row["context"].as_str().unwrap(),
                )
                .unwrap();
            let pair = (
                format!(
                    "Kind: {}\nText:\n{}",
                    row["kind"].as_str().unwrap(),
                    row["text"].as_str().unwrap()
                ),
                format!("Python context:\n{}", row["context"].as_str().unwrap()),
            );
            let tokens = classifier
                .tokenizer
                .encode((pair.0.as_str(), pair.1.as_str()), true)
                .unwrap();
            assert_eq!(
                serde_json::to_value(tokens.get_ids()).unwrap(),
                row["input_ids"],
                "token IDs differ: {}",
                row["id"]
            );
            assert_eq!(
                serde_json::to_value(tokens.get_type_ids()).unwrap(),
                row["token_type_ids"]
            );
            let expected = row["score"].as_f64().unwrap();
            // CPU runtime builds can differ numerically; keep exact tokenization and stable decisions.
            assert!(
                (score - expected).abs() < 0.02,
                "{}: {score} != {expected}",
                row["id"]
            );
            if (expected - classifier.threshold()).abs() > 0.02 {
                assert_eq!(
                    score >= classifier.threshold(),
                    expected >= classifier.threshold()
                );
            }
        }
        assert!(
            classifier
                .probability("comment", &"word ".repeat(500), "pass")
                .is_err()
        );
    }
}
