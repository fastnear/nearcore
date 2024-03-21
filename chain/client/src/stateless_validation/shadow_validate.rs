use std::time::Instant;

use lru::LruCache;
use near_chain::types::{RuntimeStorageConfig, StorageDataSource};
use near_chain::{Block, BlockHeader};
use near_chain_primitives::Error;
use near_primitives::challenge::PartialState;
use near_primitives::hash::CryptoHash;
use near_primitives::sharding::{ShardChunk, ShardChunkHeader};
use near_primitives::stateless_validation::{ChunkStateTransition, ChunkStateWitnessInner};
use near_primitives::types::ShardId;
use zstd::{decode_all, encode_all};

use crate::stateless_validation::chunk_validator::{
    pre_validate_chunk_state_witness, validate_chunk_state_witness, validate_prepared_transactions,
};
use crate::{metrics, Client};

impl Client {
    // Temporary feature to make node produce state witness for every chunk in every processed block
    // and then self-validate it.
    pub(crate) fn shadow_validate_block_chunks(&mut self, block: &Block) -> Result<(), Error> {
        if !cfg!(feature = "shadow_chunk_validation") {
            return Ok(());
        }
        let block_hash = block.hash();
        tracing::debug!(target: "stateless_validation", ?block_hash, "shadow validation for block chunks");
        let prev_block = self.chain.get_block(block.header().prev_hash())?;
        let prev_block_chunks = prev_block.chunks();
        for chunk in
            block.chunks().iter().filter(|chunk| chunk.is_new_chunk(block.header().height()))
        {
            let chunk = self.chain.get_chunk_clone_from_header(chunk)?;
            let prev_chunk_header = prev_block_chunks.get(chunk.shard_id() as usize).unwrap();
            if let Err(err) =
                self.shadow_validate_chunk(prev_block.header(), prev_chunk_header, &chunk)
            {
                metrics::SHADOW_CHUNK_VALIDATION_FAILED_TOTAL.inc();
                tracing::error!(
                    target: "stateless_validation",
                    ?err,
                    shard_id = chunk.shard_id(),
                    ?block_hash,
                    "shadow chunk validation failed"
                );
            }
        }
        Ok(())
    }

    fn shadow_validate_chunk(
        &mut self,
        prev_block_header: &BlockHeader,
        prev_chunk_header: &ShardChunkHeader,
        chunk: &ShardChunk,
    ) -> Result<(), Error> {
        let shard_id = chunk.shard_id();
        let chunk_hash = chunk.chunk_hash();
        let chunk_header = chunk.cloned_header();

        let transactions_validation_storage_config = RuntimeStorageConfig {
            state_root: chunk_header.prev_state_root(),
            use_flat_storage: true,
            source: StorageDataSource::Db,
            state_patch: Default::default(),
            record_storage: true,
        };

        // We call `validate_prepared_transactions()` here because we need storage proof for transactions validation.
        // Normally it is provided by chunk producer, but for shadow validation we need to generate it ourselves.
        let Ok(validated_transactions) = validate_prepared_transactions(
            &self.chain,
            self.runtime_adapter.as_ref(),
            &chunk_header,
            transactions_validation_storage_config,
            chunk.transactions(),
        ) else {
            return Err(Error::Other(
                "Could not produce storage proof for new transactions".to_owned(),
            ));
        };

        let witness = self.create_state_witness_inner(
            prev_block_header,
            prev_chunk_header,
            chunk,
            validated_transactions.storage_proof,
        )?;
        let witness_bytes = borsh::to_vec(&witness)?;
        let witness_size = witness_bytes.len();
        metrics::CHUNK_STATE_WITNESS_TOTAL_SIZE
            .with_label_values(&[&shard_id.to_string()])
            .observe(witness_size as f64);

        record_storage_proof_value_size_distribution(&witness);
        metrics::CHUNK_STATE_WITNESS_REDUCED_SIZE
            .with_label_values(&[&shard_id.to_string(), "baseline"])
            .observe(witness_size as f64);
        self.apply_witness_state_cache(witness.clone());
        {
            let witness_bytes = witness_bytes.clone();
            rayon::spawn(move || {
                compress_state_witness(shard_id, witness_bytes);
            });
        }
        {
            let witness = witness.clone();
            rayon::spawn(move || {
                compress_large_storage_proof_values(witness);
            });
        }

        let pre_validation_start = Instant::now();
        let pre_validation_result = pre_validate_chunk_state_witness(
            &witness,
            &self.chain,
            self.epoch_manager.as_ref(),
            self.runtime_adapter.as_ref(),
        )?;
        tracing::debug!(
            target: "stateless_validation",
            shard_id,
            ?chunk_hash,
            witness_size,
            pre_validation_elapsed = ?pre_validation_start.elapsed(),
            "completed shadow chunk pre-validation"
        );
        let epoch_manager = self.epoch_manager.clone();
        let runtime_adapter = self.runtime_adapter.clone();
        rayon::spawn(move || {
            let validation_start = Instant::now();
            match validate_chunk_state_witness(
                witness,
                pre_validation_result,
                epoch_manager.as_ref(),
                runtime_adapter.as_ref(),
            ) {
                Ok(()) => {
                    tracing::debug!(
                        target: "stateless_validation",
                        shard_id,
                        ?chunk_hash,
                        validation_elapsed = ?validation_start.elapsed(),
                        "completed shadow chunk validation"
                    );
                }
                Err(err) => {
                    metrics::SHADOW_CHUNK_VALIDATION_FAILED_TOTAL.inc();
                    tracing::error!(
                        target: "stateless_validation",
                        ?err,
                        shard_id,
                        ?chunk_hash,
                        "shadow chunk validation failed"
                    );
                }
            }
        });
        Ok(())
    }

