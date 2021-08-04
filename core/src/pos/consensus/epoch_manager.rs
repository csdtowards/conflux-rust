// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use super::{
    block_storage::BlockStore,
    counters,
    error::{error_kind, DbError},
    liveness::{
        leader_reputation::{
            ActiveInactiveHeuristic, DiemDBBackend, LeaderReputation,
        },
        proposal_generator::ProposalGenerator,
        proposer_election::ProposerElection,
        rotating_proposer_election::{choose_leader, RotatingProposer},
        round_proposer_election::RoundProposer,
        round_state::{
            ExponentialTimeInterval, RoundState, RoundStateLogSchema,
        },
    },
    logging::{LogEvent, LogSchema},
    metrics_safety_rules::MetricsSafetyRules,
    network::{
        ConsensusMsg, ConsensusNetworkSender, IncomingBlockRetrievalRequest,
        NetworkReceivers,
    },
    persistent_liveness_storage::{
        LedgerRecoveryData, PersistentLivenessStorage, RecoveryData,
    },
    round_manager::{
        RecoveryManager, RoundManager, UnverifiedEvent, VerifiedEvent,
    },
    state_replication::{StateComputer, TxnManager},
    util::time_service::TimeService,
};
use crate::pos::{
    consensus::liveness::vrf_proposer_election::VrfProposer,
    mempool::SubmissionStatus, protocol::network_sender::NetworkSender,
};
use anyhow::{bail, ensure, Context};
use channel::diem_channel;
use consensus_types::{
    common::{Author, Round},
    epoch_retrieval::EpochRetrievalRequest,
};
use diem_config::config::{ConsensusConfig, ConsensusProposerType, NodeConfig};
use diem_infallible::duration_since_epoch;
use diem_logger::prelude::*;
use diem_metrics::monitor;
use diem_types::{
    account_address::AccountAddress,
    epoch_change::EpochChangeProof,
    epoch_state::EpochState,
    on_chain_config::{OnChainConfigPayload, ValidatorSet},
    transaction::SignedTransaction,
};
use futures::{
    channel::{mpsc, oneshot},
    select, StreamExt,
};
use pow_types::PowInterface;
use safety_rules::SafetyRulesManager;
use std::{
    cmp::Ordering,
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
    time::Duration,
};

/// RecoveryManager is used to process events in order to sync up with peer if
/// we can't recover from local consensusdb RoundManager is used for normal
/// event handling. We suppress clippy warning here because we expect most of
/// the time we will have RoundManager
#[allow(clippy::large_enum_variant)]
pub enum RoundProcessor {
    Recovery(RecoveryManager),
    Normal(RoundManager),
}

#[allow(clippy::large_enum_variant)]
pub enum LivenessStorageData {
    RecoveryData(RecoveryData),
    LedgerRecoveryData(LedgerRecoveryData),
}

impl LivenessStorageData {
    pub fn expect_recovery_data(self, msg: &str) -> RecoveryData {
        match self {
            LivenessStorageData::RecoveryData(data) => data,
            LivenessStorageData::LedgerRecoveryData(_) => panic!("{}", msg),
        }
    }
}

// Manager the components that shared across epoch and spawn per-epoch
// RoundManager with epoch-specific input.
pub struct EpochManager {
    author: Author,
    config: ConsensusConfig,
    time_service: Arc<dyn TimeService>,
    //self_sender: channel::Sender<Event<ConsensusMsg>>,
    network_sender: NetworkSender,
    timeout_sender: channel::Sender<Round>,
    proposal_timeout_sender: channel::Sender<Round>,
    new_round_timeout_sender: channel::Sender<Round>,
    txn_manager: Arc<dyn TxnManager>,
    state_computer: Arc<dyn StateComputer>,
    storage: Arc<dyn PersistentLivenessStorage>,
    safety_rules_manager: SafetyRulesManager,
    processor: Option<RoundProcessor>,
    reconfig_events: diem_channel::Receiver<(), OnChainConfigPayload>,
    // Conflux PoW handler
    pow_handler: Arc<dyn PowInterface>,
    tx_sender: mpsc::Sender<(
        SignedTransaction,
        oneshot::Sender<anyhow::Result<SubmissionStatus>>,
    )>,
}

