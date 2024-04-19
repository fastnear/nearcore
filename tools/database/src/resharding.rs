use std::path::{Path, PathBuf};
use std::sync::Arc;

use near_async::time::Clock;
use near_chain::rayon_spawner::RayonAsyncComputationSpawner;
use near_chain::resharding::ReshardingResponse;
use near_chain::types::ChainConfig;
use near_chain::{Chain, ChainGenesis, DoomslugThresholdMode};
use near_epoch_manager::shard_tracker::{ShardTracker, TrackedConfig};
use near_epoch_manager::EpochManager;
use near_epoch_manager::EpochManagerAdapter;
use near_primitives::{hash::CryptoHash, types::EpochId};
use near_store::db::{MixedDB, ReadOrder, RocksDB, SplitDB};
use near_store::genesis::initialize_sharded_genesis_state;
use near_store::{Mode, NodeStorage, Store, Temperature};
use nearcore::NightshadeRuntimeExt;
use nearcore::{open_storage, NearConfig, NightshadeRuntime};

#[derive(clap::Args)]
pub(crate) struct ReshardingCommand {
    #[clap(long)]
    block_hash: CryptoHash,

    #[clap(long)]
    shard_id: u64,

    #[clap(long)]
    write_path: PathBuf,
}

impl ReshardingCommand {
    pub(crate) fn run(&self, mut config: NearConfig, home_dir: &Path) -> anyhow::Result<()> {
        Self::check_resharding_config(&mut config);

        let mut chain = self.get_chain(config, home_dir)?;

        let resharding_request = chain.custom_build_state_for_resharding_preprocessing(
            &self.block_hash,
            &self.block_hash,
            self.shard_id,
        )?;

        let shard_uid = resharding_request.shard_uid;

        let response = Chain::build_state_for_split_shards(resharding_request);
        let ReshardingResponse { sync_hash, new_state_roots: state_roots, .. } = response;

        let state_roots = state_roots?;
        tracing::info!(target: "resharding", ?state_roots, "state roots");

        chain.build_state_for_split_shards_postprocessing(shard_uid, &sync_hash, state_roots)?;

        Ok(())
    }

    fn get_store(&self, home_dir: &Path, config: &mut NearConfig) -> Result<Store, anyhow::Error> {
        // Open hot and cold as usual.
        let storage = open_storage(home_dir, config)?;
        let cold_db = storage.cold_db().unwrap().to_owned();
        let hot_db = storage.into_inner(Temperature::Hot);

        // We need real split db so that it correctly handles reads of missing
        // values in the columns that are not in the cold db.
        let split_db = SplitDB::new(hot_db, cold_db);

        // Open write db.
        let write_path = if self.write_path.is_absolute() {
            PathBuf::from(&self.write_path)
        } else {
            home_dir.join(&self.write_path)
        };
        let write_path = write_path.as_path();
        let write_config = &config.config.store;
        let write_db = RocksDB::open(write_path, write_config, Mode::ReadWrite, Temperature::Hot)?;
        let write_db = Arc::new(write_db);

        // Prepare the full mixed db.
        // It will read, in order, from write, hot and cold.
        // It will write only to the write db.
        let mixed_db = MixedDB::new(split_db, write_db, ReadOrder::WriteDBFirst);

        // The only way to create a Store is to go through NodeStorage.
        let storage = NodeStorage::new(mixed_db);
        let store = storage.get_hot_store();
        Ok(store)
    }

    fn get_chain(&self, mut config: NearConfig, home_dir: &Path) -> Result<Chain, anyhow::Error> {
        let store = self.get_store(home_dir, &mut config)?;

        let epoch_manager = EpochManager::new_arc_handle(store.clone(), &config.genesis.config);
        let genesis_epoch_config = epoch_manager.get_epoch_config(&EpochId::default())?;
        initialize_sharded_genesis_state(
            store.clone(),
            &config.genesis,
            &genesis_epoch_config,
            Some(home_dir),
        );
        let shard_tracker = ShardTracker::new(
            TrackedConfig::from_config(&config.client_config),
            epoch_manager.clone(),
        );
        let runtime_adapter =
            NightshadeRuntime::from_config(home_dir, store, &config, epoch_manager.clone())?;
        let chain_genesis = ChainGenesis::new(&config.genesis.config);
        let client_config = config.client_config;
        let chain_config = ChainConfig {
            save_trie_changes: client_config.save_trie_changes,
            background_migration_threads: client_config.client_background_migration_threads,
            resharding_config: client_config.resharding_config.clone(),
        };
        let chain = Chain::new(
            Clock::real(),
            epoch_manager.clone(),
            shard_tracker.clone(),
            runtime_adapter.clone(),
            &chain_genesis,
            DoomslugThresholdMode::TwoThirds,
            chain_config,
            None,
            Arc::new(RayonAsyncComputationSpawner),
            None,
        )
        .unwrap();
        Ok(chain)
    }

    // Rely on the regular config but make sure it's configured correctly for
    // the on demand resharding. It's executed while the node is not running so
    // it should be as fast as possible - there should be no throttling.
    fn check_resharding_config(config: &mut NearConfig) {
        if config.config.resharding_config.batch_delay != time::Duration::ZERO {
            panic!("batch_delay must be zero for on demand resharding");
        };

        if config.client_config.resharding_config.get().batch_delay != time::Duration::ZERO {
            panic!("batch_delay must be zero for on demand resharding");
        };
    }
}
