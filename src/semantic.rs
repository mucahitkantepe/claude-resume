//! Semantic search: sessions are embedded with a small local model (bge-small-en-v1.5, run with
//! candle) and ranked by cosine similarity to the query.
//!
//! The model is only ever downloaded by `claude-resume embed`, after the user agrees. Everything
//! else checks the local cache and fails with a hint instead of touching the network.

use crate::store::Store;
use crate::text;
use anyhow::Result;

pub const MODEL_REPO: &str = "BAAI/bge-small-en-v1.5";
/// Identifies how stored vectors were computed. Changing the model, pooling or the text that is
/// embedded must change this, so stale vectors get recomputed.
pub const MODEL_KEY: &str = "bge-small-en-v1.5/cls/title+prompts-6000/v1";
pub const MODEL_SIZE: &str = "~133 MB";
/// What is embedded per session: its title and the start of what the user asked.
pub const PROMPT_CHARS: usize = 6_000;
/// Chunk size for embedding (bge-small reads at most 512 tokens per chunk).
pub const CHUNK_CHARS: usize = 1_500;
/// Results below this cosine similarity are dropped.
pub const MIN_SIMILARITY: f32 = 0.45;
pub const MAX_RESULTS: usize = 50;

/// Where the model lives inside the models directory (the Hugging Face cache layout).
pub fn model_dir(models: &std::path::Path) -> std::path::PathBuf {
    models.join(format!("models--{}", MODEL_REPO.replace('/', "--")))
}

/// The text a session is embedded from.
pub fn session_text(title: &str, prompts: &str) -> String {
    format!("{title}\n{}", text::cap(prompts, PROMPT_CHARS))
}

/// A stable 64-bit FNV-1a hash (std's hasher is not stable across releases).
pub fn text_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Split `s` into pieces of at most `max_chars` characters, preferring to break at whitespace.
pub fn chunks(s: &str, max_chars: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let head = text::cap(rest, max_chars);
        let cut = if head.len() == rest.len() {
            head.len()
        } else {
            match head.rfind(char::is_whitespace) {
                Some(i) if i > head.len() / 2 => i,
                _ => head.len(),
            }
        };
        out.push(rest[..cut].trim());
        rest = rest[cut..].trim_start();
    }
    out.retain(|c| !c.is_empty());
    out
}

pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Mean of the vectors, L2-normalised.
pub fn mean_normalized(vectors: &[Vec<f32>]) -> Vec<f32> {
    let Some(dim) = vectors.first().map(Vec::len) else {
        return Vec::new();
    };
    let mut mean = vec![0.0; dim];
    for v in vectors {
        for (m, x) in mean.iter_mut().zip(v) {
            *m += x;
        }
    }
    normalize(&mut mean);
    mean
}

/// Cosine similarity of two L2-normalised vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_bytes(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Sessions whose stored vector is missing or was computed from other text or another model:
/// `(sid, text to embed, hash of that text)`.
pub fn stale(store: &Store) -> Result<Vec<(String, String, String)>> {
    Ok(store
        .embedding_sources(PROMPT_CHARS)?
        .into_iter()
        .filter_map(|(sid, title, prompts, model, hash)| {
            let text = session_text(&title, &prompts);
            let new_hash = text_hash(&text);
            let fresh =
                model.as_deref() == Some(MODEL_KEY) && hash.as_deref() == Some(new_hash.as_str());
            (!fresh).then_some((sid, text, new_hash))
        })
        .collect())
}

#[cfg(feature = "semantic")]
pub use model::{Embedder, Engine, model_cached};

#[cfg(not(feature = "semantic"))]
pub fn model_cached(_models: &std::path::Path) -> bool {
    false
}

#[cfg(feature = "semantic")]
mod model {
    use super::*;
    use anyhow::{Context, bail};
    use candle_core::{Device, IndexOp, Tensor};
    use candle_nn::VarBuilder;
    use candle_transformers::models::bert::{BertModel, Config, DTYPE, HiddenAct};
    use std::path::{Path, PathBuf};
    use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

    const FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
    /// bge's instruction for retrieval queries (passages are embedded without it).
    const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
    const BATCH: usize = 16;

    /// Whether the model is in the local cache. Never touches the network.
    pub fn model_cached(models: &Path) -> bool {
        let repo = hf_hub::Cache::new(models.to_path_buf()).model(MODEL_REPO.to_string());
        FILES.iter().all(|f| repo.get(f).is_some())
    }

    fn model_files(models: &Path, allow_download: bool) -> Result<Vec<PathBuf>> {
        let cached = hf_hub::Cache::new(models.to_path_buf()).model(MODEL_REPO.to_string());
        if let Some(files) = FILES
            .iter()
            .map(|f| cached.get(f))
            .collect::<Option<Vec<_>>>()
        {
            return Ok(files);
        }
        if !allow_download {
            bail!(
                "the semantic search model isn't downloaded yet; run `claude-resume embed` ({MODEL_SIZE}, one time)"
            );
        }
        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_cache_dir(models.to_path_buf())
            .build()?;
        let repo = api.model(MODEL_REPO.to_string());
        FILES
            .iter()
            .map(|f| {
                repo.get(f)
                    .with_context(|| format!("downloading {MODEL_REPO}/{f}"))
            })
            .collect()
    }

    pub struct Embedder {
        model: BertModel,
        tokenizer: Tokenizer,
        device: Device,
    }

