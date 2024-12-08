use std::{collections::HashMap, io::BufReader, path::Path, str::FromStr};

use anyhow::{Context, Result};
use config::Config;
use git2::{Cred, Direction, IndexAddOption, RemoteCallbacks, Repository, ResetType, Signature};
use oci_distribution::{client::ClientConfig, secrets::RegistryAuth, Client, Reference};
use overrides::Overrides;
use regex::Regex;
use rocket::{
    request::{FromRequest, Outcome},
    routes, Request, State,
};
use serde_yaml::Value;
use tempfile::TempDir;
use walkdir::WalkDir;

mod config;
mod overrides;

#[rocket::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    let temp_dir = TempDir::with_prefix("image-updater")?;
    let prefix = std::env::var("PREFIX").unwrap_or_else(|_| "/".to_string());

    let config = Config {
        repository_url: std::env::var("REPOSITORY_URL").context("REPOSITORY_URL")?,
        ssh_key_path: std::env::var("SSH_KEY_PATH").context("SSH_KEY_PATH")?,
        github_username: std::env::var("GITHUB_USERNAME").context("GITHUB_KEY")?,
        github_key: std::env::var("GITHUB_KEY").context("GITHUB_KEY")?,
        repo_tmpdir: temp_dir.path().to_path_buf(),
        secret: std::env::var("SECRET").context("SECRET")?,
    };

    clone_or_reset(
        &config.repository_url,
        &config.repo_tmpdir,
        Path::new(&config.ssh_key_path),
    )?;

    log::info!("Starting rocket");

    rocket::build()
        .mount(&prefix, routes![root])
        .manage(config)
        .launch()
        .await?;

    Ok(())
}

pub struct SecretGuard;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SecretGuard {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let secret = req.headers().get_one("X-Secret");
        let Some(config) = req.rocket().state::<Config>() else {
            return Outcome::Error((rocket::http::Status::InternalServerError, ()));
        };

        if secret == Some(&config.secret) {
            return Outcome::Success(SecretGuard);
        }

        Outcome::Error((rocket::http::Status::Unauthorized, ()))
    }
}

#[rocket::get("/")]
async fn root(config: &State<Config>, _secret: SecretGuard) {
    log::info!("Update triggered by webhook");

    if let Err(e) = update(config).await {
        log::error!("Error while updating: {}", e);
    }

    log::info!("Update complete");
}

async fn update(config: &State<Config>) -> Result<()> {
    let repo = clone_or_reset(
        &config.repository_url,
        &config.repo_tmpdir,
        Path::new(&config.ssh_key_path),
    )?;
    let candidates = find_candidates(&config.repo_tmpdir)?;

    let mut has_changed = false;
    for candidate in candidates {
        let tag =
            get_latest_tag_for_candidate(&candidate, &config.github_username, &config.github_key)
                .await?;
        has_changed |= update_tag_for_candidate(&config.repo_tmpdir, &candidate, &tag)?;
    }

    if !has_changed {
        log::info!("No image changes, skipping commit and push");
        return Ok(());
    }

    add_and_commit(&repo)?;
    let mut remote = repo.find_remote("origin")?;
    let mut cb = RemoteCallbacks::new();
    cb.credentials(|_, username, _| {
        Cred::ssh_key(
            username.unwrap_or("git"),
            None,
            Path::new(&config.ssh_key_path),
            None,
        )
    });

    let mut connection = remote.connect_auth(Direction::Push, Some(cb), None)?;
    connection.remote().push(&["refs/heads/main"], None)?;

    Ok(())
}

fn add_and_commit(repo: &Repository) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(["."], IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let oid = index.write_tree()?;
    let signature = Signature::now("Automatic image updater", "nobody@bananium.fr")?;
    let parent_commit = repo.head()?.peel_to_commit()?;
    let tree = repo.find_tree(oid).unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Updated images",
        &tree,
        &[&parent_commit],
    )?;

    Ok(())
}

fn update_tag_for_candidate(repo_path: &Path, candidate: &Candidate, tag: &str) -> Result<bool> {
    let overrides_path = repo_path
        .join(&candidate.path)
        .join(format!(".argocd-source-{}.yaml", candidate.app_name));
    let mut current_overrides = if overrides_path.exists() {
        serde_yaml::from_str(&std::fs::read_to_string(&overrides_path)?)?
    } else {
        Overrides::default()
    };

    let mut has_changed = false;

    for parameters in &mut current_overrides.helm.parameters.0 {
        if parameters.name == candidate.helm_image_tag && parameters.value != tag {
            log::info!("Updating {} to {}", candidate.url, tag);
            parameters.value = tag.to_string();
            has_changed = true;
        }
    }

    if has_changed {
        std::fs::write(overrides_path, serde_yaml::to_string(&current_overrides)?)?;
    }

    Ok(has_changed)
}