    fn apply_witness_state_cache(&mut self, mut witness: ChunkStateWitnessInner) {
        let shard_id = witness.chunk_header.shard_id();
        self.apply_transition_state_cache(shard_id, &mut witness.main_state_transition);
        for transition in witness.implicit_transitions.iter_mut() {
            self.apply_transition_state_cache(shard_id, transition);
        }
        let witness_size = borsh::to_vec(&witness).unwrap().len();
        metrics::CHUNK_STATE_WITNESS_REDUCED_SIZE
            .with_label_values(&[&shard_id.to_string(), "cache_state_values"])
            .observe(witness_size as f64);
    }

    fn apply_transition_state_cache(
        &mut self,
        shard_id: ShardId,
        transition: &mut ChunkStateTransition,
    ) {
        const CUT_OFF_VALUE_SIZE: usize = 32000;
        const MAX_CACHE_SIZE: usize = 1000;
        let cache =
            self.state_cache.entry(shard_id).or_insert_with(|| LruCache::new(MAX_CACHE_SIZE));
        let PartialState::TrieValues(values) = &mut transition.base_state;
        values.retain(|v| {
            v.len() < CUT_OFF_VALUE_SIZE || cache.get(&CryptoHash::hash_bytes(v.as_ref())).is_none()
        });
        values.sort_by_key(|v| v.len());
        let mut updated = false;
        for v in values.iter().rev().filter(|v| v.len() >= CUT_OFF_VALUE_SIZE) {
            cache.push(CryptoHash::hash_bytes(v.as_ref()), ());
            updated = true;
        }
        if updated {
            metrics::STATE_VALUES_CACHE_UPDATED_COUNT
                .with_label_values(&[&shard_id.to_string()])
                .inc();
        }
        metrics::STATE_VALUES_CACHE_SIZE
            .with_label_values(&[&shard_id.to_string()])
            .set(cache.len() as i64);
    }
}

fn record_storage_proof_value_size_distribution(witness: &ChunkStateWitnessInner) {
    let ranges: Vec<_> = {
        let sizes = [0, 100, 1000, 4000, 16_000, 32_000, 64_000, 128_000];
        sizes
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                (
                    v..(*sizes.get(i + 1).unwrap_or(&usize::MAX)),
                    format!(
                        "{v}..{}",
                        sizes.get(i + 1).map(|v| v.to_string()).unwrap_or("inf".to_string())
                    ),
                )
            })
            .collect()
    };
    let shard_id = witness.chunk_header.shard_id();
    for transition in
        [&witness.main_state_transition].into_iter().chain(witness.implicit_transitions.iter())
    {
        let PartialState::TrieValues(values) = &transition.base_state;
        for val in values {
            for (rng, lbl) in ranges.iter() {
                if rng.contains(&val.len()) {
                    metrics::CHUNK_STATE_WITNESS_STORAGE_PROOF_VALUES_SIZE_TOTAL
                        .with_label_values(&[&shard_id.to_string(), lbl.as_str()])
                        .inc_by(val.len() as u64);
                    break;
                }
            }
        }
    }
}

fn compress_state_witness(shard_id: ShardId, witness_bytes: Vec<u8>) {
    for level in [3] {
        let strategy = format!("compress_witness_level_{level}");
        let _timer = metrics::CHUNK_STATE_WITNESS_COMPRESSION_TIME
            .with_label_values(&[&shard_id.to_string(), strategy.as_str()])
            .start_timer();
        let compressed_bytes = encode_all(witness_bytes.as_slice(), level).unwrap();
        metrics::CHUNK_STATE_WITNESS_REDUCED_SIZE
            .with_label_values(&[&shard_id.to_string(), strategy.as_str()])
            .observe(compressed_bytes.len() as f64);
        decode_all(compressed_bytes.as_slice()).unwrap();
    }
}

fn compress_large_storage_proof_values(mut witness: ChunkStateWitnessInner) {
    let strategy = "compress_storage_proof_values";
    let shard_id = witness.chunk_header.shard_id();
    {
        let _timer = metrics::CHUNK_STATE_WITNESS_COMPRESSION_TIME
            .with_label_values(&[&shard_id.to_string(), strategy])
            .start_timer();
        apply_transition_storage_proof_compression(&mut witness.main_state_transition);
        for transition in witness.implicit_transitions.iter_mut() {
            apply_transition_storage_proof_compression(transition);
        }
    }
    let witness_bytes = borsh::to_vec(&witness).unwrap();
    metrics::CHUNK_STATE_WITNESS_REDUCED_SIZE
        .with_label_values(&[&shard_id.to_string(), strategy])
        .observe(witness_bytes.len() as f64);
}

fn apply_transition_storage_proof_compression(
    transition: &mut ChunkStateTransition,
) {
    const CUT_OFF_VALUE_SIZE: usize = 128000;
    let PartialState::TrieValues(values) = &mut transition.base_state;
    for val in values {
        if val.len() >= CUT_OFF_VALUE_SIZE {
            let compressed = encode_all(val.as_ref(), 0).unwrap();
            decode_all(compressed.as_slice()).unwrap();
            *val = compressed.into();
        }
    }
}