    impl Embedder {
        /// Load the model from the local cache, downloading it first only if `allow_download`.
        pub fn load(models: &Path, allow_download: bool) -> Result<Self> {
            let files = model_files(models, allow_download)?;
            let device = if cfg!(feature = "metal") {
                Device::new_metal(0).unwrap_or(Device::Cpu)
            } else {
                Device::Cpu
            };
            let mut config: Config = serde_json::from_str(&std::fs::read_to_string(&files[0])?)?;
            config.hidden_act = HiddenAct::GeluApproximate;
            let mut tokenizer = Tokenizer::from_file(&files[1])
                .map_err(|e| anyhow::anyhow!("loading tokenizer: {e}"))?;
            tokenizer
                .with_padding(Some(PaddingParams::default()))
                .with_truncation(Some(TruncationParams {
                    max_length: 512,
                    ..TruncationParams::default()
                }))
                .map_err(|e| anyhow::anyhow!("configuring tokenizer: {e}"))?;
            // SAFETY: the weights file is only read, and nothing else mutates it while mapped.
            let vb = unsafe { VarBuilder::from_mmaped_safetensors(&files[2..], DTYPE, &device)? };
            let model = BertModel::load(vb, &config)?;
            Ok(Self {
                model,
                tokenizer,
                device,
            })
        }

        pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
            let mut v = self.embed(&[format!("{QUERY_PREFIX}{query}")])?;
            Ok(v.remove(0))
        }

        /// One vector for a session: the mean of its chunks' vectors.
        pub fn embed_session(&self, text: &str) -> Result<Vec<f32>> {
            let chunks = chunks(text, CHUNK_CHARS);
            if chunks.is_empty() {
                bail!("nothing to embed");
            }
            Ok(mean_normalized(&self.embed(&chunks)?))
        }

        /// L2-normalised CLS embeddings (bge is trained for CLS pooling), in batches.
        fn embed<S: AsRef<str>>(&self, texts: &[S]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for batch in texts.chunks(BATCH) {
                let inputs: Vec<&str> = batch.iter().map(AsRef::as_ref).collect();
                let encodings = self
                    .tokenizer
                    .encode_batch(inputs, true)
                    .map_err(|e| anyhow::anyhow!("tokenizing: {e}"))?;
                let ids = encodings
                    .iter()
                    .map(|e| Tensor::new(e.get_ids(), &self.device))
                    .collect::<candle_core::Result<Vec<_>>>()?;
                let mask = encodings
                    .iter()
                    .map(|e| Tensor::new(e.get_attention_mask(), &self.device))
                    .collect::<candle_core::Result<Vec<_>>>()?;
                let ids = Tensor::stack(&ids, 0)?;
                let mask = Tensor::stack(&mask, 0)?;
                let hidden = self.model.forward(&ids, &ids.zeros_like()?, Some(&mask))?;
                for mut v in hidden.i((.., 0, ..))?.to_vec2::<f32>()? {
                    normalize(&mut v);
                    out.push(v);
                }
            }
            Ok(out)
        }
    }

    /// The model plus every stored session vector, loaded once per search session.
    pub struct Engine {
        embedder: Embedder,
        vectors: Vec<(String, Vec<f32>)>,
    }

    impl Engine {
        pub fn load(store: &Store, models: &Path) -> Result<Self> {
            let vectors: Vec<(String, Vec<f32>)> = store
                .embeddings(MODEL_KEY)?
                .into_iter()
                .map(|(sid, b)| (sid, from_bytes(&b)))
                .collect();
            if vectors.is_empty() {
                bail!("no sessions are embedded yet; run `claude-resume embed`");
            }
            Ok(Self {
                embedder: Embedder::load(models, false)?,
                vectors,
            })
        }

        /// `(sid, similarity)`, best first.
        pub fn rank(&self, query: &str) -> Result<Vec<(String, f32)>> {
            let q = self.embedder.embed_query(query)?;
            let mut scored: Vec<(String, f32)> = self
                .vectors
                .iter()
                .map(|(sid, v)| (sid.clone(), cosine(&q, v)))
                .filter(|(_, s)| *s >= MIN_SIMILARITY)
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            scored.truncate(MAX_RESULTS);
            Ok(scored)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_sensitive() {
        assert_eq!(text_hash(""), "cbf29ce484222325");
        assert_eq!(text_hash("a"), "af63dc4c8601ec8c");
        assert_ne!(text_hash("deploy"), text_hash("deploy "));
    }

    #[test]
    fn chunks_break_at_whitespace_and_cover_everything() {
        let s = "alpha beta gamma delta epsilon";
        let c = chunks(s, 12);
        assert!(c.iter().all(|p| p.chars().count() <= 12));
        assert_eq!(c.join(" "), s);
        assert_eq!(chunks("", 10), Vec::<&str>::new());
        let long_word = "x".repeat(30);
        assert_eq!(
            chunks(&long_word, 10).len(),
            3,
            "a word longer than a chunk is split"
        );
    }

    #[test]
    fn vector_math() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let m = mean_normalized(&[vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert!((cosine(&m, &m) - 1.0).abs() < 1e-6);
        assert!((m[0] - m[1]).abs() < 1e-6);
        assert!(mean_normalized(&[]).is_empty());
        assert_eq!(from_bytes(&to_bytes(&[1.5, -2.25, 0.0])), [1.5, -2.25, 0.0]);
    }

    #[test]
    fn session_text_caps_prompts() {
        let t = session_text("Title", &"p".repeat(PROMPT_CHARS * 2));
        assert_eq!(t.chars().count(), "Title\n".len() + PROMPT_CHARS);
    }
}