async fn get_latest_tag_for_candidate(
    candidate: &Candidate,
    github_username: &str,
    github_key: &str,
) -> Result<String> {
    log::info!("Getting latest tag for candidate: {}", candidate.url);
    let tags_re = Regex::new(candidate.allow_tags.trim_start_matches("regexp:"))?;
    let config = ClientConfig::default();
    let client = Client::new(config);

    let auth = RegistryAuth::Basic(github_username.to_string(), github_key.to_string());
    let reference = Reference::from_str(&candidate.url)?;

    let mut tags = client
        .list_tags(&reference, &auth, None, None)
        .await?
        .tags
        .into_iter()
        .filter(|name| tags_re.is_match(name))
        .collect::<Vec<_>>();

    tags.sort_by(|a, b| alphanumeric_sort::compare_path(a, b));

    Ok(tags
        .last()
        .with_context(|| format!("No tags matched the regex for {}", candidate.app_name))?
        .to_string())
}

#[derive(Clone, Debug)]
pub struct Candidate {
    app_name: String,
    url: String,
    allow_tags: String,
    helm_image_tag: String,
    path: String,
}

fn find_candidates(repo_path: &Path) -> Result<Vec<Candidate>> {
    log::info!("Extracting candidates");
    let mut candidates = vec![];

    for entry in WalkDir::new(repo_path).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }

        let Some(file_name) = entry.file_name().to_str() else {
            continue;
        };

        if !file_name.ends_with(".yaml") && !file_name.ends_with(".yml") {
            continue;
        }

        let candidates_from_file = get_candidates_from(entry.path())?;
        candidates.extend(candidates_from_file.into_iter());
    }

    Ok(candidates)
}

fn get_candidates_from(file_path: &Path) -> Result<Vec<Candidate>> {
    let content = std::fs::read_to_string(file_path)?;

    let yaml = content
        .trim()
        .trim_start_matches("---")
        .trim_end_matches("---");

    let reader = BufReader::new(yaml.as_bytes());
    let documents = yaml_split::DocumentIterator::new(reader);
    let mut candidates = vec![];
    for document in documents {
        let Ok(parsed) = serde_yaml::from_str::<HashMap<String, Value>>(&document?) else {
            log::debug!("Couldn't parse {:?}. Ignoring it.", file_path);
            continue;
        };

        if !is_argo_app(&parsed) {
            continue;
        }

        let Some(metadata) = parsed.get("metadata").and_then(Value::as_mapping) else {
            log::debug!("No metadata, ignoring");
            continue;
        };
        let Some(annotations) = metadata.get("annotations").and_then(Value::as_mapping) else {
            log::debug!("No annotations, ignoring");
            continue;
        };
        let Some(image_list) = annotations
            .get("argocd-image-updater.argoproj.io/image-list")
            .and_then(Value::as_str)
        else {
            log::debug!("No image_list, ignoring");
            continue;
        };

        let Some(spec) = parsed.get("spec").and_then(Value::as_mapping) else {
            continue;
        };
        let Some(source) = spec.get("source").and_then(Value::as_mapping) else {
            continue;
        };
        let Some(path) = source.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(app_name) = metadata.get("name").and_then(Value::as_str) else {
            continue;
        };

        let images = image_list.split(',');
        for image in images {
            let image = image.trim();
            let Some((name, url)) = image.split_once('=') else {
                continue;
            };

            let Some(allow_tags) = annotations
                .get(format!(
                    "argocd-image-updater.argoproj.io/{}.allow-tags",
                    name
                ))
                .and_then(Value::as_str)
            else {
                log::warn!("Found image {} without `allow-tags`. Ignoring.", name);
                continue;
            };
            let Some(helm_image_tag) = annotations
                .get(format!(
                    "argocd-image-updater.argoproj.io/{}.helm.image-tag",
                    name
                ))
                .and_then(Value::as_str)
            else {
                log::warn!("Found image {} without `helm.image-tag`. Ignoring.", name);
                continue;
            };

            candidates.push(Candidate {
                app_name: app_name.to_string(),
                url: url.to_string(),
                allow_tags: allow_tags.to_string(),
                helm_image_tag: helm_image_tag.to_string(),
                path: path.to_string(),
            });
        }
    }

    Ok(candidates)
}

fn is_argo_app(value: &HashMap<String, Value>) -> bool {
    let api_version = value.get("apiVersion").and_then(|v| v.as_str());
    let kind = value.get("kind").and_then(|v| v.as_str());

    kind == Some("Application") && api_version == Some("argoproj.io/v1alpha1")
}

fn clone_or_reset(repo_url: &str, repo_path: &Path, ssh_key_path: &Path) -> Result<Repository> {
    log::info!("Resetting upstream repo");

    let repo = Repository::init(repo_path)?;
    {
        let mut remote = repo
            .find_remote("origin")
            .or_else(|_| repo.remote("origin", repo_url))?;

        let mut cb = RemoteCallbacks::new();
        cb.credentials(|_, username, _| {
            Cred::ssh_key(username.unwrap_or("git"), None, ssh_key_path, None)
        });

        let mut connection = remote.connect_auth(Direction::Fetch, Some(cb), None)?;
        connection.remote().fetch(&["main"], None, None)?;

        let fetch_head = repo.find_reference("FETCH_HEAD")?;
        repo.reset(
            &fetch_head.peel(git2::ObjectType::Commit)?,
            ResetType::Hard,
            None,
        )?;
    }

    Ok(repo)
}
