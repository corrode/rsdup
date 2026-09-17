//! Local-only Ollama embeddings with an optional disk cache that stores no source.
//!
//! Call [`OllamaEmbedder::verify_model`] before publishing a report, including
//! reports served entirely from cache. No models are downloaded by this module.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::semantic::Embedder;

const ENDPOINT: &str = "http://localhost:11434";
const PROVIDER: &str = "ollama";
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
// Bump when extraction/embedding-input semantics or the cache format change.
const CACHE_POLICY_VERSION: u32 = 1;

/// Provenance of the embeddings; dimensions are unknown until the first vector.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaMetadata {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub digest: String,
    pub dimensions: Option<usize>,
}

/// Blocking embeddings from the fixed local Ollama endpoint.
pub struct OllamaEmbedder {
    agent: ureq::Agent,
    metadata: OllamaMetadata,
    cache_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Tags {
    models: Vec<Tag>,
}

#[derive(Debug, Deserialize)]
struct Tag {
    name: String,
    digest: String,
    capabilities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ShowResponse {
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    model: String,
    embeddings: Vec<Vec<f64>>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    policy_version: u32,
    key: String,
    metadata: OllamaMetadata,
    vector: Vec<f64>,
}

impl OllamaEmbedder {
    /// Resolve an installed model to its digest. The cache is opt-in.
    ///
    /// Both explicit names and names with an implicit `:latest` are accepted.
    /// Install/start Ollama and install the desired model separately first.
    pub fn new(model: &str, cache_dir: Option<PathBuf>) -> Result<Self> {
        ensure!(
            !model.trim().is_empty(),
            "Ollama model name must not be empty"
        );
        let agent = local_agent();
        let tag = resolve_model(fetch_tags(&agent)?, model)?;
        Self::from_tag(agent, tag, cache_dir)
    }

    /// Use the sole installed model advertising the `embedding` capability.
    ///
    /// Errors if none or multiple are installed; no model is guessed, downloaded,
    /// or asked to generate embeddings during discovery. The cache is opt-in.
    pub fn auto(cache_dir: Option<PathBuf>) -> Result<Self> {
        let agent = local_agent();
        let tag =
            discover_embedding_model(fetch_tags(&agent)?, |name| fetch_capabilities(&agent, name))?;
        Self::from_tag(agent, tag, cache_dir)
    }

    fn from_tag(agent: ureq::Agent, tag: Tag, cache_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(dir) = &cache_dir {
            fs::create_dir_all(dir)
                .with_context(|| format!("create Ollama cache directory {}", dir.display()))?;
        }
        Ok(Self {
            agent,
            metadata: OllamaMetadata {
                provider: PROVIDER.into(),
                endpoint: ENDPOINT.into(),
                model: tag.name,
                digest: tag.digest,
                dimensions: None,
            },
            cache_dir,
        })
    }

    /// Check that the installed tag still resolves to the pinned digest.
    ///
    /// Required before publishing a report. Ollama embeds by mutable tag rather
    /// than providing a transactional digest assertion; this detects changes at
    /// check time, but cannot rule out a tag being changed and changed back.
    pub fn verify_model(&self) -> Result<()> {
        let tag = resolve_model(fetch_tags(&self.agent)?, &self.metadata.model)?;
        ensure!(
            tag.name == self.metadata.model && tag.digest == self.metadata.digest,
            "Ollama model {:?} changed digest (pinned {}, now {}); restart the scan",
            self.metadata.model,
            self.metadata.digest,
            tag.digest
        );
        Ok(())
    }

    pub fn metadata(&self) -> OllamaMetadata {
        self.metadata.clone()
    }

    fn cache_key(&self, body: &str) -> String {
        cache_key(body, &self.metadata, CACHE_POLICY_VERSION)
    }

