//! In-process CPU embeddings from a pinned Jina v2 base code model.
//!
//! Construct only after the user selects this backend: construction downloads
//! missing public model artifacts (~642 MB) from Hugging Face; source code is never uploaded.
//! Model files live in `dirs::cache_dir()/rsdup/fastembed`, or the directory set
//! by `FASTEMBED_CACHE_DIR`. The optional constructor path is a separate, opt-in
//! embedding-result cache, containing hashes and vectors rather than source.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::semantic::Embedder;

const MODEL: &str = "jinaai/jina-embeddings-v2-base-code";
const REVISION: &str = "516f4baf13dec4ddddda8631e019b5737c8bc250";
const MAX_TOKENS: usize = 8192;
const DIMENSIONS: usize = 768;
const CACHE_POLICY: u32 = 1;
const MAX_CACHE_BYTES: u64 = 128 * 1024;
// SHA-256 identities of the official artifacts at REVISION.
// Verify the actual bytes passed to FastEmbed.
const ARTIFACTS: &[(&str, &str)] = &[
    (
        "onnx/model.onnx",
        "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
    ),
    (
        "config.json",
        "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6",
    ),
    (
        "special_tokens_map.json",
        "06e405a36dfe4b9604f484f6a1e619af1a7f7d09e34a8555eb0b77b66318067f",
    ),
    (
        "tokenizer.json",
        "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
    ),
    (
        "tokenizer_config.json",
        "f477aeb15ff9f78d3c1ddf2361d2b0b8b20cf55220f839f29a37f3a18efddd89",
    ),
];

/// Model and inference provenance, including the verified artifact identities.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalMetadata {
    pub provider: String,
    pub model: String,
    pub revision: String,
    pub dimensions: usize,
    pub artifacts_sha256: BTreeMap<String, String>,
    pub inference: LocalInferenceConfig,
}

/// Settings that affect tokenization and numerical results (also cache identity).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalInferenceConfig {
    pub fastembed_version: String,
    pub ort_version: String,
    pub tokenizers_version: String,
    pub execution_provider: String,
    pub intra_threads: usize,
    pub max_tokens: usize,
    pub special_tokens: bool,
    pub truncation: bool,
    pub pooling: String,
    pub normalization: String,
    pub precision: String,
    pub batch_size: usize,
    pub target: String,
}

impl LocalMetadata {
    fn new(intra_threads: usize) -> Self {
        Self {
            provider: "fastembed".into(),
            model: MODEL.into(),
            revision: REVISION.into(),
            dimensions: DIMENSIONS,
            artifacts_sha256: ARTIFACTS
                .iter()
                .map(|(name, hash)| (name.to_string(), hash.to_string()))
                .collect(),
            inference: LocalInferenceConfig {
                fastembed_version: "7.0.1".into(),
                ort_version: "2.0.0-rc.13".into(),
                tokenizers_version: "0.23.2".into(),
                execution_provider: "cpu".into(),
                intra_threads,
                max_tokens: MAX_TOKENS,
                special_tokens: true,
                truncation: false,
                pooling: "attention-mask mean".into(),
                normalization: "FastEmbed L2; semantic core renormalizes in f64".into(),
                precision: "f32 inference, f64 output".into(),
                batch_size: 1,
                target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            },
        }
    }
}

/// Blocking, sequential CPU inference. Construction may download model files.
pub struct LocalEmbedder {
    model: TextEmbedding,
    metadata: LocalMetadata,
    cache_dir: Option<PathBuf>,
}

