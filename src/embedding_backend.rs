//! CLI-only backend selection. Choices apply to this run and are never persisted.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rsdup::{
    local_embedding::{LocalEmbedder, LocalMetadata},
    ollama::{OllamaEmbedder, OllamaMetadata},
    semantic::Embedder,
};
use serde::Serialize;

const CHOICES: &str = "Choose how to compare code:\n  1) FastEmbed: CPU, no server; public pinned model download (~642 MB) if not cached.\n  2) Ollama: local server and installed embedding model; no automatic setup or downloads.";
const CHOICE_COMMANDS: &str = "Use `rsdup --embedding-backend fastembed` or `rsdup --embedding-backend ollama --embedding-model unclemusclez/jina-embeddings-v2-base-code`. For another installed Ollama model, use --embedding-model <name>. For syntax-only comparison: rsdup --mode near-miss";
const OLLAMA_SETUP: &str = "To use Ollama, install it from https://ollama.com/start, start the app or run `ollama serve`, then run `ollama pull unclemusclez/jina-embeddings-v2-base-code`. Retry with `rsdup --embedding-backend ollama --embedding-model unclemusclez/jina-embeddings-v2-base-code`. If embedding models are already installed, choose one with --embedding-model <name>.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Backend {
    /// Use an available Ollama model; otherwise ask before choosing a backend.
    #[default]
    Auto,
    /// In-process CPU embeddings; permits a ~642 MB public model download.
    Fastembed,
    /// Use local Ollama only; never install, start, or pull models automatically.
    Ollama,
}

impl Backend {
    pub fn with_model(self, model: Option<&str>) -> Result<Self> {
        match (self, model) {
            (Self::Fastembed, Some(_)) => bail!(
                "--embedding-model selects an Ollama model and cannot be used with --embedding-backend fastembed; remove --embedding-model to use the pinned FastEmbed model, or use --embedding-backend ollama"
            ),
            (_, Some(_)) => Ok(Self::Ollama),
            _ => Ok(self),
        }
    }
}

pub enum BackendEmbedder {
    Fastembed(Box<LocalEmbedder>),
    Ollama(OllamaEmbedder),
}

// Untagged preserves the existing Ollama report fields without a wrapper/tag.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Metadata {
    Fastembed(Box<LocalMetadata>),
    Ollama(OllamaMetadata),
}

impl BackendEmbedder {
    pub fn new(
        backend: Backend,
        model: Option<&str>,
        cache: Option<PathBuf>,
        quiet: bool,
    ) -> Result<Self> {
        let backend = backend.with_model(model)?;
        let selected = select(
            backend,
            can_prompt(quiet, io::stdin().is_terminal(), io::stderr().is_terminal()),
            || match model {
                Some(model) => OllamaEmbedder::new(model, cache.clone()),
                None => OllamaEmbedder::auto_with_discovery_timeout(
                    cache.clone(),
                    Duration::from_secs(3),
                ),
            },
            |reason| prompt(io::stdin().lock(), io::stderr().lock(), reason),
        )?;
        let embedder = match selected {
            Selected::Fastembed => {
                if !quiet {
                    eprintln!(
                        "FastEmbed will load a public pinned model from Hugging Face (~642 MB download if not cached). Code stays on this machine; inference runs on CPU."
                    );
                }
                Self::Fastembed(Box::new(LocalEmbedder::new(cache, !quiet)?))
            }
            Selected::Ollama(embedder) => Self::Ollama(embedder),
        };
        if !quiet {
            match embedder.metadata() {
                Metadata::Fastembed(meta) => eprintln!("Using FastEmbed model: {}", meta.model),
                Metadata::Ollama(meta) => eprintln!("Using Ollama model: {}", meta.model),
            }
        }
        Ok(embedder)
    }

    pub fn metadata(&self) -> Metadata {
        match self {
            Self::Fastembed(embedder) => Metadata::Fastembed(Box::new(embedder.metadata())),
            Self::Ollama(embedder) => Metadata::Ollama(embedder.metadata()),
        }
    }

    pub fn verify_model(&self) -> Result<()> {
        match self {
            Self::Fastembed(_) => Ok(()),
            Self::Ollama(embedder) => embedder.verify_model(),
        }
    }
}

