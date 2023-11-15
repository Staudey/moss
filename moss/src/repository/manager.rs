// SPDX-FileCopyrightText: Copyright © 2020-2023 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;

use futures::{future, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::{fs, io};

use crate::db::meta;
use crate::{config, package, Installation};
use crate::{environment, stone};

use crate::repository::{self, Repository};

/// Manage a bunch of repositories
pub struct Manager {
    installation: Installation,
    repositories: HashMap<repository::Id, repository::Active>,
}

impl Manager {
    /// Create a [`Manager`] for the supplied [`Installation`]
    pub async fn new(installation: Installation) -> Result<Self, Error> {
        // Load all configs, default if none exist
        let configs = config::load::<repository::Map>(&installation.root)
            .await
            .unwrap_or_default();

        // Open all repo meta dbs and collect into hash map
        let repositories =
            future::try_join_all(configs.into_iter().map(|(id, repository)| async {
                let db = open_meta_db(&id, &installation).await?;

                Ok::<_, Error>((id.clone(), repository::Active { id, repository, db }))
            }))
            .await?
            .into_iter()
            .collect();

        Ok(Self {
            installation,
            repositories,
        })
    }

    /// Add a [`Repository`]
    pub async fn add_repository(
        &mut self,
        id: repository::Id,
        repository: Repository,
    ) -> Result<(), Error> {
        // Save repo as new config file
        // We save it as a map for easy merging across
        // multiple configuration files
        {
            let map = repository::Map::with([(id.clone(), repository.clone())]);

            config::save(&self.installation.root, &id, &map)
                .await
                .map_err(Error::SaveConfig)?;
        }

        let db = open_meta_db(&id, &self.installation).await?;

        self.repositories
            .insert(id.clone(), repository::Active { id, repository, db });

        Ok(())
    }

    /// Remove a [`Repository`]
    pub async fn remove_repository(&mut self, id: repository::Id) -> Result<(), Error> {
        self.repositories.remove(&id);

        let path = self.installation.repo_path(id.to_string());

        fs::remove_dir_all(path).await.map_err(Error::RemoveDir)?;

        Ok(())
    }

    /// Refresh all [`Repository`]'s by fetching it's latest index
    /// file and updating it's associated meta database
    pub async fn refresh_all(&mut self) -> Result<(), Error> {
        // Fetch index file + add to meta_db
        future::try_join_all(
            self.repositories
                .iter()
                .map(|(id, state)| refresh_index(id, state, &self.installation)),
        )
        .await?;

        Ok(())
    }

    /// Refresh a [`Repository`] by Id
    pub async fn refresh(&mut self, id: &repository::Id) -> Result<(), Error> {
        if let Some(repo) = self.repositories.get(id) {
            refresh_index(id, repo, &self.installation).await
        } else {
            Err(Error::UnknownRepo(id.clone()))
        }
    }

    /// Returns the active repositories held by this manager
    pub(crate) fn active(&self) -> impl Iterator<Item = repository::Active> + '_ {
        self.repositories.values().cloned()
    }

    /// List all of the known repositories
    pub fn list(&self) -> impl ExactSizeIterator<Item = (&repository::Id, &Repository)> {
        self.repositories
            .iter()
            .map(|(id, state)| (id, &state.repository))
    }
}

/// Open the meta db file, ensuring it's
/// directory exists
async fn open_meta_db(
    id: &repository::Id,
    installation: &Installation,
) -> Result<meta::Database, Error> {
    let dir = installation.repo_path(id.to_string());

    fs::create_dir_all(&dir).await.map_err(Error::CreateDir)?;

    let db = meta::Database::new(dir.join("db"), installation.read_only()).await?;

    Ok(db)
}

/// Fetches a stone index file from the repository URL,
/// saves it to the repo installation path, then
/// loads it's metadata into the meta db
async fn refresh_index(
    id: &repository::Id,
    state: &repository::Active,
    installation: &Installation,
) -> Result<(), Error> {
    let out_dir = installation.repo_path(id.to_string());

    fs::create_dir_all(&out_dir)
        .await
        .map_err(Error::CreateDir)?;

    let out_path = out_dir.join("stone.index");

    // Fetch index & write to `out_path`
    repository::fetch_index(state.repository.uri.clone(), &out_path).await?;

    // Wipe db since we're refreshing from a new index file
    state.db.wipe().await?;

    // Get a stream of payloads
    let (_, payloads) = stone::stream_payloads(&out_path).await?;

    // Update each payload into the meta db
    payloads
        .map_err(Error::ReadStone)
        // Batch up to `DB_BATCH_SIZE` payloads
        .chunks(environment::DB_BATCH_SIZE)
        // Transpose error for early bail
        .map(|results| results.into_iter().collect::<Result<Vec<_>, _>>())
        .try_for_each(|payloads| async {
            // Construct Meta for each payload
            let packages = payloads
                .into_iter()
                .filter_map(|payload| {
                    if let stone::read::PayloadKind::Meta(meta) = payload {
                        Some(meta)
                    } else {
                        None
                    }
                })
                .map(|payload| {
                    let meta = package::Meta::from_stone_payload(&payload.body)?;

                    // Create id from hash of meta
                    let hash = meta.hash.clone().ok_or(Error::MissingMetaField(
                        stone::payload::meta::Tag::PackageHash,
                    ))?;
                    let id = package::Id::from(hash);

                    Ok((id, meta))
                })
                .collect::<Result<Vec<_>, Error>>()?;

            // Batch add to db
            //
            // Sqlite supports up to 32k parametized query binds. Adding a
            // package has 13 binds x 1k batch size = 17k. This leaves us
            // overhead to add more binds in the future, otherwise we can
            // lower the `DB_BATCH_SIZE`.
            state.db.batch_add(packages).await.map_err(Error::Database)
        })
        .await?;

    Ok(())
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Missing metadata field: {0:?}")]
    MissingMetaField(stone::payload::meta::Tag),
    #[error("create directory")]
    CreateDir(#[source] io::Error),
    #[error("remove directory")]
    RemoveDir(#[source] io::Error),
    #[error("fetch index file")]
    FetchIndex(#[from] repository::FetchError),
    #[error("read index file")]
    ReadStone(#[from] stone::read::Error),
    #[error("meta db")]
    Database(#[from] meta::Error),
    #[error("save config")]
    SaveConfig(#[source] config::SaveError),
    #[error("unknown repo")]
    UnknownRepo(repository::Id),
}

impl From<package::MissingMetaError> for Error {
    fn from(error: package::MissingMetaError) -> Self {
        Self::MissingMetaField(error.0)
    }
}