impl EpochManager {
    pub fn new(
        node_config: &NodeConfig,
        time_service: Arc<dyn TimeService>,
        //self_sender: channel::Sender<Event<ConsensusMsg>>,
        network_sender: NetworkSender,
        timeout_sender: channel::Sender<Round>,
        proposal_timeout_sender: channel::Sender<Round>,
        new_round_timeout_sender: channel::Sender<Round>,
        txn_manager: Arc<dyn TxnManager>,
        state_computer: Arc<dyn StateComputer>,
        storage: Arc<dyn PersistentLivenessStorage>,
        reconfig_events: diem_channel::Receiver<(), OnChainConfigPayload>,
        pow_handler: Arc<dyn PowInterface>,
        author: AccountAddress,
        tx_sender: mpsc::Sender<(
            SignedTransaction,
            oneshot::Sender<anyhow::Result<SubmissionStatus>>,
        )>,
    ) -> Self
    {
        let config = node_config.consensus.clone();
        let sr_config = &node_config.consensus.safety_rules;
        let safety_rules_manager = SafetyRulesManager::new(sr_config);
        diem_debug!("EpochManager.author={:?}", author);
        Self {
            author,
            config,
            time_service,
            //self_sender,
            network_sender,
            timeout_sender,
            proposal_timeout_sender,
            new_round_timeout_sender,
            txn_manager,
            state_computer,
            storage,
            safety_rules_manager,
            processor: None,
            reconfig_events,
            pow_handler,
            tx_sender,
        }
    }

    fn epoch_state(&self) -> &EpochState {
        match self
            .processor
            .as_ref()
            .expect("EpochManager not started yet")
        {
            RoundProcessor::Normal(p) => p.epoch_state(),
            RoundProcessor::Recovery(p) => p.epoch_state(),
        }
    }

    fn epoch(&self) -> u64 { self.epoch_state().epoch }

    fn create_round_state(
        &self, time_service: Arc<dyn TimeService>,
        timeout_sender: channel::Sender<Round>,
        proposal_timeout_sender: channel::Sender<Round>,
        new_round_timeout_sender: channel::Sender<Round>,
    ) -> RoundState
    {
        // 1.5^6 ~= 11
        // Timeout goes from initial_timeout to initial_timeout*11 in 6 steps
        let time_interval = Box::new(ExponentialTimeInterval::new(
            Duration::from_millis(self.config.round_initial_timeout_ms),
            1.2,
            6,
        ));
        RoundState::new(
            time_interval,
            time_service,
            timeout_sender,
            proposal_timeout_sender,
            new_round_timeout_sender,
        )
    }

    /// Create a proposer election handler based on proposers
    fn create_proposer_election(
        &self, epoch_state: &EpochState,
    ) -> Box<dyn ProposerElection + Send + Sync> {
        let proposers = epoch_state
            .verifier
            .get_ordered_account_addresses_iter()
            .collect::<Vec<_>>();
        match &self.config.proposer_type {
            ConsensusProposerType::RotatingProposer => Box::new(
                RotatingProposer::new(proposers, self.config.contiguous_rounds),
            ),
            // We don't really have a fixed proposer!
            ConsensusProposerType::FixedProposer => {
                let proposer = choose_leader(proposers);
                Box::new(RotatingProposer::new(
                    vec![proposer],
                    self.config.contiguous_rounds,
                ))
            }
            ConsensusProposerType::LeaderReputation(heuristic_config) => {
                let backend = Box::new(DiemDBBackend::new(
                    proposers.len(),
                    self.storage.diem_db(),
                ));
                let heuristic = Box::new(ActiveInactiveHeuristic::new(
                    self.author,
                    heuristic_config.active_weights,
                    heuristic_config.inactive_weights,
                ));
                Box::new(LeaderReputation::new(proposers, backend, heuristic))
            }
            ConsensusProposerType::RoundProposer(round_proposers) => {
                // Hardcoded to the first proposer
                let default_proposer = proposers.get(0).unwrap();
                Box::new(RoundProposer::new(
                    round_proposers.clone(),
                    *default_proposer,
                ))
            }
            ConsensusProposerType::VrfProposer => Box::new(VrfProposer::new(
                self.author,
                self.config
                    .safety_rules
                    .vrf_private_key
                    .as_ref()
                    .expect(
                        "VRF private key mush be set for VRF leader election",
                    )
                    .private_key(),
                self.config.safety_rules.vrf_proposal_threshold,
                epoch_state.clone(),
            )),
        }
    }