    fn read_cache(&mut self, path: &Path, key: &str) -> Result<Option<Vec<f64>>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read cache {}", path.display()));
            }
        };
        let result = (|| {
            let mut bytes = Vec::new();
            file.take(MAX_RESPONSE_BYTES + 1).read_to_end(&mut bytes)?;
            let entry: CacheEntry = parse_json(&bytes)?;
            ensure!(
                entry.policy_version == CACHE_POLICY_VERSION,
                "cache policy mismatch"
            );
            ensure!(entry.key == key, "cache input key mismatch");
            let mut expected = self.metadata.clone();
            expected.dimensions = Some(entry.vector.len());
            ensure!(
                entry.metadata == expected,
                "cache provenance or dimensions mismatch"
            );
            validate_vector(&entry.vector, self.metadata.dimensions)?;
            Ok(entry.vector)
        })();
        let vector: Vec<f64> = result.with_context(|| {
            format!(
                "invalid Ollama cache entry {}; remove this file and rerun to recompute",
                path.display()
            )
        })?;
        self.metadata.dimensions = Some(vector.len());
        Ok(Some(vector))
    }

    fn write_cache(&self, path: &Path, key: &str, vector: &[f64]) -> Result<()> {
        let entry = CacheEntry {
            policy_version: CACHE_POLICY_VERSION,
            key: key.into(),
            metadata: self.metadata(),
            vector: vector.to_vec(),
        };
        let bytes = serde_json::to_vec(&entry)?;
        ensure!(
            bytes.len() as u64 <= MAX_RESPONSE_BYTES,
            "Ollama cache entry exceeds 16 MiB"
        );
        let parent = path
            .parent()
            .context("cache entry has no parent directory")?;
        // Same-directory temporary files make publication atomic. NamedTempFile
        // also cleans up failed writes and creates private files on Unix.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

impl Embedder for OllamaEmbedder {
    fn embed(&mut self, body: &str) -> Result<Vec<f64>> {
        let key = self.cache_key(body);
        let path = self
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{key}.json")));
        if let Some(path) = &path
            && let Some(vector) = self.read_cache(path, &key)?
        {
            return Ok(vector);
        }

        self.verify_model()?;
        let response = self.agent.post(&format!("{ENDPOINT}/api/embed")).send_json(
            serde_json::json!({
                "model": self.metadata.model,
                "input": body,
                "truncate": false,
            }),
        ).context("local Ollama embedding request failed; ensure Ollama is running and the installed model supports embeddings (input truncation is disabled)")?;
        let response: EmbeddingResponse = read_response(response).context(
            "local Ollama /api/embed failed; if the input exceeds the model's token limit, lower --max-body-chars and rerun (input truncation is disabled)",
        )?;
        let vector = response.into_vector(&self.metadata.model, self.metadata.dimensions)?;
        self.verify_model()?;
        self.metadata.dimensions = Some(vector.len());
        if let Some(path) = path {
            self.write_cache(&path, &key, &vector)
                .with_context(|| format!("write Ollama cache entry {}", path.display()))?;
        }
        Ok(vector)
    }
}

fn local_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into()
}

fn fetch_tags(agent: &ureq::Agent) -> Result<Tags> {
    (|| {
        let response = agent.get(&format!("{ENDPOINT}/api/tags")).call()?;
        read_response(response)
    })()
    .context("cannot query local Ollama at http://localhost:11434/api/tags; install Ollama and start it (for example, `ollama serve`), then retry; rsdup never downloads models")
}

fn fetch_capabilities(agent: &ureq::Agent, model: &str) -> Result<Vec<String>> {
    (|| -> Result<Vec<String>> {
        let response = agent
            .post(&format!("{ENDPOINT}/api/show"))
            .send_json(serde_json::json!({ "model": model }))?;
        let response: ShowResponse = read_response(response)?;
        Ok(response.capabilities)
    })()
    .with_context(|| format!("cannot inspect local Ollama model {model:?} via /api/show"))
}

