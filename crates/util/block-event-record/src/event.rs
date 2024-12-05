#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum Event {
    HeaderReady,
    BodyReady,
    SyncGraph,
    ConGraph,
    ConGraphDone,
    ComputeEpoch,
    NotifyTxPool,
    TxPoolUpdated,
}

impl Event {
    pub fn key(&self) -> &'static str {
        match self {
            Event::HeaderReady => "header_ready",
            Event::BodyReady => "body_ready",
            Event::SyncGraph => "sync_graph",
            Event::ConGraph => "consensys_graph_insert",
            Event::ConGraphDone => "consensys_graph_ready",
            Event::ComputeEpoch => "compute_epoch",
            Event::NotifyTxPool => "notify_tx_pool",
            Event::TxPoolUpdated => "tx_pool_updated",
        }
    }
}