    async fn process_epoch_retrieval(
        &mut self, request: EpochRetrievalRequest, peer_id: AccountAddress,
    ) -> anyhow::Result<()> {
        diem_debug!(
            LogSchema::new(LogEvent::ReceiveEpochRetrieval)
                .remote_peer(peer_id)
                .epoch(self.epoch()),
            "[EpochManager] receive {}",
            request,
        );
        let proof = self
            .storage
            .diem_db()
            .get_epoch_ending_ledger_infos(
                request.start_epoch,
                request.end_epoch,
            )
            .map_err(DbError::from)
            .context("[EpochManager] Failed to get epoch proof")?;
        let msg = ConsensusMsg::EpochChangeProof(Box::new(proof));
        self.network_sender.send_to(peer_id, &msg).context(format!(
            "[EpochManager] Failed to send epoch proof to {}",
            peer_id
        ))
    }

    async fn process_different_epoch(
        &mut self, different_epoch: u64, peer_id: AccountAddress,
    ) -> anyhow::Result<()> {
        diem_debug!(
            LogSchema::new(LogEvent::ReceiveMessageFromDifferentEpoch)
                .remote_peer(peer_id)
                .epoch(self.epoch()),
            remote_epoch = different_epoch,
        );
        match different_epoch.cmp(&self.epoch()) {
            // We try to help nodes that have lower epoch than us
            Ordering::Less => {
                self.process_epoch_retrieval(
                    EpochRetrievalRequest {
                        start_epoch: different_epoch,
                        end_epoch: self.epoch(),
                    },
                    peer_id,
                )
                .await
            }
            // We request proof to join higher epoch
            Ordering::Greater => {
                let request = EpochRetrievalRequest {
                    start_epoch: self.epoch(),
                    end_epoch: different_epoch,
                };
                let msg =
                    ConsensusMsg::EpochRetrievalRequest(Box::new(request));
                self.network_sender.send_to(peer_id, &msg).context(format!(
                    "[EpochManager] Failed to send epoch retrieval to {}",
                    peer_id
                ))
            }
            Ordering::Equal => {
                bail!("[EpochManager] Same epoch should not come to process_different_epoch");
            }
        }
    }

    async fn start_new_epoch(
        &mut self, proof: EpochChangeProof, peer_id: AccountAddress,
    ) -> anyhow::Result<()> {
        let ledger_info = proof
            .verify(self.epoch_state())
            .context("[EpochManager] Invalid EpochChangeProof")?;
        diem_debug!(
            LogSchema::new(LogEvent::NewEpoch)
                .epoch(ledger_info.ledger_info().next_block_epoch()),
            "Received verified epoch change",
        );

        // make sure storage is on this ledger_info too, it should be no-op if
        // it's already committed
        // self.state_computer
        //     .sync_to(ledger_info.clone())
        //     .await
        //     .context(format!(
        //         "[EpochManager] State sync to new epoch {}",
        //         ledger_info
        //     ))?;
        for ledger_info in proof.get_all_ledger_infos() {
            let mut new_epoch = false;
            match self.processor_mut() {
                RoundProcessor::Recovery(_) => {
                    bail!("start_new_epoch for Recovery processor");
                }
                RoundProcessor::Normal(p) => {
                    if ledger_info.ledger_info().epoch()
                        == p.epoch_state().epoch
                    {
                        p.sync_to_ledger_info(&ledger_info, peer_id).await?;
                        new_epoch = true;
                    } else {
                        diem_error!(
                            "Unexpected epoch change: me={} get={}",
                            p.epoch_state().epoch,
                            ledger_info.ledger_info().epoch()
                        );
                    }
                }
            }
            if new_epoch {
                monitor!("reconfig", self.expect_new_epoch().await);
            }
        }

        Ok(())
    }