impl Embedder for BackendEmbedder {
    fn embed(&mut self, body: &str) -> Result<Vec<f64>> {
        match self {
            Self::Fastembed(embedder) => embedder.embed(body),
            Self::Ollama(embedder) => embedder.embed(body),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Selected<T> {
    Fastembed,
    Ollama(T),
}

fn can_prompt(quiet: bool, stdin_terminal: bool, stderr_terminal: bool) -> bool {
    !quiet && stdin_terminal && stderr_terminal
}

// Inject both discovery and user input so policy tests never access the network
// or initialize a model (which could download hundreds of megabytes).
fn select<T>(
    backend: Backend,
    interactive: bool,
    probe: impl FnOnce() -> Result<T>,
    choose: impl FnOnce(&str) -> Result<Backend>,
) -> Result<Selected<T>> {
    if backend == Backend::Fastembed {
        return Ok(Selected::Fastembed);
    }
    let error = match probe() {
        Ok(embedder) => return Ok(Selected::Ollama(embedder)),
        Err(error) => error,
    };
    if backend == Backend::Ollama {
        return Err(error).context(OLLAMA_SETUP);
    }
    if !interactive {
        return Err(error).context(format!(
            "No Ollama model could be selected automatically. No prompt is available with --quiet or non-terminal stdin/stderr.\n{CHOICES}\n{CHOICE_COMMANDS}"
        ));
    }
    match choose(&format!("{error:#}"))? {
        Backend::Fastembed => Ok(Selected::Fastembed),
        Backend::Ollama => Err(error).context(OLLAMA_SETUP),
        Backend::Auto => bail!("choose fastembed or ollama explicitly; no backend was selected"),
    }
}

fn prompt(mut input: impl BufRead, mut output: impl Write, reason: &str) -> Result<Backend> {
    writeln!(
        output,
        "No Ollama model could be selected automatically: {reason}\n\n{CHOICES}"
    )?;
    write!(
        output,
        "Choose 1 or 2 (fastembed/ollama; this run only, no default): "
    )?;
    output.flush()?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .context("read backend choice")?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" | "fastembed" => Ok(Backend::Fastembed),
        "2" | "ollama" => Ok(Backend::Ollama),
        _ => bail!(
            "no backend selected; rerun with --embedding-backend fastembed or --embedding-backend ollama"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_model_implies_ollama_except_with_fastembed() {
        for backend in [Backend::Auto, Backend::Ollama] {
            assert_eq!(backend.with_model(Some("chosen")).unwrap(), Backend::Ollama);
        }
        assert!(
            Backend::Fastembed
                .with_model(Some("chosen"))
                .unwrap_err()
                .to_string()
                .contains("remove --embedding-model")
        );
    }

    #[test]
    fn explicit_fastembed_never_probes_or_prompts() {
        let selected = select::<()>(
            Backend::Fastembed,
            false,
            || panic!("probe"),
            |_| panic!("prompt"),
        );
        assert_eq!(selected.unwrap(), Selected::Fastembed);
    }

    #[test]
    fn available_ollama_never_prompts() {
        for backend in [Backend::Auto, Backend::Ollama] {
            assert_eq!(
                select(backend, true, || Ok("installed"), |_| panic!("prompt")).unwrap(),
                Selected::Ollama("installed")
            );
        }
    }

    #[test]
    fn explicit_ollama_never_falls_back() {
        let error = select::<()>(
            Backend::Ollama,
            true,
            || bail!("unavailable"),
            |_| panic!("prompt"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("ollama.com/start"));
        assert!(message.contains("ollama pull unclemusclez/jina-embeddings-v2-base-code"));
        assert!(message.contains("--embedding-model <name>"));
        assert!(message.contains("unavailable"));
    }

    #[test]
    fn auto_requires_consent_and_retains_discovery_reason() {
        for reason in [
            "server unavailable",
            "multiple models; --embedding-model a or b",
            "no embedding models",
        ] {
            let result = select::<()>(
                Backend::Auto,
                true,
                || bail!("{reason}"),
                |message| {
                    assert_eq!(message, reason);
                    Ok(Backend::Fastembed)
                },
            );
            assert_eq!(result.unwrap(), Selected::Fastembed);
            let error = select::<()>(
                Backend::Auto,
                true,
                || bail!("{reason}"),
                |_| Ok(Backend::Ollama),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(reason));
            assert!(message.contains("ollama pull unclemusclez/jina-embeddings-v2-base-code"));
        }
    }

    #[test]
    fn quiet_or_redirected_streams_never_prompt() {
        for quiet in [false, true] {
            for stdin in [false, true] {
                for stderr in [false, true] {
                    let interactive = can_prompt(quiet, stdin, stderr);
                    assert_eq!(interactive, !quiet && stdin && stderr);
                    if !interactive {
                        let error = select::<()>(
                            Backend::Auto,
                            interactive,
                            || bail!("not available"),
                            |_| panic!("prompt"),
                        )
                        .unwrap_err();
                        let message = format!("{error:#}");
                        assert!(message.contains("--embedding-backend fastembed"));
                        assert!(message.contains("--embedding-backend ollama"));
                        assert!(message.contains("not available"));
                        assert!(message.contains("unclemusclez/jina-embeddings-v2-base-code"));
                        assert!(!message.contains("ollama pull"));
                    }
                }
            }
        }
    }

    #[test]
    fn prompt_has_no_default_and_explains_cost_without_setup_noise() {
        for (answer, expected) in [
            ("1\n", Backend::Fastembed),
            ("fastembed\n", Backend::Fastembed),
            ("2\n", Backend::Ollama),
            ("Ollama\n", Backend::Ollama),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                prompt(answer.as_bytes(), &mut output, "test reason").unwrap(),
                expected
            );
            let text = String::from_utf8(output).unwrap();
            for expected in [
                "test reason",
                "642 MB",
                "CPU",
                "no server",
                "no automatic setup or downloads",
            ] {
                assert!(text.contains(expected));
            }
            for deferred in [
                "ollama.com/start",
                "ollama serve",
                "ollama pull",
                "--embedding-backend",
            ] {
                assert!(!text.contains(deferred));
            }
        }
        for answer in ["", "\n", "auto\n", "invalid\n"] {
            assert!(prompt(answer.as_bytes(), Vec::new(), "unavailable").is_err());
        }
    }

    #[test]
    fn ollama_metadata_keeps_existing_json_shape() {
        let metadata = OllamaMetadata {
            provider: "ollama".into(),
            endpoint: "http://localhost:11434".into(),
            model: "test".into(),
            digest: "digest".into(),
            dimensions: Some(3),
        };
        assert_eq!(
            serde_json::to_value(Metadata::Ollama(metadata.clone())).unwrap(),
            serde_json::to_value(metadata).unwrap()
        );
    }
}
