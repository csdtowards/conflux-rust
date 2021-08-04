// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pos::consensus::{
    block_storage::BlockReader, state_replication::TxnManager,
    util::time_service::TimeService,
};
use anyhow::{bail, ensure, format_err, Context};
use consensus_types::{
    block::Block,
    block_data::BlockData,
    common::{Author, Round},
    quorum_cert::QuorumCert,
};

use diem_crypto::PrivateKey;
use diem_infallible::Mutex;
use diem_logger::{debug as diem_debug, error as diem_error};
use diem_types::{
    transaction::{RawTransaction, TransactionPayload},
    validator_config::{
        ConsensusPrivateKey, ConsensusPublicKey, ConsensusVRFPrivateKey,
        ConsensusVRFPublicKey,
    },
    validator_verifier::ValidatorVerifier,
};
use pow_types::PowInterface;
use std::sync::Arc;

#[cfg(test)]
#[path = "proposal_generator_test.rs"]
mod proposal_generator_test;

/// ProposalGenerator is responsible for generating the proposed block on
/// demand: it's typically used by a validator that believes it's a valid
/// candidate for serving as a proposer at a given round.
/// ProposalGenerator is the one choosing the branch to extend:
/// - round is given by the caller (typically determined by RoundState).
/// The transactions for the proposed block are delivered by TxnManager.
///
/// TxnManager should be aware of the pending transactions in the branch that it
/// is extending, such that it will filter them out to avoid transaction
/// duplication.
pub struct ProposalGenerator {
    // The account address of this validator
    author: Author,
    // Block store is queried both for finding the branch to extend and for
    // generating the proposed block.
    block_store: Arc<dyn BlockReader + Send + Sync>,
    // Transaction manager is delivering the transactions.
    txn_manager: Arc<dyn TxnManager>,
    // Time service to generate block timestamps
    time_service: Arc<dyn TimeService>,
    // Max number of transactions to be added to a proposed block.
    max_block_size: u64,
    // Last round that a proposal was generated
    last_round_generated: Mutex<Round>,
    // Handle the interaction with PoW consensus.
    pow_handler: Arc<dyn PowInterface>,
    // FIXME(lpl): Where to put them?
    pub private_key: ConsensusPrivateKey,
    pub public_key: ConsensusPublicKey,
    pub vrf_private_key: ConsensusVRFPrivateKey,
    pub vrf_public_key: ConsensusVRFPublicKey,
}

impl ProposalGenerator {
    pub fn new(
        author: Author, block_store: Arc<dyn BlockReader + Send + Sync>,
        txn_manager: Arc<dyn TxnManager>, time_service: Arc<dyn TimeService>,
        max_block_size: u64, pow_handler: Arc<dyn PowInterface>,
        private_key: ConsensusPrivateKey, public_key: ConsensusPublicKey,
        vrf_private_key: ConsensusVRFPrivateKey,
        vrf_public_key: ConsensusVRFPublicKey,
    ) -> Self
    {
        Self {
            author,
            block_store,
            txn_manager,
            time_service,
            max_block_size,
            last_round_generated: Mutex::new(0),
            pow_handler,
            private_key,
            public_key,
            vrf_private_key,
            vrf_public_key,
        }
    }

    pub fn author(&self) -> Author { self.author }

    /// Creates a NIL block proposal extending the highest certified block from
    /// the block store.
    pub fn generate_nil_block(&self, round: Round) -> anyhow::Result<Block> {
        let hqc = self.ensure_highest_quorum_cert(round)?;
        Ok(Block::new_nil(round, hqc.as_ref().clone()))
    }