fn discover_embedding_model(
    mut tags: Tags,
    mut fetch: impl FnMut(&str) -> Result<Vec<String>>,
) -> Result<Tag> {
    tags.models.sort_by(|a, b| a.name.cmp(&b.name));
    let mut candidates = Vec::new();
    for tag in &tags.models {
        let capabilities = match &tag.capabilities {
            Some(capabilities) => capabilities.clone(),
            None => fetch(&tag.name)?,
        };
        if capabilities
            .iter()
            .any(|capability| capability == "embedding")
        {
            candidates.push(tag.name.clone());
        }
    }
    match candidates.as_slice() {
        [] => bail!(
            "no installed Ollama model supports embeddings; install an embedding model separately, then retry (rsdup never downloads models)"
        ),
        [name] => resolve_model(tags, name),
        _ => bail!(
            "multiple installed Ollama models support embeddings; choose one with --embedding-model <name>: {}",
            candidates.join(", ")
        ),
    }
}

fn read_response<T: DeserializeOwned>(mut response: ureq::http::Response<ureq::Body>) -> Result<T> {
    let status = response.status();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec();
    if !status.is_success() {
        #[derive(Deserialize)]
        struct ErrorResponse {
            error: Option<String>,
        }

        // Only expose Ollama's error field, never an arbitrary response body.
        // Unreadable, oversized, or malformed error bodies fall back to status.
        let message = bytes
            .as_ref()
            .ok()
            .and_then(|bytes| parse_json::<ErrorResponse>(bytes).ok())
            .and_then(|response| response.error)
            .filter(|message| !message.trim().is_empty());
        let redirect = if status.is_redirection() {
            "; redirects are disabled"
        } else {
            ""
        };
        if let Some(message) = message {
            bail!("Ollama returned HTTP {status}{redirect}: {message}");
        }
        bail!("Ollama returned HTTP {status}{redirect}");
    }
    parse_json(&bytes?)
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(
        bytes.len() as u64 <= MAX_RESPONSE_BYTES,
        "Ollama JSON exceeds 16 MiB"
    );
    serde_json::from_slice(bytes).context("malformed Ollama JSON")
}

fn resolve_model(tags: Tags, requested: &str) -> Result<Tag> {
    let latest = format!("{requested}:latest");
    let name = if tags.models.iter().any(|tag| tag.name == requested) {
        requested
    } else {
        &latest
    };
    let mut matching = tags.models.into_iter().filter(|tag| tag.name == name);
    let Some(mut tag) = matching.next() else {
        bail!(
            "Ollama model {requested:?} is not installed locally; install it separately, then retry (rsdup never downloads models)"
        );
    };
    ensure!(
        matching.next().is_none(),
        "ambiguous Ollama model tag {name:?}"
    );
    let digest = tag.digest.strip_prefix("sha256:").unwrap_or(&tag.digest);
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "Ollama model {name:?} returned an invalid SHA-256 digest"
    );
    tag.digest = digest.to_ascii_lowercase();
    Ok(tag)
}

impl EmbeddingResponse {
    fn into_vector(mut self, model: &str, dimensions: Option<usize>) -> Result<Vec<f64>> {
        ensure!(
            self.model == model,
            "Ollama returned model {:?}, expected {:?}",
            self.model,
            model
        );
        ensure!(
            self.embeddings.len() == 1,
            "Ollama must return exactly one embedding for one input"
        );
        let vector = self.embeddings.pop().context("missing Ollama embedding")?;
        validate_vector(&vector, dimensions)?;
        Ok(vector)
    }
}

fn validate_vector(vector: &[f64], dimensions: Option<usize>) -> Result<()> {
    ensure!(!vector.is_empty(), "Ollama embedding is empty");
    ensure!(
        vector.iter().all(|value| value.is_finite()),
        "Ollama embedding contains non-finite values"
    );
    ensure!(
        vector.iter().any(|value| *value != 0.0),
        "Ollama embedding is a zero vector"
    );
    if let Some(expected) = dimensions {
        ensure!(
            vector.len() == expected,
            "Ollama embedding dimension changed: expected {expected}, got {}",
            vector.len()
        );
    }
    Ok(())
}