    async fn start_round_manager(
        &mut self, recovery_data: RecoveryData, epoch_state: EpochState,
    ) {
        // Release the previous RoundManager, especially the SafetyRule client
        self.processor = None;
        let epoch = epoch_state.epoch;
        counters::EPOCH.set(epoch_state.epoch as i64);
        counters::CURRENT_EPOCH_VALIDATORS
            .set(epoch_state.verifier.len() as i64);
        diem_info!(
            epoch = epoch_state.epoch,
            validators = epoch_state.verifier.to_string(),
            root_block = recovery_data.root_block(),
            "Starting new epoch",
        );
        let last_vote = recovery_data.last_vote();

        diem_info!(epoch = epoch, "Create BlockStore");
        let block_store = Arc::new(BlockStore::new(
            Arc::clone(&self.storage),
            recovery_data,
            Arc::clone(&self.state_computer),
            self.config.max_pruned_blocks_in_mem,
            Arc::clone(&self.time_service),
            self.pow_handler.clone(),
        ));

        diem_info!(epoch = epoch, "Update SafetyRules");

        let mut safety_rules = MetricsSafetyRules::new(
            self.safety_rules_manager.client(),
            self.storage.clone(),
        );
        if let Err(error) = safety_rules.perform_initialize() {
            diem_error!(
                epoch = epoch,
                error = error,
                "Unable to initialize safety rules.",
            );
        }

        diem_info!(epoch = epoch, "Create ProposalGenerator");
        // TODO(lpl): Decide key management.
        // txn manager is required both by proposal generator (to pull the
        // proposers) and by event processor (to update their status).
        let proposal_generator = match epoch_state
            .verifier
            .get_public_key(&self.author)
        {
            Some(public_key) => {
                // TODO(lpl): Handle no vrf.
                let vrf_key =
                    self.config.safety_rules.vrf_private_key.as_ref().unwrap();
                let private_key = self
                    .config
                    .safety_rules
                    .test
                    .as_ref()
                    .expect("test config set")
                    .consensus_key
                    .as_ref()
                    .expect("private key set in pos")
                    .private_key();
                Some(ProposalGenerator::new(
                    self.author,
                    block_store.clone(),
                    self.txn_manager.clone(),
                    self.time_service.clone(),
                    self.config.max_block_size,
                    self.pow_handler.clone(),
                    private_key,
                    public_key,
                    vrf_key.private_key(),
                    vrf_key.public_key(),
                ))
            }
            None => None,
        };

        diem_info!(epoch = epoch, "Create RoundState");
        let round_state = self.create_round_state(
            self.time_service.clone(),
            self.timeout_sender.clone(),
            self.proposal_timeout_sender.clone(),
            self.new_round_timeout_sender.clone(),
        );

        diem_info!(epoch = epoch, "Create ProposerElection");
        let proposer_election = self.create_proposer_election(&epoch_state);
        let network_sender = ConsensusNetworkSender::new(
            self.author,
            self.network_sender.clone(),
            //self.self_sender.clone(),
            epoch_state.verifier.clone(),
        );

        let mut processor = RoundManager::new(
            epoch_state,
            block_store,
            round_state,
            proposer_election,
            proposal_generator,
            safety_rules,
            network_sender,
            self.txn_manager.clone(),
            self.storage.clone(),
            self.config.sync_only,
            self.tx_sender.clone(),
        );
        // Only check if we should send election after entering an new epoch.
        if let Err(e) = processor
            .broadcast_election(
                self.author,
                self.config
                    .safety_rules
                    .test
                    .as_ref()
                    .expect("test config set")
                    .consensus_key
                    .as_ref()
                    .expect("private key set in pos"),
                self.config.safety_rules.vrf_private_key.as_ref().unwrap(),
            )
            .await
        {
            diem_error!("error in broadcasting election tx: {:?}", e);
        }
        processor.start(last_vote).await;
        self.processor = Some(RoundProcessor::Normal(processor));
        diem_info!(epoch = epoch, "RoundManager started");
    }

    // Depending on what data we can extract from consensusdb, we may or may not
    // have an event processor at startup. If we need to sync up with peers
    // for blocks to construct a valid block store, which is required to
    // construct an event processor, we will take care of the sync up here.
    async fn start_recovery_manager(
        &mut self, ledger_recovery_data: LedgerRecoveryData,
        epoch_state: EpochState,
    )
    {
        let epoch = epoch_state.epoch;
        let network_sender = ConsensusNetworkSender::new(
            self.author,
            self.network_sender.clone(),
            //self.self_sender.clone(),
            epoch_state.verifier.clone(),
        );
        self.processor = Some(RoundProcessor::Recovery(RecoveryManager::new(
            epoch_state,
            network_sender,
            self.storage.clone(),
            self.state_computer.clone(),
            ledger_recovery_data.commit_round(),
        )));
        diem_info!(epoch = epoch, "SyncProcessor started");
    }

