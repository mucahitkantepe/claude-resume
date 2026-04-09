use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, HiddenAct, DTYPE};
use std::path::PathBuf;
use std::sync::OnceLock;

const MODEL_REPO: &str = "BAAI/bge-small-en-v1.5";
const EMBEDDING_DIM: usize = 384;
const MAX_TOKENS: usize = 512;
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
const CHUNK_SIZE: usize = 800;
const CHUNK_OVERLAP: usize = 100;
const MAX_CHUNKS: usize = 32;

fn models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("models")
}

pub struct Embedder {
    model: BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
}

impl Embedder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let (config_path, tokenizer_path, model_path) = download_model()?;

        let device = {
            #[cfg(feature = "metal")]
            { Device::new_metal(0).unwrap_or(Device::Cpu) }
            #[cfg(not(feature = "metal"))]
            { Device::Cpu }
        };

        let config_str = std::fs::read_to_string(&config_path)?;
        let mut config: Config = serde_json::from_str(&config_str)?;
        config.hidden_act = HiddenAct::GeluApproximate;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[model_path], DTYPE, &device)?
        };
        let model = BertModel::load(vb, &config)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("tokenizer: {}", e))?;
        let _ = tokenizer.with_truncation(Some(tokenizers::TruncationParams {
            max_length: MAX_TOKENS,
            stride: 0,
            strategy: tokenizers::TruncationStrategy::LongestFirst,
            direction: tokenizers::TruncationDirection::Right,
        }));

        Ok(Embedder { model, tokenizer, device })
    }

    /// Embed a query (with retrieval prefix for asymmetric search).
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let prefixed = format!("{}{}", QUERY_PREFIX, text);
        self.embed_text(&prefixed)
    }

    /// Embed text to a normalized vector.
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| format!("encode: {}", e))?;
        let token_ids = encoding.get_ids();
        let n_tokens = token_ids.len();
        if n_tokens == 0 {
            return Ok(vec![0.0f32; EMBEDDING_DIM]);
        }

        let ids: Vec<i64> = token_ids.iter().map(|&id| id as i64).collect();
        let token_ids_tensor = Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
        let token_type_ids = token_ids_tensor.zeros_like()?;

        let embeddings = self.model.forward(&token_ids_tensor, &token_type_ids, None)?;

        // Mean pooling across token dimension
        let sum = embeddings.sum(1)?;
        let mean = (sum / (n_tokens as f64))?;

        // L2 normalize
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean.broadcast_div(&norm)?;

        Ok(normalized.squeeze(0)?.to_vec1::<f32>()?)
    }

    /// Embed a session's full search text with chunking strategy.
    pub fn embed_session(&self, search_text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        if search_text.len() <= 1000 {
            return self.embed_text(search_text);
        }

        // Build chunks
        let mut all_chunks: Vec<&str> = Vec::new();
        let mut start = 0;
        while start < search_text.len() {
            let end = snap_right(search_text, (start + CHUNK_SIZE).min(search_text.len()));
            let s = snap_left(search_text, start);
            if s < end {
                all_chunks.push(&search_text[s..end]);
            }
            if end >= search_text.len() { break; }
            start = end.saturating_sub(CHUNK_OVERLAP);
        }

        // Sample if too many: first 8, last 8, 16 evenly from middle
        let selected: Vec<&str> = if all_chunks.len() <= MAX_CHUNKS {
            all_chunks
        } else {
            let mut sel = Vec::with_capacity(MAX_CHUNKS);
            let first = 8.min(all_chunks.len());
            let last = 8.min(all_chunks.len() - first);
            sel.extend_from_slice(&all_chunks[..first]);
            sel.extend_from_slice(&all_chunks[all_chunks.len() - last..]);
            let middle = &all_chunks[first..all_chunks.len() - last];
            let mid_count = MAX_CHUNKS - first - last;
            if !middle.is_empty() && mid_count > 0 {
                let step = middle.len() as f64 / mid_count as f64;
                for i in 0..mid_count {
                    let idx = (i as f64 * step) as usize;
                    if idx < middle.len() {
                        sel.push(middle[idx]);
                    }
                }
            }
            sel
        };

        // Embed each chunk
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        for chunk in &selected {
            if chunk.trim().is_empty() { continue; }
            match self.embed_text(chunk) {
                Ok(emb) => embeddings.push(emb),
                Err(_) => continue,
            }
        }

        if embeddings.is_empty() {
            return Ok(vec![0.0f32; EMBEDDING_DIM]);
        }

        // Mean pool across chunks
        let n = embeddings.len() as f32;
        let mut mean = vec![0.0f32; EMBEDDING_DIM];
        for emb in &embeddings {
            for (i, v) in emb.iter().enumerate() {
                mean[i] += v / n;
            }
        }

        // L2 normalize
        let norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut mean {
                *v /= norm;
            }
        }

        Ok(mean)
    }
}

/// Check if the model files are already cached locally.
pub fn is_model_downloaded() -> bool {
    let cache_dir = models_dir();
    let api = match hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .build()
    {
        Ok(a) => a,
        Err(_) => return false,
    };
    let repo = api.model(MODEL_REPO.to_string());
    // Try to resolve from cache only (no network)
    for file in &["config.json", "tokenizer.json", "model.safetensors"] {
        if repo.get(file).is_err() {
            // hf-hub checks cache first; if it fails with no network, model isn't cached
            return false;
        }
    }
    true
}

fn download_model() -> Result<(PathBuf, PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let cache_dir = models_dir();
    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .build()?;
    let repo = api.model(MODEL_REPO.to_string());

    let config_path = repo.get("config.json")?;
    let tokenizer_path = repo.get("tokenizer.json")?;
    let model_path = repo.get("model.safetensors")?;

    Ok((config_path, tokenizer_path, model_path))
}

fn snap_left(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

fn snap_right(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}

// ── Cosine similarity (both vectors must be L2-normalized) ──

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob
}

pub fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

// ── Global lazy singleton ──

static EMBEDDER: OnceLock<Option<Embedder>> = OnceLock::new();

pub fn get_or_init_embedder() -> Option<&'static Embedder> {
    EMBEDDER.get_or_init(|| {
        Embedder::new().ok()
    }).as_ref()
}

