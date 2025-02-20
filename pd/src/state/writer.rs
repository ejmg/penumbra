use std::collections::VecDeque;

use anyhow::Result;
use jmt::TreeWriterAsync;
use penumbra_chain::params::ChainParams;
use penumbra_crypto::merkle::{self, TreeExt};
use penumbra_proto::Protobuf;
use penumbra_stake::{FundingStream, RateDataById, ValidatorStateName};
use sqlx::{query, Pool, Postgres};
use tendermint::block;
use tokio::sync::watch;

use super::jellyfish;
use crate::{genesis, PendingBlock, NUM_RECENT_ANCHORS};

#[derive(Debug)]
pub struct Writer {
    pub(super) pool: Pool<Postgres>,
    // A state::Reader instance that uses the same connection pool as this
    // Writer, allowing it to read (e.g., for transaction verification) without
    // risking contention with other users of the read connection pool.
    pub(super) private_reader: super::Reader,
    //pub(super) tmp: evmap::WriteHandle<&'static str, String>,
    // Push channels for chain state
    pub(super) chain_params_tx: watch::Sender<ChainParams>,
    pub(super) height_tx: watch::Sender<block::Height>,
    pub(super) next_rate_data_tx: watch::Sender<RateDataById>,
    pub(super) valid_anchors_tx: watch::Sender<VecDeque<merkle::Root>>,
}

impl Writer {
    /// Initializes in-memory caches / notification channels.
    /// Called by `state::new()` on init.
    pub(super) async fn init_caches(&self) -> Result<()> {
        let chain_params = self
            .private_reader
            .genesis_configuration()
            .await?
            .chain_params;
        let height = self.private_reader.height().await?;
        let next_rate_data = self.private_reader.next_rate_data().await?;
        let valid_anchors = self
            .private_reader
            .recent_anchors(NUM_RECENT_ANCHORS)
            .await?;

        // Sends fail if every receiver has been dropped, which is not our problem.
        let _ = self.chain_params_tx.send(chain_params);
        let _ = self.height_tx.send(height);
        let _ = self.next_rate_data_tx.send(next_rate_data);
        let _ = self.valid_anchors_tx.send(valid_anchors);

        Ok(())
    }

    /// Borrow a private `state::Reader` instance that uses the same connection
    /// pool as this writer.  This allows the writer to read data from the
    /// database without contention from other `state::Reader`s.
    pub fn private_reader(&self) -> &super::Reader {
        &self.private_reader
    }

    /// Commits the genesis config to the database, prior to the first block commit.
    pub async fn commit_genesis(&self, genesis_config: &genesis::AppState) -> Result<()> {
        let mut dbtx = self.pool.begin().await?;

        let genesis_bytes = serde_json::to_vec(&genesis_config)?;

        // ON CONFLICT is excluded here so that an error is raised
        // if genesis config is attempted to be set more than once
        query!(
            r#"
            INSERT INTO blobs (id, data) VALUES ('gc', $1)
            "#,
            &genesis_bytes[..]
        )
        .execute(&mut dbtx)
        .await?;

        // Delegations require knowing the rates for the next epoch, so
        // pre-populate with 0 reward => exchange rate 1 for the current
        // (index 0) and next (index 1) epochs.
        for epoch in [0, 1] {
            query!(
                "INSERT INTO base_rates (
                epoch,
                base_reward_rate,
                base_exchange_rate
            ) VALUES ($1, $2, $3)",
                epoch,
                0,
                1_0000_0000
            )
            .execute(&mut dbtx)
            .await?;
        }