impl LocalEmbedder {
    /// Load the pinned model, downloading missing artifacts into the user cache.
    ///
    /// `cache` enables result caching under `<cache>/fastembed-v1`; it does not
    /// control model storage. Set `FASTEMBED_CACHE_DIR` to change model storage.
    /// `show_progress` controls model download progress only.
    pub fn new(cache: Option<PathBuf>, show_progress: bool) -> Result<Self> {
        let model_cache =
            model_cache_dir(std::env::var_os("FASTEMBED_CACHE_DIR"), dirs::cache_dir())?;
        let repo = ApiBuilder::new()
            .with_cache_dir(model_cache.clone())
            .with_token(None)
            .with_progress(show_progress)
            .build()
            .context("initialize public Hugging Face model downloader")?
            .repo(Repo::with_revision(
                MODEL.into(),
                RepoType::Model,
                REVISION.into(),
            ));
        let mut artifacts = BTreeMap::new();
        for &(name, expected) in ARTIFACTS {
            // hf-hub 0.5 resolves even commit hashes through refs/. A snapshot
            // downloaded via `main` can already exist without a commit-named ref.
            let snapshot = snapshot_path(&model_cache, name);
            let path = if snapshot.try_exists()? {
                snapshot
            } else {
                repo.get(name).with_context(|| format!(
                    "download FastEmbed artifact {MODEL}@{REVISION}/{name} into {}; check network access or set FASTEMBED_CACHE_DIR to a populated model cache",
                    model_cache.display()
                ))?
            };
            let bytes = fs::read(&path)
                .with_context(|| format!("read model artifact {}", path.display()))?;
            verify_artifact(&bytes, expected).with_context(|| format!(
                "invalid FastEmbed model artifact {}; remove this file and rerun to download it again", path.display()
            ))?;
            artifacts.insert(name, bytes);
        }
        let mut take = |name| {
            artifacts
                .remove(name)
                .context("missing verified model artifact")
        };
        let model = UserDefinedEmbeddingModel::new(
            take("onnx/model.onnx")?,
            TokenizerFiles {
                config_file: take("config.json")?,
                special_tokens_map_file: take("special_tokens_map.json")?,
                tokenizer_file: take("tokenizer.json")?,
                tokenizer_config_file: take("tokenizer_config.json")?,
            },
        )
        .with_pooling(Pooling::Mean);
        let threads = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let mut model = TextEmbedding::try_new_from_user_defined(
            model,
            InitOptionsUserDefined::new()
                .with_max_length(MAX_TOKENS)
                .with_intra_threads(threads)
                .with_execution_providers(vec![
                    ort::ep::CPU::default().with_arena_allocator(true).build(),
                ]),
        )
        .context("initialize FastEmbed Jina v2 base code CPU model")?;
        configure_tokenizer(&mut model.tokenizer)?;
        let cache_dir = cache.map(|dir| dir.join("fastembed-v1"));
        if let Some(dir) = &cache_dir {
            fs::create_dir_all(dir)
                .with_context(|| format!("create FastEmbed result cache {}", dir.display()))?;
        }
        Ok(Self {
            model,
            metadata: LocalMetadata::new(threads),
            cache_dir,
        })
    }

    /// Return provenance of the loaded, hash-verified model and inference policy.
    pub fn metadata(&self) -> LocalMetadata {
        self.metadata.clone()
    }
}

impl Embedder for LocalEmbedder {
    fn embed(&mut self, body: &str) -> Result<Vec<f64>> {
        // Preflight even cache hits: no cached result can bypass the input policy.
        preflight(&self.model.tokenizer, body)?;
        let key = cache_key(body, &self.metadata)?;
        let path = self
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{key}.json")));
        if let Some(path) = &path
            && let Some(vector) = read_cache(path, &key, &self.metadata)?
        {
            return Ok(vector);
        }
        let embeddings = self
            .model
            .embed(vec![body], Some(1))
            .context("FastEmbed CPU inference failed (input truncation is disabled)")?;
        let vector = into_vector(embeddings)?;
        if let Some(path) = path {
            write_cache(&path, &key, &self.metadata, &vector)
                .with_context(|| format!("write FastEmbed result cache {}", path.display()))?;
        }
        Ok(vector)
    }
}

