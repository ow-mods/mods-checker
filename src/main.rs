use std::io::Write;

use clap::Parser;
use cli::CheckerSubcommand;
use octocrab::{models::repos::Release, repos::RepoHandler, Octocrab};
use owmods_core::{
    config::Config,
    constants::{DEFAULT_ALERT_URL, DEFAULT_DB_URL},
    db::{LocalDatabase, RemoteDatabase},
    download::{install_mod_from_url, install_mod_from_zip},
    mods::local::{LocalMod, ModManifest},
};
use serde_derive::Serialize;
use tempfile::TempDir;
use thiserror::Error;
use versions::Versioning;

mod cli;

#[derive(Error, Debug, Serialize)]
pub enum CheckerError {
    #[error(
        "This unique name appears to be in use by another mod ({0}), please choose a different one"
    )]
    UniqueNameInUse(String),
    #[error("This mod's repo doesn't appear to exist, please double check the URL you specified")]
    MissingRepo,
    #[error("This mod appears to be missing a release, did you forget to publish it?")]
    MissingRelease,
    #[error("This mod has a release, but it's missing the mod asset, make sure you've uploaded a ZIP file")]
    MissingModAsset,
    #[error("This mod failed to install when testing it: {0}")]
    FailedToInstall(String),
    #[error(
        "The unique name of this mod is not what was expected, expected {expected} (from the unique name you gave), got {actual} (from the mod's manifest.json)"
    )]
    UnexpectedUniqueName { expected: String, actual: String },
    #[error("The version of this mod's manifest does not match the tag of the release, expected {expected} (from the release tag), got {actual} (from the mod's manifest.json). This can cause your mod to always be marked as out of date on the mod manager.")]
    UnexpectedVersion { expected: String, actual: String },
    #[error("This mod's manifest doesn't define a DLL file. Double check you specified `fileName` in manifest.json")]
    MissingDLL,
    #[error("This mod's manifest defines a DLL file that doesn't exist: {0}. Double check the name your specified in manifest.json")]
    InvalidDLL(String),
    #[error("This mod depends on another mod that seemingly doesn't exist: {0}")]
    MissingDependency(String),
}

#[derive(Error, Debug, Serialize)]
pub enum CheckerWarning {
    #[error("This mod's repo doesn't have a description, the description is used on the manager and website to describe your mod. You can add one by clicking the pencil icon on the right sidebar on your repo's main page")]
    MissingDescription,
    #[error("This mod's repo doesn't have a README, the README is used on the website to describe your mod. You can add one by creating a file called `README.md` at the root of your repo")]
    MissingReadme,
    #[error("The git tag ({0}) of this mod's release is not parsable semver, this can cause your mod to always be marked as out of date on the mod manager")]
    InvalidSemverTag(String),
}

type Result<T = (), E = CheckerError> = std::result::Result<T, E>;

async fn get_latest_release(repo: &RepoHandler<'_>) -> Result<Release> {
    eprintln!("Getting Latest Release...");
    let release = repo
        .releases()
        .get_latest()
        .await
        .map_err(|_| CheckerError::MissingRelease)?;
    Ok(release)
}