    async fn start_processor(&mut self, payload: OnChainConfigPayload) {
        let epoch_state: EpochState = payload.get().unwrap_or_else(|_| {
            let validator_set: ValidatorSet = payload.get().unwrap();
            EpochState {
                epoch: payload.epoch(),
                verifier: (&validator_set).into(),
                // genesis pivot decision
                vrf_seed: self
                    .storage
                    .diem_db()
                    .get_latest_ledger_info()
                    .expect("non-empty ledger info")
                    .ledger_info()
                    .pivot_decision()
                    .unwrap()
                    .block_hash
                    .as_bytes()
                    .to_vec(),
            }
        });

        match self.storage.start() {
            LivenessStorageData::RecoveryData(initial_data) => {
                self.start_round_manager(initial_data, epoch_state).await
            }
            LivenessStorageData::LedgerRecoveryData(ledger_recovery_data) => {
                self.start_recovery_manager(ledger_recovery_data, epoch_state)
                    .await
            }
        }
    }

    async fn process_message(
        &mut self, peer_id: AccountAddress, consensus_msg: ConsensusMsg,
    ) -> anyhow::Result<()> {
        // we can't verify signatures from a different epoch
        let maybe_unverified_event =
            self.process_epoch(peer_id, consensus_msg).await?;

        if let Some(unverified_event) = maybe_unverified_event {
            // same epoch -> run well-formedness + signature check
            let verified_event = unverified_event
                .clone()
                .verify(
                    &self.epoch_state().verifier,
                    self.epoch_state().vrf_seed.as_slice(),
                )
                .context("[EpochManager] Verify event")
                .map_err(|err| {
                    diem_error!(
                        SecurityEvent::ConsensusInvalidMessage,
                        remote_peer = peer_id,
                        error = ?err,
                        unverified_event = unverified_event
                    );
                    err
                })?;

            // process the verified event
            self.process_event(peer_id, verified_event).await?;
        }
        Ok(())
    }

    async fn process_epoch(
        &mut self, peer_id: AccountAddress, msg: ConsensusMsg,
    ) -> anyhow::Result<Option<UnverifiedEvent>> {
        match msg {
            ConsensusMsg::ProposalMsg(_)
            | ConsensusMsg::SyncInfo(_)
            | ConsensusMsg::VoteMsg(_) => {
                let event: UnverifiedEvent = msg.into();
                if event.epoch() == self.epoch() {
                    return Ok(Some(event));
                } else {
                    monitor!(
                        "process_different_epoch_consensus_msg",
                        self.process_different_epoch(event.epoch(), peer_id)
                            .await?
                    );
                }
            }
            ConsensusMsg::EpochChangeProof(proof) => {
                let msg_epoch = proof.epoch()?;
                diem_debug!(
                    LogSchema::new(LogEvent::ReceiveEpochChangeProof)
                        .remote_peer(peer_id)
                        .epoch(self.epoch()),
                    "Proof from epoch {}",
                    msg_epoch,
                );
                if msg_epoch == self.epoch() {
                    monitor!(
                        "process_epoch_proof",
                        self.start_new_epoch(*proof, peer_id).await?
                    );
                } else {
                    bail!(
                        "[EpochManager] Unexpected epoch proof from epoch {}, local epoch {}",
                        msg_epoch,
                        self.epoch()
                    );
                }
            }
            ConsensusMsg::EpochRetrievalRequest(request) => {
                ensure!(
                    request.end_epoch <= self.epoch(),
                    "[EpochManager] Received EpochRetrievalRequest beyond what we have locally"
                );
                monitor!(
                    "process_epoch_retrieval",
                    self.process_epoch_retrieval(*request, peer_id).await?
                );
            }
            _ => {
                bail!("[EpochManager] Unexpected messages: {:?}", msg);
            }
        }
        Ok(None)
    }