        let mut next_rate_data = RateDataById::default();
        for genesis::ValidatorPower { validator, power } in &genesis_config.validators {
            query!(
                "INSERT INTO validators (
                    identity_key,
                    consensus_key,
                    sequence_number,
                    name,
                    website,
                    description,
                    voting_power,
                    validator_state,
                    unbonding_epoch
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                validator.identity_key.encode_to_vec(),
                validator.consensus_key.to_bytes(),
                validator.sequence_number as i64,
                validator.name,
                validator.website,
                validator.description,
                power.value() as i64,
                // TODO: use real ValidatorState here (ok for now because all validators
                // in genesis start in ACTIVE state)
                ValidatorStateName::Active.to_str().to_string(),
                Option::<i64>::None,
            )
            .execute(&mut dbtx)
            .await?;

            for FundingStream { address, rate_bps } in validator.funding_streams.as_ref() {
                query!(
                    "INSERT INTO validator_fundingstreams (
                        identity_key,
                        address,
                        rate_bps
                    ) VALUES ($1, $2, $3)",
                    validator.identity_key.encode_to_vec(),
                    address.to_string(),
                    *rate_bps as i32,
                )
                .execute(&mut dbtx)
                .await?;
            }

            // The initial voting power is set from the genesis configuration,
            // but later, it's recomputed based on the size of each validator's
            // delegation pool.  Delegations require knowing the rates for the
            // next epoch, so pre-populate with 0 reward => exchange rate 1 for
            // the current (index 0) and next (index 1) epochs.
            for epoch in [0, 1] {
                query!(
                    "INSERT INTO validator_rates (
                    identity_key,
                    epoch,
                    validator_reward_rate,
                    validator_exchange_rate
                ) VALUES ($1, $2, $3, $4)",
                    validator.identity_key.encode_to_vec(),
                    epoch,
                    0,
                    1_0000_0000i64, // 1 represented as 1e8
                )
                .execute(&mut dbtx)
                .await?;
            }

            next_rate_data.insert(
                validator.identity_key.clone(),
                penumbra_stake::RateData {
                    identity_key: validator.identity_key.clone(),
                    epoch_index: 1,
                    validator_reward_rate: 0,
                    validator_exchange_rate: 1_0000_0000,
                },
            );
        }

        let chain_params = genesis_config.chain_params.clone();
        // Finally, commit the transaction and then update subscribers
        dbtx.commit().await?;
        // Sends fail if every receiver has been dropped, which is not our problem.
        // We wrote these, so push updates to subscribers.
        let _ = self.chain_params_tx.send(chain_params);
        let _ = self.next_rate_data_tx.send(next_rate_data);
        // These haven't been set yet.
        // let _ = self.height_tx.send(height);
        // let _ = self.valid_anchors_tx.send(valid_anchors);

        Ok(())
    }

    /// Commits a block to the state, returning the new app hash.
    pub async fn commit_block(&self, block: PendingBlock) -> Result<Vec<u8>> {
        // TODO: batch these queries?
        let mut dbtx = self.pool.begin().await?;

        let nct_anchor = block.note_commitment_tree.root2();
        let nct_bytes = bincode::serialize(&block.note_commitment_tree)?;
        query!(
            r#"
            INSERT INTO blobs (id, data) VALUES ('nct', $1)
            ON CONFLICT (id) DO UPDATE SET data = $1
            "#,
            &nct_bytes[..]
        )
        .execute(&mut dbtx)
        .await?;

        let height = block.height.expect("height must be set");

        // The Jellyfish Merkle tree batches writes to its backing store, so we
        // first need to write the JMT kv pairs...
        let (jmt_root, tree_update_batch) = jmt::JellyfishMerkleTree::new(&self.private_reader)
            .put_value_set(
                // TODO: create a JmtKey enum, where each variant has
                // a different domain-separated hash
                vec![(
                    jellyfish::Key::NoteCommitmentAnchor.hash(),
                    nct_anchor.clone(),
                )],
                height,
            )
            .await?;
        // ... and then write the resulting batch update to the backing store:
        jellyfish::DbTx(&mut dbtx)
            .write_node_batch(&tree_update_batch.node_batch)
            .await?;

        // The app hash is the root of the Jellyfish Merkle Tree.  We save the
        // NCT anchor separately for convenience, but it's already included in
        // the JMT root.
        // TODO: no way to access the Diem HashValue as array, even though it's stored that way?
        let app_hash: [u8; 32] = jmt_root.to_vec().try_into().unwrap();

        query!(
            "INSERT INTO blocks (height, nct_anchor, app_hash) VALUES ($1, $2, $3)",
            height as i64,
            &nct_anchor.to_bytes()[..],
            &app_hash[..]
        )
        .execute(&mut dbtx)
        .await?;

        // Add newly created notes into the chain state.
        for (note_commitment, positioned_note) in block.notes.into_iter() {
            query!(
                r#"
                INSERT INTO notes (
                    note_commitment,
                    ephemeral_key,
                    encrypted_note,
                    transaction_id,
                    position,
                    height
                ) VALUES ($1, $2, $3, $4, $5, $6)"#,
                &<[u8; 32]>::from(note_commitment)[..],
                &positioned_note.data.ephemeral_key.0[..],
                &positioned_note.data.encrypted_note[..],
                &positioned_note.data.transaction_id[..],
                positioned_note.position as i64,
                height as i64,
            )
            .execute(&mut dbtx)
            .await?;
        }

        // Mark spent notes as spent.
        for nullifier in block.spent_nullifiers.into_iter() {
            query!(
                "INSERT INTO nullifiers VALUES ($1, $2)",
                &<[u8; 32]>::from(nullifier)[..],
                height as i64,
            )
            .execute(&mut dbtx)
            .await?;
        }

        // Track the net change in delegations in this block.
        let epoch_index = block.epoch.unwrap().index;
        for (identity_key, delegation_change) in block.delegation_changes {
            query!(
                "INSERT INTO delegation_changes VALUES ($1, $2, $3)",
                identity_key.encode_to_vec(),
                epoch_index as i64,
                delegation_change
            )
            .execute(&mut dbtx)
            .await?;
        }

        // Save any new assets found in the block to the asset registry.
        for (id, asset) in block.supply_updates {
            query!(
                r#"INSERT INTO assets (asset_id, denom, total_supply) VALUES ($1, $2, $3) ON CONFLICT (asset_id) DO UPDATE SET denom=$2, total_supply=$3"#,
                &id.to_bytes()[..],
                asset.0.to_string(),
                asset.1 as i64
            )
            .execute(&mut dbtx)
            .await?;
        }

        if let (Some(base_rate_data), Some(rate_data)) =
            (block.next_base_rate, block.next_rates.as_ref())
        {
            query!(
                "INSERT INTO base_rates VALUES ($1, $2, $3)",
                base_rate_data.epoch_index as i64,
                base_rate_data.base_reward_rate as i64,
                base_rate_data.base_exchange_rate as i64,
            )
            .execute(&mut dbtx)
            .await?;

            for rate in rate_data {
                query!(
                    "INSERT INTO validator_rates VALUES ($1, $2, $3, $4)",
                    rate.identity_key.encode_to_vec(),
                    rate.epoch_index as i64,
                    rate.validator_reward_rate as i64,
                    rate.validator_exchange_rate as i64,
                )
                .execute(&mut dbtx)
                .await?;
            }
        }

        if let Some(validator_statuses) = block.next_validator_statuses {
            for status in validator_statuses {
                query!(
                    "UPDATE validators SET voting_power=$1 WHERE identity_key = $2",
                    status.voting_power as i64,
                    status.identity_key.encode_to_vec(),
                )
                .execute(&mut dbtx)
                .await?;
            }
        }

        let mut valid_anchors = self.valid_anchors_tx.borrow().clone();
        if valid_anchors.len() >= NUM_RECENT_ANCHORS {
            valid_anchors.pop_back();
        }
        valid_anchors.push_front(nct_anchor);
        let next_rate_data = block.next_rates.map(|next_rates| {
            next_rates
                .into_iter()
                .map(|rd| (rd.identity_key.clone(), rd))
                .collect::<RateDataById>()
        });

        // Finally, commit the transaction and then update subscribers
        dbtx.commit().await?;
        // Errors in sends arise only if no one is listening -- not our problem.
        let _ = self.height_tx.send(height.try_into().unwrap());
        let _ = self.valid_anchors_tx.send(valid_anchors);
        if let Some(next_rate_data) = next_rate_data {
            let _ = self.next_rate_data_tx.send(next_rate_data);
        }
        // chain_params_tx is a no-op, currently chain params don't change

        Ok(app_hash.to_vec())
    }
}