fn cache_key(body: &str, metadata: &OllamaMetadata, policy: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [
        b"rsdup-ollama-embedding".as_slice(),
        &policy.to_le_bytes(),
        metadata.provider.as_bytes(),
        metadata.endpoint.as_bytes(),
        metadata.model.as_bytes(),
        metadata.digest.as_bytes(),
        body.as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedder(cache_dir: Option<PathBuf>) -> OllamaEmbedder {
        OllamaEmbedder {
            agent: local_agent(),
            metadata: OllamaMetadata {
                provider: PROVIDER.into(),
                endpoint: ENDPOINT.into(),
                model: "code:latest".into(),
                digest: "a".repeat(64),
                dimensions: None,
            },
            cache_dir,
        }
    }

    #[test]
    fn transport_is_local_only_and_bounded() {
        let agent = local_agent();
        assert!(agent.config().proxy().is_none());
        assert_eq!(agent.config().max_redirects(), 0);
        assert!(!agent.config().http_status_as_error());
        assert_eq!(
            agent.config().timeouts().global,
            Some(Duration::from_secs(120))
        );
    }

    fn response(status: u16, body: impl Into<Vec<u8>>) -> ureq::http::Response<ureq::Body> {
        ureq::http::Response::builder()
            .status(status)
            .body(ureq::Body::builder().data(body))
            .unwrap()
    }

    #[test]
    fn failed_response_exposes_only_ollama_json_error() {
        let error = read_response::<serde_json::Value>(response(
            400,
            r#"{"error":"input length exceeds maximum context length","input":"private source"}"#,
        ))
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Ollama returned HTTP 400 Bad Request: input length exceeds maximum context length"
        );
        assert!(!error.contains("private source"));
    }

    #[test]
    fn failed_response_without_json_error_falls_back_to_status() {
        for body in [
            "private non-JSON response",
            r#"{"error":"private", malformed}"#,
            r#"{"input":"private source"}"#,
            r#"{"error":null}"#,
            r#"{"error":42}"#,
            r#"{"error":"  "}"#,
            "",
        ] {
            let error = read_response::<serde_json::Value>(response(500, body))
                .unwrap_err()
                .to_string();
            assert_eq!(error, "Ollama returned HTTP 500 Internal Server Error");
        }
        let error = read_response::<serde_json::Value>(response(302, "redirect body"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Ollama returned HTTP 302 Found; redirects are disabled"
        );
    }

    #[test]
    fn response_body_limit_applies_to_success_and_failure() {
        let oversized = vec![b' '; MAX_RESPONSE_BYTES as usize + 1];
        assert!(read_response::<serde_json::Value>(response(200, oversized.clone())).is_err());
        let error = read_response::<serde_json::Value>(response(400, oversized))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Ollama returned HTTP 400 Bad Request");
        assert_eq!(
            read_response::<serde_json::Value>(response(200, r#"{"models":[]}"#)).unwrap(),
            serde_json::json!({"models": []})
        );
    }

    #[test]
    fn resolves_exact_then_latest_and_requires_valid_digest() {
        let tags = || Tags {
            models: vec![
                Tag {
                    name: "code".into(),
                    digest: "a".repeat(64),
                    capabilities: None,
                },
                Tag {
                    name: "code:latest".into(),
                    digest: "b".repeat(64),
                    capabilities: None,
                },
            ],
        };
        assert_eq!(
            resolve_model(tags(), "code").unwrap().digest,
            "a".repeat(64)
        );
        assert_eq!(
            resolve_model(tags(), "code:latest").unwrap().digest,
            "b".repeat(64)
        );
        let mut latest = tags();
        latest.models.remove(0);
        assert_eq!(resolve_model(latest, "code").unwrap().name, "code:latest");
        assert!(resolve_model(tags(), "missing").is_err());
        let mut invalid = tags();
        invalid.models[0].digest = "bad".into();
        assert!(resolve_model(invalid, "code").is_err());
    }

    #[test]
    fn parses_tags_and_embedding_responses() {
        let tags: Tags = parse_json(
            serde_json::to_string(&serde_json::json!({
                "models": [{"name": "code:latest", "digest": "a".repeat(64)}]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(resolve_model(tags, "code").unwrap().name, "code:latest");
        let response: EmbeddingResponse =
            parse_json(br#"{"model":"code:latest","embeddings":[[0.5,-0.5]],"total_duration":1}"#)
                .unwrap();
        assert_eq!(
            response.into_vector("code:latest", Some(2)).unwrap(),
            vec![0.5, -0.5]
        );
        for json in [
            r#"{"model":"other","embeddings":[[1]]}"#,
            r#"{"model":"code:latest","embeddings":[]}"#,
            r#"{"model":"code:latest","embeddings":[[1],[2]]}"#,
            r#"{"model":"code:latest","embeddings":[[]]}"#,
            r#"{"model":"code:latest","embeddings":[[0,0]]}"#,
        ] {
            let response: EmbeddingResponse = parse_json(json.as_bytes()).unwrap();
            assert!(response.into_vector("code:latest", None).is_err());
        }
        assert!(parse_json::<EmbeddingResponse>(br#"{"error":"failed"}"#).is_err());
        assert!(
            parse_json::<EmbeddingResponse>(br#"{"model":"code:latest","embeddings":[[1e999]]}"#)
                .is_err()
        );
    }

    fn model(name: &str, capabilities: &[&str]) -> Tag {
        Tag {
            name: name.into(),
            digest: format!("sha256:{}", "A".repeat(64)),
            capabilities: Some(capabilities.iter().map(|s| (*s).into()).collect()),
        }
    }

    fn discover(models: Vec<Tag>) -> Result<Tag> {
        discover_embedding_model(Tags { models }, |_| panic!("unexpected /api/show"))
    }

    #[test]
    fn discovery_requires_an_installed_embedding_model() {
        for models in [vec![], vec![model("chat:latest", &["completion"])]] {
            let error = discover(models).unwrap_err().to_string();
            assert!(error.contains("install an embedding model"), "{error}");
            assert!(error.contains("rsdup never downloads models"), "{error}");
        }
    }

    #[test]
    fn discovery_selects_only_embedding_capabilities_and_validates_digest() {
        for reverse in [false, true] {
            let mut models = vec![
                model("chat:latest", &["completion"]),
                model("embed:latest", &["embedding", "completion"]),
                model("embedding-in-name:latest", &[]),
                model("plural:latest", &["embeddings"]),
            ];
            if reverse {
                models.reverse();
            }
            let tag = discover(models).unwrap();
            assert_eq!(tag.name, "embed:latest");
            assert_eq!(tag.digest, "a".repeat(64));
        }
        let mut invalid = model("embed:latest", &["embedding"]);
        invalid.digest = "bad".into();
        assert!(
            discover(vec![invalid])
                .unwrap_err()
                .to_string()
                .contains("invalid SHA-256")
        );
    }

    #[test]
    fn discovery_lists_multiple_candidates_in_sorted_order() {
        let models = || {
            vec![
                model("z:latest", &["embedding"]),
                model("chat:latest", &["completion"]),
                model("a:latest", &["embedding"]),
            ]
        };
        let error = discover(models()).unwrap_err().to_string();
        let mut reversed = models();
        reversed.reverse();
        assert_eq!(error, discover(reversed).unwrap_err().to_string());
        assert!(
            error.contains("--embedding-model <name>: a:latest, z:latest"),
            "{error}"
        );
        assert!(!error.contains("chat:latest"));
    }

    #[test]
    fn discovery_reads_tags_capabilities_or_falls_back_to_show() {
        let tags =
            || {
                parse_json::<Tags>(
            serde_json::to_vec(&serde_json::json!({
                "models": [
                    {"name": "chat", "digest": "a".repeat(64), "capabilities": ["completion"]},
                    {"name": "empty", "digest": "a".repeat(64), "capabilities": []},
                    {"name": "embed", "digest": "b".repeat(64)}
                ]
            })).unwrap().as_slice()
        ).unwrap()
            };
        assert!(tags().models[2].capabilities.is_none());
        let mut calls = Vec::new();
        let tag = discover_embedding_model(tags(), |name| {
            calls.push(name.to_owned());
            let show: ShowResponse = read_response(response(
                200,
                r#"{"capabilities":["embedding"],"details":{}}"#,
            ))?;
            Ok(show.capabilities)
        })
        .unwrap();
        assert_eq!(calls, vec!["embed"]);
        assert_eq!(tag.name, "embed");
        assert_eq!(tag.digest, "b".repeat(64));
        let error = discover_embedding_model(tags(), |_| bail!("show failed")).unwrap_err();
        assert_eq!(error.to_string(), "show failed");
        assert!(read_response::<ShowResponse>(response(200, "{}")).is_err());
    }

    #[test]
    fn rejects_invalid_vectors_and_dimension_changes() {
        for vector in [
            vec![],
            vec![0.0, -0.0],
            vec![f64::NAN],
            vec![f64::INFINITY],
            vec![f64::NEG_INFINITY],
        ] {
            assert!(validate_vector(&vector, None).is_err());
        }
        assert!(validate_vector(&[1.0], Some(2)).is_err());
        assert!(validate_vector(&[f64::MAX, f64::MIN_POSITIVE], Some(2)).is_ok());
    }

    #[test]
    fn cache_keys_invalidate_on_input_digest_and_policy() {
        let provider = embedder(None);
        let key = provider.cache_key("body");
        assert_ne!(key, provider.cache_key("other body"));
        assert_ne!(
            key,
            cache_key("body", &provider.metadata, CACHE_POLICY_VERSION + 1)
        );
        let mut changed = provider.metadata();
        changed.digest = "b".repeat(64);
        assert_ne!(key, cache_key("body", &changed, CACHE_POLICY_VERSION));
        changed = provider.metadata();
        changed.dimensions = Some(2);
        assert_eq!(key, cache_key("body", &changed, CACHE_POLICY_VERSION));
    }

    #[test]
    fn cache_roundtrip_without_network_or_source_storage() {
        let dir = tempfile::tempdir().unwrap();
        let mut provider = embedder(Some(dir.path().into()));
        let body = "private source must never be stored";
        let key = provider.cache_key(body);
        let path = dir.path().join(format!("{key}.json"));
        assert!(provider.read_cache(&path, &key).unwrap().is_none());
        provider.metadata.dimensions = Some(2);
        provider.write_cache(&path, &key, &[0.5, -0.5]).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains(body));
        provider.metadata.dimensions = None;
        assert_eq!(provider.embed(body).unwrap(), vec![0.5, -0.5]);
        assert_eq!(provider.metadata().dimensions, Some(2));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        provider.metadata.dimensions = Some(3);
        assert!(provider.read_cache(&path, &key).is_err());
    }

    #[test]
    fn corrupt_or_mismatched_cache_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let mut provider = embedder(None);
        provider.metadata.dimensions = Some(2);
        let key = provider.cache_key("body");
        let path = dir.path().join("entry.json");
        provider.write_cache(&path, &key, &[1.0, 2.0]).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (pointer, value) in [
            ("/policy_version", serde_json::json!(999)),
            ("/key", serde_json::json!("wrong")),
            ("/metadata/provider", serde_json::json!("other")),
            ("/metadata/endpoint", serde_json::json!("http://remote")),
            ("/metadata/model", serde_json::json!("other")),
            ("/metadata/digest", serde_json::json!("b".repeat(64))),
            ("/metadata/dimensions", serde_json::json!(3)),
            ("/vector", serde_json::json!([0, 0])),
        ] {
            let mut altered = original.clone();
            *altered.pointer_mut(pointer).unwrap() = value;
            fs::write(&path, serde_json::to_vec(&altered).unwrap()).unwrap();
            let error = provider.read_cache(&path, &key).unwrap_err().to_string();
            assert!(error.contains("remove this file"), "{error}");
        }
        fs::write(&path, b"not JSON").unwrap();
        assert!(provider.read_cache(&path, &key).is_err());
    }

    #[test]
    fn rejects_oversized_json() {
        assert!(parse_json::<Tags>(&vec![b' '; MAX_RESPONSE_BYTES as usize + 1]).is_err());
    }
}
