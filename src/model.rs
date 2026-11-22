use crate::fs::{lockfile, private_dir, read_regular, sync_dir};
use crate::profile::{self, Stage};
use crate::repo::Repo;
use crate::vectors::{self, Embedder, Model};
use anyhow::{Context, Result, ensure};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

pub(crate) const MODEL: Model = Model {
    id: "intfloat/multilingual-e5-small",
    revision: "614241f622f53c4eeff9890bdc4f31cfecc418b3",
    dimensions: 384,
};

const CONTEXT: usize = 512;

struct Artifact {
    name: &'static str,
    remote: &'static str,
    size: usize,
    sha256: Option<&'static str>,
}

const WEIGHTS: Artifact = Artifact {
    name: "model.onnx",
    remote: "onnx/model.onnx",
    size: 470_268_510,
    sha256: Some("ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665"),
};
const TOKENIZER: Artifact = Artifact {
    name: "tokenizer.json",
    remote: "tokenizer.json",
    size: 17_082_730,
    sha256: Some("0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39"),
};
const CONFIG: Artifact = Artifact {
    name: "config.json",
    remote: "config.json",
    size: 655,
    sha256: None,
};
const SPECIAL: Artifact = Artifact {
    name: "special_tokens_map.json",
    remote: "special_tokens_map.json",
    size: 167,
    sha256: None,
};
const TOK_CONFIG: Artifact = Artifact {
    name: "tokenizer_config.json",
    remote: "tokenizer_config.json",
    size: 443,
    sha256: None,
};
const ARTIFACTS: &[Artifact] = &[WEIGHTS, TOKENIZER, CONFIG, SPECIAL, TOK_CONFIG];

pub(crate) struct LocalModel(TextEmbedding, tokenizers::Tokenizer);

#[derive(Default)]
pub(crate) struct SharedModel(Mutex<Option<LoadedModel>>);

struct LoadedModel {
    files: ModelFiles,
    model: LocalModel,
    queries: QueryCache,
}

const MAX_CACHED_QUERIES: usize = 64;
const MAX_CACHED_QUERY_BYTES: usize = 4096;

#[derive(Default)]
struct QueryCache(VecDeque<(String, Vec<f32>)>);

impl QueryCache {
    fn embed(
        &mut self,
        text: &str,
        compute: impl FnOnce() -> Result<Vec<f32>>,
    ) -> Result<Vec<f32>> {
        let cacheable = text.len() <= MAX_CACHED_QUERY_BYTES;
        if let Some(entry) = self
            .0
            .iter()
            .position(|(key, _)| key == text)
            .and_then(|index| self.0.remove(index))
        {
            let vector = entry.1.clone();
            self.0.push_back(entry);
            profile::query_cache(true, true);
            return Ok(vector);
        }
        profile::query_cache(false, cacheable);
        let vector = compute()?;
        if cacheable {
            if self.0.len() == MAX_CACHED_QUERIES {
                self.0.pop_front();
            }
            self.0.push_back((text.to_owned(), vector.clone()));
        }
        Ok(vector)
    }
}

#[derive(PartialEq, Eq)]
struct ModelFiles {
    dir: PathBuf,
    artifacts: Vec<ArtifactStamp>,
}

#[derive(PartialEq, Eq)]
struct ArtifactStamp {
    dev: u64,
    ino: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl SharedModel {
    pub(crate) fn with<T>(
        &self,
        repo: &Repo,
        use_model: impl FnOnce(&mut LocalModel) -> Result<T>,
    ) -> Result<T> {
        self.with_loaded(repo, |loaded| use_model(&mut loaded.model))
    }

    pub(crate) fn query(&self, repo: &Repo, text: &str) -> Result<Vec<f32>> {
        self.with_loaded(repo, |loaded| {
            loaded.queries.embed(text, || {
                profile::measure(Stage::EmbedQuery, || loaded.model.query(text))
            })
        })
    }

    fn with_loaded<T>(
        &self,
        repo: &Repo,
        use_model: impl FnOnce(&mut LoadedModel) -> Result<T>,
    ) -> Result<T> {
        let mut cached = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("model session lock poisoned"))?;
        let dir = model_dir(repo)?;
        let artifacts = match ready(&dir) {
            Ok(artifacts) => artifacts,
            Err(error) => {
                *cached = None;
                return Err(error);
            }
        };
        let files = ModelFiles { dir, artifacts };
        if cached.as_ref().is_none_or(|loaded| loaded.files != files) {
            *cached = None;
            let model = profile::measure(Stage::ModelLoad, || LocalModel::from_dir(&files.dir))?;
            *cached = Some(LoadedModel {
                files,
                model,
                queries: QueryCache::default(),
            });
        }
        use_model(cached.as_mut().context("no cached ONNX session")?)
    }
}

