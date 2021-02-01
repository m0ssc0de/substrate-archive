// Copyright 2017-2021 Parity Technologies (UK) Ltd.
// This file is part of substrate-archive.

// substrate-archive is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// substrate-archive is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with substrate-archive.  If not, see <http://www.gnu.org/licenses/>.

use std::{env, fs, io, marker::PhantomData, path::PathBuf, sync::Arc};

use serde::{de::DeserializeOwned, Deserialize};

use sc_chain_spec::ChainSpec;
use sc_client_api::backend as api_backend;
use sc_executor::NativeExecutionDispatch;
use sp_api::{ApiExt, ConstructRuntimeApi};
use sp_block_builder::BlockBuilder as BlockBuilderApi;
use sp_blockchain::{Backend as BlockchainBackend, Error as BlockchainError};
use sp_runtime::{
	generic::BlockId,
	traits::{BlakeTwo256, Block as BlockT, NumberFor},
};

use substrate_archive_backend::{
	runtime_api, ExecutionMethod, ReadOnlyBackend, ReadOnlyDB, RuntimeConfig, TArchiveClient,
};

use crate::{
	actors::{System, SystemConfig},
	database::{self, DatabaseConfig},
	error::Result,
	logger::{self, FileLoggerConfig, LoggerConfig},
	substrate_archive_default_dir,
};

/// Configure Chain.
#[derive(Clone, Debug, Deserialize)]
pub struct ChainConfig {
	/// chain path to the rocksdb database.
	pub(crate) data_path: Option<PathBuf>,
	/// how much cache should rocksdb keep
	#[serde(default = "default_cache_size")]
	pub(crate) cache_size: usize,
	/// rocksdb secondary directory
	pub(crate) rocksdb_secondary_path: Option<PathBuf>,
	/// chain spec.
	#[serde(skip)]
	pub(crate) spec: Option<ChainSpecConfig>,
}

/// Configure Chain Spec.
#[derive(Clone, Debug)]
pub(crate) struct ChainSpecConfig {
	/// chain spec name.
	pub name: String,
	/// chain spec id.
	pub id: String,
}

impl Default for ChainConfig {
	fn default() -> Self {
		Self { data_path: None, cache_size: default_cache_size(), rocksdb_secondary_path: None, spec: None }
	}
}

// 128 MB
fn default_cache_size() -> usize {
	128
}

