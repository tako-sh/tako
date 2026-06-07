use super::*;
use crate::commands::backups::BackupCommands;
use crate::commands::credentials::CredentialCommands;
use crate::commands::secret::SecretKeyCommands;
use crate::commands::storage::{StorageCommands, StorageProviderArg};
use clap::{CommandFactory, Parser};

mod backups;
mod credentials;
mod deploy;
mod dev;
mod general;
mod secrets;
mod servers;
mod storages;