impl LocalModel {
    pub(crate) fn load(repo: &Repo) -> Result<Self> {
        let dir = model_dir(repo)?;
        ready(&dir)?;
        Self::from_dir(&dir)
    }

    fn from_dir(dir: &Path) -> Result<Self> {
        let tokenizer = TokenizerFiles {
            tokenizer_file: TOKENIZER.read(dir)?,
            config_file: CONFIG.read(dir)?,
            special_tokens_map_file: SPECIAL.read(dir)?,
            tokenizer_config_file: TOK_CONFIG.read(dir)?,
        };
        let model = UserDefinedEmbeddingModel::new(WEIGHTS.read(dir)?, tokenizer)
            .with_pooling(Pooling::Mean);
        let options = InitOptionsUserDefined::default()
            .with_max_length(CONTEXT)
            .with_intra_threads(4);
        let embedding = TextEmbedding::try_new_from_user_defined(model, options)
            .with_context(|| format!("load FastEmbed/ONNX model from {}", dir.display()))?;
        let mut tokenizer = embedding.tokenizer.clone();
        tokenizer
            .with_truncation(None)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        tokenizer.with_padding(None);
        Ok(Self(embedding, tokenizer))
    }

    pub(crate) fn query(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut embeddings = self
            .0
            .embed([format!("query: {text}")], Some(1))
            .context("embed search query")?;
        ensure!(
            embeddings.len() == 1,
            "model returned the wrong number of query embeddings"
        );
        embeddings
            .pop()
            .context("model returned no query embedding")
    }
}

impl Embedder for LocalModel {
    fn model(&self) -> Model {
        MODEL
    }

    fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>> {
        let encoded = self
            .1
            .encode(text, false)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(encoded.get_offsets().to_vec())
    }

    fn passages(&mut self, passages: &[&str]) -> Result<Vec<Vec<f32>>> {
        let input: Vec<_> = passages
            .iter()
            .map(|text| format!("passage: {text}"))
            .collect();
        self.0
            .embed(input, Some(4))
            .context("embed passages with FastEmbed")
    }
}

fn model_dir(repo: &Repo) -> Result<PathBuf> {
    Ok(repo
        .model_cache()?
        .join(format!("multilingual-e5-small-{}", MODEL.revision)))
}

pub(crate) fn check_cache(repo: &Repo) -> Result<()> {
    ready(&model_dir(repo)?).map(drop)
}