    /// The function generates a new proposal block: the returned future is
    /// fulfilled when the payload is delivered by the TxnManager
    /// implementation.  At most one proposal can be generated per round (no
    /// proposal equivocation allowed). Errors returned by the TxnManager
    /// implementation are propagated to the caller. The logic for choosing
    /// the branch to extend is as follows: 1. The function gets the highest
    /// head of a one-chain from block tree. The new proposal must extend
    /// hqc to ensure optimistic responsiveness. 2. The round is provided by
    /// the caller. 3. In case a given round is not greater than the
    /// calculated parent, return an OldRound error.
    pub async fn generate_proposal(
        &mut self, round: Round, validators: ValidatorVerifier,
    ) -> anyhow::Result<BlockData> {
        {
            let mut last_round_generated = self.last_round_generated.lock();
            if *last_round_generated < round {
                *last_round_generated = round;
            } else {
                bail!("Already proposed in the round {}", round);
            }
        }

        let hqc = self.ensure_highest_quorum_cert(round)?;

        // TODO(lpl): Handle reconfiguraiton.
        let (payload, timestamp) = if hqc
            .certified_block()
            .has_reconfiguration()
        {
            // Reconfiguration rule - we propose empty blocks with parents'
            // timestamp after reconfiguration until it's committed
            (vec![], hqc.certified_block().timestamp_usecs())
        } else {
            // One needs to hold the blocks with the references to the payloads
            // while get_block is being executed: pending blocks
            // vector keeps all the pending ancestors of the extended branch.
            let mut pending_blocks = self
                .block_store
                .path_from_root(hqc.certified_block().id())
                .ok_or_else(|| {
                    format_err!(
                        "HQC {} already pruned",
                        hqc.certified_block().id()
                    )
                })?;
            // Avoid txn manager long poll it the root block has txns, so that
            // the leader can deliver the commit proof to others
            // without delay.
            pending_blocks.push(self.block_store.root());

            // Exclude all the pending transactions: these are all the ancestors
            // of parent (including) up to the root (including).
            let exclude_payload: Vec<&Vec<_>> = pending_blocks
                .iter()
                .flat_map(|block| block.payload())
                .collect();

            // All proposed blocks in a branch are guaranteed to have increasing
            // timestamps since their predecessor block will not be
            // added to the BlockStore until the local time exceeds
            // it.
            let timestamp = self.time_service.get_current_timestamp();

            let parent_block = if let Some(p) = pending_blocks.last() {
                p.clone()
            } else {
                self.block_store.root()
            };

            let mut payload = self
                .txn_manager
                .pull_txns(
                    self.max_block_size,
                    exclude_payload,
                    parent_block.id(),
                    validators,
                )
                .await
                .context("Fail to retrieve txn")?;
            diem_debug!(
                "generate_proposal: Pull {} transactions",
                payload.len()
            );

            // FIXME(lpl): For now, sending default H256 will return the first
            // pivot decision.
            let parent_decision = parent_block
                .block_info()
                .pivot_decision()
                .map(|d| d.block_hash)
                .unwrap_or_default();
            let new_pivot_decision =
                payload.iter().find_map(|tx| match tx.payload() {
                    TransactionPayload::PivotDecision(decision) => {
                        if decision.height
                            <= parent_block
                                .block_info()
                                .pivot_decision()
                                .map(|d| d.height)
                                .unwrap_or_default()
                        {
                            None
                        } else {
                            Some(decision.block_hash)
                        }
                    }
                    _ => None,
                });
            if new_pivot_decision.is_none()
                && payload.last().is_some()
                && matches!(
                    payload.last().unwrap().payload(),
                    TransactionPayload::PivotDecision(_)
                )
            {
                payload.pop();
            }
            /*
            match self.pow_handler.next_pivot_decision(parent_decision).await {

                Some((height, block_hash)) => {
                    let pivot_decision =
                        PivotBlockDecision { height, block_hash };
                    let raw_tx = RawTransaction::new_pivot_decision(
                        self.author,
                        0,
                        pivot_decision,
                        ChainId::default(),
                    );
                    let signed_tx = raw_tx
                        .sign(&self.private_key, self.public_key.clone())?
                        .into_inner();
                    payload.push(signed_tx);
             */
            match new_pivot_decision {
                Some(block_hash) => {
                    // Included new registered or updated nodes as transactions.
                    let staking_events = self
                        .pow_handler
                        .get_staking_events(parent_decision, block_hash)?;
                    diem_debug!("generate_proposal: staking_events={:?}", staking_events);
                    for event in staking_events {
                        match RawTransaction::from_staking_event(
                            &event,
                            self.author,
                        ) {
                            Ok(raw_tx) => {
                                let signed_tx = raw_tx
                                    .sign(
                                        &self.private_key,
                                        self.public_key.clone(),
                                    )?
                                    .into_inner();
                                payload.push(signed_tx);
                            }
                            // TODO(lpl): This is not supposed to happen, so
                            // should we return error here?
                            Err(e) => diem_error!(
                                "Get invalid staking event: err={:?}",
                                e
                            ),
                        }
                    }
                }
                None => {
                    warn!("pos progress without new pivot decision");
                }
            }

            (payload, timestamp.as_micros() as u64)
        };

        // create block proposal
        Ok(BlockData::new_proposal(
            payload,
            self.author,
            round,
            timestamp,
            hqc.as_ref().clone(),
        ))
    }

    fn ensure_highest_quorum_cert(
        &self, round: Round,
    ) -> anyhow::Result<Arc<QuorumCert>> {
        let hqc = self.block_store.highest_quorum_cert();
        ensure!(
            hqc.certified_block().round() < round,
            "Given round {} is lower than hqc round {}",
            round,
            hqc.certified_block().round()
        );
        ensure!(
            !hqc.ends_epoch(),
            "The epoch has already ended,a proposal is not allowed to generated"
        );

        Ok(hqc)
    }
}