/// Configure WASM Tracing.
#[derive(Clone, Debug, Deserialize)]
pub struct TracingConfig {
	/// Targets for tracing.
	#[serde(default)]
	pub targets: String,
	/// Folder where Tracing-Enabled WASM Binaries are kept.
	/// Folder should contain all runtime-versions for their chain
	/// that a user should want to collect traces from.
	pub folder: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ControlConfig {
	/// number of threads to spawn for task execution.
	#[serde(default = "default_task_workers")]
	pub(crate) task_workers: usize,
	/// maximum amount of blocks to index at once
	#[serde(default = "default_max_block_load")]
	pub(crate) max_block_load: u32,
}

impl Default for ControlConfig {
	fn default() -> Self {
		Self { task_workers: default_task_workers(), max_block_load: default_max_block_load() }
	}
}

fn default_task_workers() -> usize {
	num_cpus::get()
}

// 100_000 blocks to index at once
fn default_max_block_load() -> u32 {
	100_000
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ArchiveConfig {
	/// chain spec and database
	#[serde(default)]
	pub chain: ChainConfig,
	/// runtime execution
	#[serde(default)]
	pub runtime: RuntimeConfig,
	/// postgres url
	pub database: Option<DatabaseConfig>,
	/// Control the actor system
	#[serde(default)]
	pub control: ControlConfig,
	/// Logger
	#[serde(default)]
	pub log: LoggerConfig,
	/// Enable state tracing while also specifying the targets
	/// and directory where the WASM runtimes are stored.
	pub wasm_tracing: Option<TracingConfig>,
}

/// The control interface of an archive system.
#[async_trait::async_trait(?Send)]
pub trait Archive<B: BlockT + Unpin, D: ReadOnlyDB>
where
	B::Hash: Unpin,
{
	/// start driving the execution of the archive
	fn drive(&mut self) -> Result<()>;

	/// this method will block indefinitely
	async fn block_until_stopped(&self) -> ();

	/// shutdown the system
	fn shutdown(self) -> Result<()>;

	/// Shutdown the system when self is boxed (useful when erasing the types of the runtime)
	fn boxed_shutdown(self: Box<Self>) -> Result<()>;

	/// Get a reference to the context the actors are using
	fn context(&self) -> Result<SystemConfig<B, D>>;
}

pub struct ArchiveBuilder<B, R, D, DB> {
	_marker: PhantomData<(B, R, D, DB)>,
	config: ArchiveConfig,
}

impl<B, R, D, DB> Default for ArchiveBuilder<B, R, D, DB> {
	fn default() -> Self {
		Self { _marker: PhantomData, config: ArchiveConfig::default() }
	}
}

impl<B, R, D, DB> ArchiveBuilder<B, R, D, DB> {
	/// Creates a archive builder with the given config.
	pub fn with_config(config: Option<ArchiveConfig>) -> Self {
		if let Some(config) = config {
			Self { _marker: PhantomData, config }
		} else {
			Self::default()
		}
	}

	/// Specify a chain spec name and id for storing metadata about the running archiver
	/// in a persistent directory.
	///
	/// # Default
	/// defaults to storing metadata in a temporary directory.
	pub fn chain_spec(mut self, spec: Box<dyn ChainSpec>) -> Self {
		self.config.chain.spec = Some(ChainSpecConfig { name: spec.name().into(), id: spec.id().into() });
		self
	}

	/// Set the chain data backend path to use for this instance.
	///
	/// # Default
	/// defaults to the environment variable CHAIN_DATA_DB
	pub fn chain_data_path<S: Into<PathBuf>>(mut self, path: S) -> Self {
		self.config.chain.data_path = Some(path.into());
		self
	}

	/// Set the amount of cache Rocksdb should keep.
	///
	/// # Default
	/// defaults to 128MB
	pub fn cache_size(mut self, cache_size: usize) -> Self {
		self.config.chain.cache_size = cache_size;
		self
	}

	/// Set the path of rocksdb secondary directory according to the chain spec.
	///
	/// # Default
	/// defaults to storing metadata in a temporary directory.
	pub fn rocksdb_secondary_path<S: Into<PathBuf>>(mut self, path: S) -> Self {
		self.config.chain.rocksdb_secondary_path = Some(path.into());
		self
	}

	/// Set the url to the Postgres Database
	///
	/// # Default
	/// defaults to value of the environment variable DATABASE_URL
	pub fn pg_url<S: Into<String>>(mut self, url: S) -> Self {
		self.config.database = Some(DatabaseConfig { url: url.into() });
		self
	}

	/// Set the method of executing the runtime Wasm code.
	///
	/// # Default
	/// defaults to the interpreted method.
	pub fn execution_method(mut self, method: ExecutionMethod) -> Self {
		self.config.runtime.exec_method = method;
		self
	}

	/// Set the number of threads spawn for block execution.
	///
	/// # Default
	/// defaults to the number of logical cpus in the system
	pub fn block_workers(mut self, workers: usize) -> Self {
		self.config.runtime.block_workers = workers;
		self
	}

	/// Number of 64KB Heap Pages to allocate for WASM execution
	///
	/// # Default
	/// defaults to 64 * (number of logic cpu's)
	pub fn wasm_pages(mut self, pages: u64) -> Self {
		self.config.runtime.wasm_pages = pages;
		self
	}

	/// Set the  number of threads spawn for task execution.
	///
	/// # Default
	/// defaults to the number of logical cpus in the system
	pub fn task_workers(mut self, workers: usize) -> Self {
		self.config.control.task_workers = workers;
		self
	}

	/// Set the number of blocks to index at once
	///
	/// # Default
	/// defaults to 100_000
	pub fn max_block_load(mut self, max_block_load: u32) -> Self {
		self.config.control.max_block_load = max_block_load;
		self
	}

	/// Set the log level of stdout.
	///
	/// # Default
	/// defaults to `DEBUG`
	pub fn log_std_level(mut self, level: log::LevelFilter) -> Self {
		self.config.log.std = level;
		self
	}

	/// Set the log level of file.
	///
	/// # Default
	/// defaults to `DEBUG`
	pub fn log_file_level(mut self, level: log::LevelFilter) -> Self {
		if let Some(file) = &mut self.config.log.file {
			file.level = level;
		} else {
			self.config.log.file = Some(FileLoggerConfig { level, ..Default::default() });
		}
		self
	}

	/// Set the log file directory path.
	///
	/// # Default
	/// defaults to `/<local>/substrate-archive`
	pub fn log_file_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
		if let Some(file) = &mut self.config.log.file {
			file.dir = Some(dir.into());
		} else {
			self.config.log.file = Some(FileLoggerConfig { dir: Some(dir.into()), ..Default::default() });
		}
		self
	}

	/// Set the log file name
	///
	/// # Default
	/// defaults to `archive.log`
	pub fn log_file_name<S: Into<String>>(mut self, name: S) -> Self {
		if let Some(file) = &mut self.config.log.file {
			file.name = name.into();
		} else {
			self.config.log.file = Some(FileLoggerConfig { name: name.into(), ..Default::default() });
		}
		self
	}

	/// Set the folder and targets for tracing.
	/// This tells substrate-archive to also store all state-traces resulting from the execution of blocks.
	///
	/// # Note
	/// Traces will only be collected if a coexisting WASM binary
	/// for the runtime version of the block being currently executed is available.
	///
	/// # Default
	/// Wasm Tracing is disabled by default.
	pub fn wasm_tracing(mut self, wasm_tracing: Option<TracingConfig>) -> Self {
		self.config.wasm_tracing = wasm_tracing;
		self
	}
}

impl<B, R, D, DB> ArchiveBuilder<B, R, D, DB>
where
	DB: ReadOnlyDB + 'static,
	B: BlockT + Unpin + DeserializeOwned,
	R: ConstructRuntimeApi<B, TArchiveClient<B, R, D, DB>> + Send + Sync + 'static,
	R::RuntimeApi: BlockBuilderApi<B, Error = BlockchainError>
		+ sp_api::Metadata<B, Error = BlockchainError>
		+ ApiExt<B, StateBackend = api_backend::StateBackendFor<ReadOnlyBackend<B, DB>, B>>
		+ Send
		+ Sync
		+ 'static,
	D: NativeExecutionDispatch + 'static,
	<R::RuntimeApi as sp_api::ApiExt<B>>::StateBackend: sp_api::StateBackend<BlakeTwo256>,
	NumberFor<B>: Into<u32> + From<u32> + Unpin,
	B::Hash: Unpin,
	B::Header: serde::de::DeserializeOwned,
{
	/// Build this instance of the Archiver.
	/// Runs the database migrations for the database at `pg_url`.
	///
	/// # Panics
	/// Panics if one of chain_data_db or pg_url is not passed to the builder
	/// and their respective environment variables are not set.
	pub fn build(mut self) -> Result<impl Archive<B, DB>> {
		// config logger
		logger::init(self.config.log.clone())?;
		log::debug!("Archive Config: {:?}", self.config);

		// config chain
		const CHAIN_DATA_VAR: &str = "CHAIN_DATA_DB";
		let chain_path = self
			.config
			.chain
			.data_path
			.unwrap_or_else(|| env::var(CHAIN_DATA_VAR).expect("missing CHAIN_DATA_DB").into());
		let chain_path = chain_path.to_str().expect("chain data path is invalid");
		let db_path = create_database_path(self.config.chain.rocksdb_secondary_path, self.config.chain.spec)?;
		let db = Arc::new(DB::open_database(chain_path, self.config.chain.cache_size, db_path)?);

		// config runtime
		self.config.runtime.wasm_runtime_overrides = self.config.wasm_tracing.as_ref().and_then(|c| c.folder.clone());

		// configure substrate client and backend
		let client = Arc::new(runtime_api::<B, R, D, DB>(db.clone(), self.config.runtime)?);
		let backend = Arc::new(ReadOnlyBackend::new(db, true));
		Self::startup_info(&*client, &*backend)?;

		// config postgres database
		const POSTGRES_VAR: &str = "DATABASE_URL";
		let pg_url = self
			.config
			.database
			.map(|config| config.url)
			.unwrap_or_else(|| env::var(POSTGRES_VAR).expect("missing DATABASE_URL"));
		smol::block_on(database::migrate(&pg_url))?;

		// config actor system
		let config = SystemConfig::new(
			backend,
			pg_url,
			client.clone(),
			self.config.control.task_workers,
			self.config.control.max_block_load,
			self.config.wasm_tracing.map(|t| t.targets),
		);
		let sys = System::<_, R, _, _>::new(client, config)?;
		Ok(sys)
	}

	/// Log some general startup info
	fn startup_info(client: &TArchiveClient<B, R, D, DB>, backend: &ReadOnlyBackend<B, DB>) -> Result<()> {
		let last_finalized_block = backend.last_finalized()?;
		let rt = client.runtime_version_at(&BlockId::Hash(last_finalized_block))?;
		log::info!(
            "Running archive for 🔗 `{}`, implementation `{}`. Latest known runtime version: {}. Latest finalized block {} 🛡️",
            rt.spec_name,
            rt.impl_name,
            rt.spec_version,
            last_finalized_block
        );
		/// The recommended open file descriptor limit to be configured for the process.
		const RECOMMENDED_OPEN_FILE_DESCRIPTOR_LIMIT: u64 = 10_000;
		if let Some(new_limit) = fdlimit::raise_fd_limit() {
			if new_limit < RECOMMENDED_OPEN_FILE_DESCRIPTOR_LIMIT {
				log::warn!(
					"⚠️  Low open file descriptor limit configured for the process. \
                     Current value: {:?}, recommended value: {:?}.",
					new_limit,
					RECOMMENDED_OPEN_FILE_DESCRIPTOR_LIMIT,
				);
			}
		}
		Ok(())
	}
}

/// Create rocksdb secondary directory if it doesn't exist yet.
/// If the ChainSpec is not specified, a temporary directory is used.
/// Return path to that directory
///
/// # Panics
///
/// Panics if the directories fail to be created.
fn create_database_path(db_path: Option<PathBuf>, spec: Option<ChainSpecConfig>) -> io::Result<PathBuf> {
	match (db_path, spec) {
		(Some(mut db_path), Some(spec)) => {
			db_path.extend(&["rocksdb_secondary", &spec.name, &spec.id]);
			fs::create_dir_all(db_path.as_path())?;
			Ok(db_path)
		}
		(None, Some(spec)) => {
			let mut path = substrate_archive_default_dir();
			path.extend(&["rocksdb_secondary", &spec.name, &spec.id]);
			fs::create_dir_all(path.as_path())?;
			Ok(path)
		}
		_ => {
			// TODO: make sure this is cleaned up on kill
			let mut tmp_path = tempfile::tempdir()?.into_path();
			tmp_path.push("rocksdb_secondary");
			Ok(tmp_path)
		}
	}
}