fn model_cache_dir(
    override_dir: Option<std::ffi::OsString>,
    user_cache: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        ensure!(!dir.is_empty(), "FASTEMBED_CACHE_DIR must not be empty");
        return Ok(PathBuf::from(dir));
    }
    Ok(user_cache
        .context(
            "cannot find the user cache directory; set FASTEMBED_CACHE_DIR for model downloads",
        )?
        .join("rsdup")
        .join("fastembed"))
}

fn snapshot_path(cache: &Path, name: &str) -> PathBuf {
    cache
        .join(Repo::new(MODEL.into(), RepoType::Model).folder_name())
        .join("snapshots")
        .join(REVISION)
        .join(name)
}

fn verify_artifact(bytes: &[u8], expected: &str) -> Result<()> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == expected,
        "model artifact SHA-256 mismatch"
    );
    Ok(())
}

fn configure_tokenizer(tokenizer: &mut Tokenizer) -> Result<()> {
    tokenizer
        .with_truncation(None)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // Sequential inference requires no padding, and preflight must count actual tokens.
    tokenizer.with_padding(None);
    Ok(())
}

fn preflight(tokenizer: &Tokenizer, body: &str) -> Result<()> {
    let encoded = tokenizer
        .encode(body, true)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
        .context("tokenize FastEmbed input")?;
    ensure!(
        encoded.len() <= MAX_TOKENS,
        "FastEmbed input has {} tokens including special tokens, exceeding {MAX_TOKENS}; lower --max-body-chars and rerun (input truncation is disabled)",
        encoded.len()
    );
    Ok(())
}

fn into_vector(mut embeddings: Vec<Vec<f32>>) -> Result<Vec<f64>> {
    ensure!(
        embeddings.len() == 1,
        "FastEmbed returned {} vectors, expected one",
        embeddings.len()
    );
    let vector: Vec<f64> = embeddings.remove(0).into_iter().map(f64::from).collect();
    validate_vector(&vector)?;
    Ok(vector)
}

fn validate_vector(vector: &[f64]) -> Result<()> {
    ensure!(
        vector.len() == DIMENSIONS,
        "FastEmbed vector has {} dimensions, expected {DIMENSIONS}",
        vector.len()
    );
    ensure!(
        vector.iter().all(|value| value.is_finite()),
        "FastEmbed vector contains non-finite values"
    );
    ensure!(
        vector.iter().any(|value| *value != 0.0),
        "FastEmbed vector has zero norm"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    policy: u32,
    key: String,
    metadata: LocalMetadata,
    vector: Vec<f64>,
}

fn cache_key(body: &str, metadata: &LocalMetadata) -> Result<String> {
    // Structured serialization prevents ambiguous concatenation; source is hashed,
    // never persisted. Include all inference settings and verified artifact hashes.
    let bytes = serde_json::to_vec(&(CACHE_POLICY, metadata, body))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn read_cache(path: &Path, key: &str, metadata: &LocalMetadata) -> Result<Option<Vec<f64>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read FastEmbed cache {}", path.display()));
        }
    };
    (|| {
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CACHE_BYTES,
            "cache entry exceeds 128 KiB"
        );
        let entry: CacheEntry = serde_json::from_slice(&bytes)?;
        ensure!(
            entry.policy == CACHE_POLICY && entry.key == key && &entry.metadata == metadata,
            "cache policy, input key, or provenance mismatch"
        );
        validate_vector(&entry.vector)?;
        Ok(Some(entry.vector))
    })()
    .with_context(|| {
        format!(
            "invalid FastEmbed cache entry {}; remove this file and rerun to recompute",
            path.display()
        )
    })
}