    async fn process_event(
        &mut self, peer_id: AccountAddress, event: VerifiedEvent,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Recovery(p) => {
                let recovery_data = match event {
                    VerifiedEvent::ProposalMsg(proposal) => {
                        p.process_proposal_msg(*proposal).await
                    }
                    VerifiedEvent::VoteMsg(vote) => {
                        p.process_vote_msg(*vote).await
                    }
                    VerifiedEvent::SyncInfo(sync_info) => {
                        p.sync_up(&sync_info, peer_id).await
                    }
                }?;
                let epoch_state = p.epoch_state().clone();
                diem_info!("Recovered from SyncProcessor");
                self.start_round_manager(recovery_data, epoch_state).await;
                Ok(())
            }
            RoundProcessor::Normal(p) => match event {
                VerifiedEvent::ProposalMsg(proposal) => monitor!(
                    "process_proposal",
                    p.process_proposal_msg(*proposal).await
                ),
                VerifiedEvent::VoteMsg(vote) => {
                    monitor!("process_vote", p.process_vote_msg(*vote).await)
                }
                VerifiedEvent::SyncInfo(sync_info) => monitor!(
                    "process_sync_info",
                    p.process_sync_info_msg(*sync_info, peer_id).await
                ),
            },
        }
    }

    fn processor_mut(&mut self) -> &mut RoundProcessor {
        self.processor
            .as_mut()
            .expect("[EpochManager] not started yet")
    }

    async fn process_block_retrieval(
        &mut self, request: IncomingBlockRetrievalRequest,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => {
                p.process_block_retrieval(request).await
            }
            _ => bail!("[EpochManager] RoundManager not started yet"),
        }
    }

    async fn process_local_timeout(
        &mut self, round: u64,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => p.process_local_timeout(round).await,
            _ => unreachable!("RoundManager not started yet"),
        }
    }

    async fn process_proposal_timeout(
        &mut self, round: u64,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => {
                p.process_proposal_timeout(round).await
            }
            _ => unreachable!("RoundManager not started yet"),
        }
    }

    async fn process_new_round_timeout(
        &mut self, round: u64,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => {
                p.process_new_round_timeout(round).await
            }
            _ => unreachable!("RoundManager not started yet"),
        }
    }

    async fn expect_new_epoch(&mut self) {
        diem_debug!("expect_new_epoch: start");
        if let Some(payload) = self.reconfig_events.next().await {
            diem_debug!("expect_new_epoch: receive event!");
            self.start_processor(payload).await;
            diem_debug!("expect_new_epoch: processor started!");
        } else {
            panic!("Reconfig sender dropped, unable to start new epoch.");
        }
    }

    pub async fn start(
        mut self, mut round_timeout_sender_rx: channel::Receiver<Round>,
        mut proposal_timeout_sender_rx: channel::Receiver<Round>,
        mut new_round_timeout_sender_rx: channel::Receiver<Round>,
        mut network_receivers: NetworkReceivers, stopped: Arc<AtomicBool>,
    )
    {
        // initial start of the processor
        self.expect_new_epoch().await;
        diem_debug!("EpochManager main_loop starts");
        loop {
            if stopped.load(AtomicOrdering::SeqCst) {
                break;
            }
            let result = monitor!(
                "main_loop",
                select! {
                    msg = network_receivers.consensus_messages.select_next_some() => {
                        let (peer, msg) = (msg.0, msg.1);
                        monitor!("process_message", self.process_message(peer, msg).await.with_context(|| format!("from peer: {}", peer)))
                    }
                    block_retrieval = network_receivers.block_retrieval.select_next_some() => {
                        monitor!("process_block_retrieval", self.process_block_retrieval(block_retrieval).await)
                    }
                    round = round_timeout_sender_rx.select_next_some() => {
                        monitor!("process_local_timeout", self.process_local_timeout(round).await)
                    }
                    round = proposal_timeout_sender_rx.select_next_some() => {
                        monitor!("process_proposal_timeout", self.process_proposal_timeout(round).await)
                    }
                    round = new_round_timeout_sender_rx.select_next_some() => {
                        monitor!("process_new_round_timeout", self.process_new_round_timeout(round).await)
                    }
                }
            );
            let round_state =
                if let RoundProcessor::Normal(p) = self.processor_mut() {
                    Some(p.round_state())
                } else {
                    None
                };
            match result {
                Ok(_) => diem_trace!(RoundStateLogSchema::new(round_state)),
                Err(e) => {
                    counters::ERROR_COUNT.inc();
                    diem_error!(error = ?e, kind = error_kind(&e), RoundStateLogSchema::new(round_state));
                }
            }

            // Continually capture the time of consensus process to ensure that
            // clock skew between validators is reasonable and to
            // find any unusual (possibly byzantine) clock behavior.
            counters::OP_COUNTERS
                .gauge("time_since_epoch_ms")
                .set(duration_since_epoch().as_millis() as i64);
        }
    }
}