// SPDX-FileCopyrightText: Copyright © 2020-2023 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::path::{Path, PathBuf};

use clap::{arg, ArgMatches, Command};
use moss::{repository, Installation, Repository};
use thiserror::Error;
use tokio::runtime;
use url::Url;

enum Action {
    // Root
    List(PathBuf),
    // Root, Id, Url
    Add(PathBuf, String, Url),
}

pub fn command() -> Command {
    Command::new("repo")
        .about("Manage software repositories")
        .long_about("Manage the available software repositories visible to the installed system")
        .subcommand_required(true)
        .subcommand(
            Command::new("add")
                .arg(arg!(<NAME> "repo name").value_parser(clap::value_parser!(String)))
                .arg(arg!(<URI> "repo uri").value_parser(clap::value_parser!(Url))),
        )
        .subcommand(
            Command::new("list")
                .about("List system software repositories")
                .long_about("List all of the system repositories and their status"),
        )
}

/// Handle subcommands to `repo`
pub fn handle(args: &ArgMatches, root: &PathBuf) -> Result<(), Error> {
    let handler = match args.subcommand() {
        Some(("add", cmd_args)) => Action::Add(
            root.clone(),
            cmd_args.get_one::<String>("NAME").cloned().unwrap(),
            cmd_args.get_one::<Url>("URI").cloned().unwrap(),
        ),
        Some(("list", _)) => Action::List(root.clone()),
        _ => unreachable!(),
    };

    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // dispatch to runtime handler function
    match handler {
        Action::List(root) => rt.block_on(list(&root)),
        Action::Add(root, name, uri) => rt.block_on(add(&root, name, uri)),
    }
}

// Actual implementation of moss repo add, asynchronous
async fn add(root: &Path, name: String, uri: Url) -> Result<(), Error> {
    let installation = Installation::open(root);

    let mut manager = repository::Manager::new(installation).await?;

    manager
        .add_repository(
            repository::Id::new(name),
            Repository {
                description: "...".into(),
                uri,
                priority: 0,
            },
        )
        .await?;

    manager.refresh_all().await?;

    Ok(())
}

async fn list(root: &Path) -> Result<(), Error> {
    let installation = Installation::open(root);
    let manager = repository::Manager::new(installation).await?;

    let configured_repos = manager.list();
    if configured_repos.is_empty() {
        println!("No repositories have been configured yet");
        return Ok(());
    }

    for (id, repo) in configured_repos {
        println!(" - {} = {:?}", id, repo);
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("repo error: {0}")]
    RepositoryManager(#[from] repository::manager::Error),
}