fn compare_unique_names(local_mod: &ModManifest, expected: &str) -> Result {
    eprintln!("Checking Unique Names...");
    let actual = local_mod.unique_name.as_str();
    if actual != expected {
        Err(CheckerError::UnexpectedUniqueName {
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    } else {
        Ok(())
    }
}

fn compare_tag_and_version(release: &Release, local_mod: &ModManifest) -> Result {
    eprintln!("Checking Versions...");
    let expected = release.tag_name.as_str().trim_start_matches('v');
    let actual = local_mod.version.as_str().trim_start_matches('v');
    if actual != expected {
        Err(CheckerError::UnexpectedVersion {
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    } else {
        Ok(())
    }
}

fn check_dependencies(local_mod: &ModManifest, remote_db: &RemoteDatabase) -> Result {
    eprintln!("Checking Dependencies...");
    if let Some(dependencies) = &local_mod.dependencies {
        for dependency in dependencies {
            if !remote_db.mods.contains_key(dependency) {
                return Err(CheckerError::MissingDependency(dependency.clone()));
            }
        }
    }
    Ok(())
}

async fn check_for_description_and_readme(
    octo: &Octocrab,
    owner: &str,
    repo_name: &str,
    repo: &RepoHandler<'_>,
    warnings: &mut Vec<CheckerWarning>,
) {
    eprintln!("Checking For Description...");
    let m_repo: octocrab::models::Repository = octo
        .get(format!("/repos/{owner}/{repo_name}"), None::<&()>)
        .await
        .unwrap();
    if m_repo
        .description
        .as_ref()
        .map(String::is_empty)
        .unwrap_or(true)
    {
        warnings.push(CheckerWarning::MissingDescription);
    }
    eprintln!("Checking For Readme...");
    if repo.get_readme().send().await.is_err() {
        warnings.push(CheckerWarning::MissingReadme);
    }
}

async fn check_mod(
    sub: CheckerSubcommand,
    expected_unique_name: Option<&str>,
    check_remote: bool,
    warnings: &mut Vec<CheckerWarning>,
) -> Result<Option<String>> {
    let working_dir = TempDir::new().unwrap();
    let path = working_dir.path();
    let config = Config {
        send_analytics: false,
        last_viewed_db_alert: None,
        owml_path: path.to_str().unwrap().to_string(),
        database_url: DEFAULT_DB_URL.to_string(),
        alert_url: DEFAULT_ALERT_URL.to_string(),
        viewed_alerts: vec![],
        path: path.join("config.json"),
    };

    eprintln!("Initializing RemoteDatabase...");

    let remote_db = RemoteDatabase::fetch(&config.database_url).await.unwrap();
    if check_remote {
        if let Some(unique_name) = expected_unique_name {
            eprintln!("Checking Unique Names...");
            if let Some(remote_mod) = remote_db.get_mod(unique_name) {
                return Err(CheckerError::UniqueNameInUse(remote_mod.name.clone()));
            }
        }
    }

    eprintln!("Initializing LocalDatabase...");

    let local_db = LocalDatabase::fetch(&config.owml_path).unwrap();

    let (local_mod, dl_url) = install_mod(sub, &config, local_db, warnings).await?;

    let manifest = local_mod.manifest;

    if let Some(unique_name) = expected_unique_name {
        compare_unique_names(&manifest, unique_name)?;
    } else if check_remote {
        eprintln!("Checking Unique Names...");
        if let Some(remote_mod) = remote_db.get_mod(&manifest.unique_name) {
            return Err(CheckerError::UniqueNameInUse(remote_mod.name.clone()));
        }
    }

    check_dependencies(&manifest, &remote_db)?;

    eprintln!("Passed All Critical Checks! Cleaning Up...");

    working_dir.close().unwrap();

    Ok(dl_url)
}

async fn install_mod(
    sub: CheckerSubcommand,
    config: &Config,
    local_db: LocalDatabase,
    warnings: &mut Vec<CheckerWarning>,
) -> Result<(LocalMod, Option<String>)> {
    match sub {
        CheckerSubcommand::Repo { repo } => {
            eprintln!("Fetching Repo...");

            let octo = octocrab::instance();
            let (owner, repo_name) = repo.split_once('/').ok_or(CheckerError::MissingRelease)?;
            let repo = octo.repos(owner, repo_name);

            if repo.get().await.is_err() {
                return Err(CheckerError::MissingRepo);
            }

            let release = get_latest_release(&repo).await?;

            let release_ver = Versioning::new(release.tag_name.as_str().trim_start_matches('v'));

            if release_ver.map_or(true, |v| !v.is_ideal()) {
                warnings.push(CheckerWarning::InvalidSemverTag(release.tag_name.clone()));
            }

            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name.ends_with(".zip"))
                .ok_or(CheckerError::MissingModAsset)?;
            let download_url = asset.browser_download_url.to_string();

            check_for_description_and_readme(&octo, owner, repo_name, &repo, warnings).await;

            eprintln!("Installing Mod...");

            let local_mod = install_mod_from_url(&download_url, None, config, &local_db)
                .await
                .map_err(|e| CheckerError::FailedToInstall(e.to_string()))?;

            compare_tag_and_version(&release, &local_mod.manifest)?;

            Ok((local_mod, Some(download_url)))
        }
        CheckerSubcommand::Url { url } => {
            eprintln!("Installing Mod...");
            let local_mod = install_mod_from_url(&url, None, config, &local_db)
                .await
                .map_err(|e| CheckerError::FailedToInstall(e.to_string()))?;
            Ok((local_mod, Some(url)))
        }
        CheckerSubcommand::File { file } => {
            eprintln!("Installing Mod...");
            let local_mod = install_mod_from_zip(&file, config, &local_db)
                .map_err(|e| CheckerError::FailedToInstall(e.to_string()))?;
            Ok((local_mod, None))
        }
    }
}

#[derive(Serialize)]
struct RawResult {
    url: Option<String>,
    warnings: Vec<CheckerWarning>,
    error: Option<CheckerError>,
}

impl RawResult {
    fn markdown(&self) -> String {
        let mut out = String::new();

        let mut issues = "### Issues\n\n".to_string();

        if let Some(e) = &self.error {
            issues.push_str(&format!("> [!CAUTION]\n> {}\n\n", e));
        }

        for warning in &self.warnings {
            issues.push_str(&format!("> [!WARNING]\n> {}\n\n", warning));
        }

        let issues = if issues == "### Issues\n\n" {
            String::new()
        } else {
            issues
        };

        out.push_str("### Results\n\n");

        if self.error.is_none() {
            out.push_str("> ✔ Success! This mod passed all checks!\n\n");
            let link = format!("owmods://install-url/{}", self.url.as_ref().unwrap());
            let text = format!("You can test installing your mod by pasting the link below into your URL bar, the mod manager should open and install it.\n\n```txt\n{link}\n```\n\n");
            out.push_str(&text);
            out.push_str("Now that all checks have passed, please wait until a database admin approves your mod.\n\n");
        } else if self.error.is_some() {
            out.push_str("> ❌ Failed, This mod doesn't seem to be valid, please fix the errors above and try again.\n\n");
            out.push_str("If you need help or believe this is a mistake, please [join the Discord](https://discord.gg/wusTQYbYTc).\n\n");
        }

        format!("## Mod Checker Report\n\nThis is an automated system to check your mod for common issues, please see the results below.\n\n{}{}\n", issues, out.trim_end())
    }
}

#[tokio::main]
async fn main() -> Result<(), CheckerError> {
    let cli = cli::CheckerCli::parse();

    let expected_unique_name = cli.expected_unique_name.as_deref();

    let mut warnings = vec![];

    let res = check_mod(
        cli.command,
        expected_unique_name,
        !cli.skip_exists,
        &mut warnings,
    )
    .await;

    let raw = match res {
        Ok(url) => RawResult {
            url,
            warnings,
            error: None,
        },
        Err(e) => RawResult {
            url: None,
            warnings,
            error: Some(e),
        },
    };

    if cli.output_md {
        let mut file = std::fs::File::create("results.md").unwrap();
        file.write_all(raw.markdown().as_bytes()).unwrap();
    }

    if cli.raw {
        println!("{}", serde_json::to_string(&raw).unwrap());
    } else {
        if let Some(e) = raw.error {
            eprintln!("Error: {}", e);
        }
        if !raw.warnings.is_empty() {
            eprintln!("Warnings:");
            for warning in raw.warnings {
                eprintln!("  {}", warning);
            }
        }
    }
    Ok(())
}