fn write_cache(path: &Path, key: &str, metadata: &LocalMetadata, vector: &[f64]) -> Result<()> {
    validate_vector(vector)?;
    let bytes = serde_json::to_vec(&CacheEntry {
        policy: CACHE_POLICY,
        key: key.into(),
        metadata: metadata.clone(),
        vector: vector.to_vec(),
    })?;
    ensure!(
        bytes.len() as u64 <= MAX_CACHE_BYTES,
        "cache entry exceeds 128 KiB"
    );
    // Match Ollama's same-directory, private temporary file + atomic publication.
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("cache path has no parent")?)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokenizers::{
        models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace,
        processors::template::TemplateProcessing,
    };

    fn tokenizer() -> Tokenizer {
        let model = WordLevel::builder()
            .vocab(
                [
                    ("[UNK]".into(), 0),
                    ("a".into(), 1),
                    ("[CLS]".into(), 2),
                    ("[SEP]".into(), 3),
                ]
                .into_iter()
                .collect(),
            )
            .unk_token("[UNK]".into())
            .build()
            .unwrap();
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));
        tokenizer.with_post_processor(Some(
            TemplateProcessing::builder()
                .try_single("[CLS] $A [SEP]")
                .unwrap()
                .special_tokens(vec![("[CLS]", 2), ("[SEP]", 3)])
                .build()
                .unwrap(),
        ));
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 4,
                ..Default::default()
            }))
            .unwrap();
        configure_tokenizer(&mut tokenizer).unwrap();
        tokenizer
    }

    /// Explicit smoke test: requires all official artifacts already cached, so
    /// even an opted-in run cannot accidentally download the model.
    #[test]
    #[ignore = "requires FASTEMBED_CACHE_DIR containing the pinned official model"]
    fn cached_official_model_smoke() -> Result<()> {
        let dir = std::env::var_os("FASTEMBED_CACHE_DIR")
            .context("set FASTEMBED_CACHE_DIR to an existing official model cache")?;
        let dir = PathBuf::from(dir);
        for &(name, _) in ARTIFACTS {
            ensure!(
                snapshot_path(&dir, name).is_file(),
                "smoke test requires cached artifact {name}"
            );
        }
        let cache = tempfile::tempdir()?;
        let mut embedder = LocalEmbedder::new(Some(cache.path().into()), false)?;
        let body = "fn sum(values: &[u32]) -> u32 { values.iter().copied().sum() }";
        let vector = embedder.embed(body)?;
        assert_eq!(vector.len(), DIMENSIONS);
        assert_eq!(embedder.embed(body)?, vector);
        assert_ne!(
            embedder.embed("fn greet() { println!(\"hello\"); }")?,
            vector
        );
        assert!(embedder.embed(&"fn ".repeat(MAX_TOKENS)).is_err());
        assert_eq!(embedder.metadata().revision, REVISION);
        assert_eq!(fs::read_dir(cache.path().join("fastembed-v1"))?.count(), 2);
        Ok(())
    }

    #[test]
    fn preflight_counts_special_tokens_and_never_truncates() {
        let tokenizer = tokenizer();
        assert!(tokenizer.get_truncation().is_none());
        assert!(tokenizer.get_padding().is_none());
        assert_eq!(tokenizer.encode("a a a a a", true).unwrap().len(), 7);
        assert!(preflight(&tokenizer, &"a ".repeat(MAX_TOKENS - 2)).is_ok());
        let error = preflight(&tokenizer, &"a ".repeat(MAX_TOKENS - 1)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("8193 tokens including special tokens")
        );
        assert!(preflight(&tokenizer, "fn café() { /* 🦀 */ }").is_ok());
    }

    #[test]
    fn payload_shape_and_values_are_validated() {
        assert!(into_vector(vec![]).is_err());
        assert!(into_vector(vec![vec![1.; DIMENSIONS]; 2]).is_err());
        assert!(into_vector(vec![vec![1.; DIMENSIONS - 1]]).is_err());
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.] {
            assert!(into_vector(vec![vec![value; DIMENSIONS]]).is_err());
        }
        assert_eq!(
            into_vector(vec![vec![0.1; DIMENSIONS]]).unwrap()[0],
            f64::from(0.1_f32)
        );
    }

    #[test]
    fn model_cache_is_user_scoped_and_override_is_explicit() {
        assert_eq!(
            model_cache_dir(None, Some(PathBuf::from("user-cache"))).unwrap(),
            PathBuf::from("user-cache/rsdup/fastembed")
        );
        assert_eq!(
            model_cache_dir(Some("override".into()), None).unwrap(),
            PathBuf::from("override")
        );
        assert!(model_cache_dir(None, None).is_err());
        assert!(model_cache_dir(Some("".into()), None).is_err());
    }

    #[test]
    fn artifact_hash_verification_rejects_modified_bytes() {
        let hash = format!("{:x}", Sha256::digest(b"official bytes"));
        assert!(verify_artifact(b"official bytes", &hash).is_ok());
        assert!(verify_artifact(b"changed bytes", &hash).is_err());
    }

    #[test]
    fn provenance_and_input_change_cache_identity() {
        let metadata = LocalMetadata::new(10);
        let key = cache_key("source", &metadata).unwrap();
        assert_ne!(key, cache_key("other", &metadata).unwrap());
        for field in ["provider", "revision", "dimensions", "model"] {
            let mut value = serde_json::to_value(&metadata).unwrap();
            value[field] = if field == "dimensions" {
                12.into()
            } else {
                "changed".into()
            };
            let changed: LocalMetadata = serde_json::from_value(value).unwrap();
            assert_ne!(key, cache_key("source", &changed).unwrap());
        }
        let mut changed = metadata.clone();
        changed.inference.truncation = true;
        assert_ne!(key, cache_key("source", &changed).unwrap());
        changed = metadata.clone();
        changed
            .artifacts_sha256
            .insert("onnx/model.onnx".into(), "wrong".into());
        assert_ne!(key, cache_key("source", &changed).unwrap());
        assert_eq!(metadata.artifacts_sha256.len(), 5);
        assert_eq!(
            TextEmbedding::get_default_pooling_method(
                &fastembed::EmbeddingModel::JinaEmbeddingsV2BaseCode
            ),
            Some(Pooling::Mean)
        );
    }

    #[test]
    fn result_cache_roundtrips_without_source_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.json");
        let metadata = LocalMetadata::new(1);
        let source = "private function body not to be stored";
        let key = cache_key(source, &metadata).unwrap();
        // This f32 inference value promoted to f64 drifts with serde_json's
        // default float parser; the float_roundtrip feature preserves it exactly.
        let vector = vec![f64::from(-0.018997425_f32); DIMENSIONS];
        assert!(read_cache(&path, &key, &metadata).unwrap().is_none());
        write_cache(&path, &key, &metadata, &vector).unwrap();
        assert_eq!(
            read_cache(&path, &key, &metadata).unwrap(),
            Some(vector.clone())
        );
        assert!(!fs::read_to_string(&path).unwrap().contains(source));
        assert!(read_cache(&path, "wrong", &metadata).is_err());
        assert!(read_cache(&path, &key, &LocalMetadata::new(2)).is_err());
        let original = fs::read(&path).unwrap();
        for (field, value) in [
            ("policy", serde_json::json!(999)),
            ("vector", serde_json::json!([1.0])),
        ] {
            let mut entry: serde_json::Value = serde_json::from_slice(&original).unwrap();
            entry[field] = value;
            fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
            assert!(read_cache(&path, &key, &metadata).is_err());
        }
        for bad in [
            b"{broken".to_vec(),
            vec![b' '; MAX_CACHE_BYTES as usize + 1],
        ] {
            fs::write(&path, bad).unwrap();
            assert!(
                read_cache(&path, &key, &metadata)
                    .unwrap_err()
                    .to_string()
                    .contains("remove this file")
            );
        }
        // Replacement is atomic and leaves no temporary files behind.
        write_cache(&path, &key, &metadata, &vector).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
