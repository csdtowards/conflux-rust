use super::event::Event;

use cfx_types::H256;
use log::debug;
use std::{
    collections::BTreeMap,
    fmt::Debug,
    time::{Instant, SystemTime},
};

pub(crate) struct BlockRecord {
    hash_ready_instant: Instant,
    hash_ready_time: SystemTime,
    time_ticks: BTreeMap<Event, u64>,
}

impl BlockRecord {
    pub(crate) fn new_received_block() -> Self {
        let hash_ready_time = SystemTime::now();
        let hash_ready_instant = Instant::now();
        Self {
            hash_ready_time,
            hash_ready_instant,
            time_ticks: BTreeMap::new(),
        }
    }

    pub(crate) fn new_mined_block() -> Self {
        use Event::{BodyReady, HeaderReady};
        let mut record = Self::new_received_block();
        record.time_ticks.insert(HeaderReady, 0);
        record.time_ticks.insert(BodyReady, 0);
        record
    }

    pub(crate) fn record_event(&mut self, event: Event) -> bool {
        self.time_ticks.entry(event).or_insert_with(|| {
            self.hash_ready_instant.elapsed().as_nanos() as u64
        });

        self.is_complete()
    }

    pub(crate) fn log(&self, hash: &H256) {
        use chrono::{DateTime, Utc};
        let start_time: DateTime<Utc> = self.hash_ready_time.into();

        let mut result = vec![
            dict_item("hash", hash),
            dict_item("start_timestamp", start_time),
        ];
        result.extend(
            self.time_ticks
                .iter()
                .map(|(event, value)| dict_item(event.key(), value)),
        );

        let status = if self.is_complete() {
            "complete"
        } else if self.is_complete_for_non_pivot() {
            "partially complete"
        } else {
            "incomplete"
        };

        debug!("Block events record {status}. {}", result.join(", "));
    }

    fn is_complete(&self) -> bool { self.time_ticks.len() == 8 }

    fn is_complete_for_non_pivot(&self) -> bool {
        self.time_ticks.range(..Event::ConGraphDone).count() == 5
    }
}

fn dict_item(key: &'static str, value: impl Debug) -> String {
    format!("{key}: {value:?}")
}