fn ready(dir: &Path) -> Result<Vec<ArtifactStamp>> {
    let check = || -> Result<Vec<ArtifactStamp>> {
        ensure!(
            fs::symlink_metadata(dir)?.is_dir(),
            "expected a model directory"
        );
        let mut stamps = Vec::with_capacity(ARTIFACTS.len());
        for artifact in ARTIFACTS {
            let meta = fs::symlink_metadata(dir.join(artifact.name))
                .with_context(|| format!("inspect {}", artifact.name))?;
            ensure!(
                meta.is_file() && meta.len() == artifact.size as u64,
                "invalid {}",
                artifact.name
            );
            stamps.push(ArtifactStamp {
                dev: meta.dev(),
                ino: meta.ino(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
            });
        }
        Ok(stamps)
    };
    check().with_context(|| format!("model unavailable at {}; run nunc models download (move an incomplete cache aside first)", dir.display()))
}

pub(crate) fn download(repo: &Repo) -> Result<PathBuf> {
    let dir = model_dir(repo)?;
    let cache = dir.parent().context("missing model cache parent")?;
    fs::create_dir_all(cache)?;
    private_dir(cache)?;
    let lock = lockfile(&cache.join("download.lock"))?;
    lock.lock().context("lock model download")?;

    match fs::symlink_metadata(&dir) {
        Ok(_) => {
            ready(&dir)?;
            validate_model(&dir)?;
            return Ok(dir);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
        Err(err) => return Err(err).context("inspect model cache"),
    }
    let stage = tempfile::tempdir_in(cache)?;
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();

    for artifact in ARTIFACTS {
        eprintln!("Downloading {} ({} bytes)", artifact.remote, artifact.size);
        artifact
            .fetch(&agent, stage.path())
            .with_context(|| format!("download {}", artifact.remote))?;
    }
    validate_model(stage.path())?;
    sync_dir(stage.path())?;
    fs::rename(stage.path(), &dir)
        .with_context(|| format!("publish model cache {}", dir.display()))?;
    sync_dir(cache)?;
    Ok(dir)
}

fn validate_model(dir: &Path) -> Result<()> {
    let mut model = LocalModel::from_dir(dir)?;
    vectors::norm(&model.query("validate local model")?, MODEL.dimensions)?;
    Ok(())
}

impl Artifact {
    fn read(&self, dir: &Path) -> Result<Vec<u8>> {
        let bytes = read_regular(&dir.join(self.name), self.size)?;
        ensure!(bytes.len() == self.size, "unexpected size of {}", self.name);
        Ok(bytes)
    }

    fn fetch(&self, agent: &ureq::Agent, dir: &Path) -> Result<()> {
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{}?download=true",
            MODEL.id, MODEL.revision, self.remote
        );
        let mut response = agent.get(&url).call()?;
        let mut stream = response.body_mut().as_reader().take(self.size as u64 + 1);
        let mut file = File::create(dir.join(self.name))?;
        let mut digest = Sha256::new();
        let mut bytes = [0_u8; 64 * 1024];
        let mut size = 0;
        loop {
            let count = stream.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            size += count;
            digest.update(&bytes[..count]);
            file.write_all(&bytes[..count])?;
        }
        ensure!(
            size == self.size,
            "expected {} artifact bytes, got {size}",
            self.size
        );
        if let Some(expected) = self.sha256 {
            ensure!(
                format!("{:x}", digest.finalize()) == expected,
                "artifact checksum mismatch"
            );
        }
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn replacing_an_artifact_changes_the_resident_cache_identity() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for artifact in ARTIFACTS {
            File::create(dir.path().join(artifact.name))?.set_len(artifact.size as u64)?;
        }
        let before = ready(dir.path())?;
        let staged = dir.path().join("replacement");
        File::create(&staged)?.set_len(TOKENIZER.size as u64)?;
        fs::rename(staged, dir.path().join(TOKENIZER.name))?;
        assert!(ready(dir.path())? != before);
        Ok(())
    }

    #[test]
    fn exact_cache_keys_promote_entries_and_evict_the_least_recently_used() -> Result<()> {
        let mut cache = QueryCache::default();
        for index in 0..MAX_CACHED_QUERIES {
            cache.embed(&format!("query {index}"), || Ok(vec![index as f32]))?;
        }
        assert_eq!(
            cache.embed("query 0", || anyhow::bail!("already cached"))?,
            [0.0]
        );
        cache.embed("query 0 ", || Ok(vec![2.0]))?;
        assert_eq!(cache.0.len(), MAX_CACHED_QUERIES);
        assert_eq!(
            cache.embed("query 0", || anyhow::bail!("recently used"))?,
            [0.0]
        );
        assert_eq!(cache.embed("query 1", || Ok(vec![10.0]))?, [10.0]);
        assert_eq!(
            cache.embed("query 0 ", || anyhow::bail!("already cached"))?,
            [2.0]
        );
        Ok(())
    }

    #[test]
    fn failed_and_oversize_queries_do_not_enter_the_cache() -> Result<()> {
        let mut cache = QueryCache::default();
        assert!(
            cache
                .embed("failed", || anyhow::bail!("inference error"))
                .is_err()
        );
        let long = "x".repeat(MAX_CACHED_QUERY_BYTES + 1);
        assert_eq!(cache.embed(&long, || Ok(vec![1.0]))?, [1.0]);
        assert_eq!(cache.embed(&long, || Ok(vec![2.0]))?, [2.0]);
        assert!(cache.0.is_empty());
        Ok(())
    }
}
