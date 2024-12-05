use std::ops::{Deref, DerefMut};

use super::{block::BlockRecord, event::Event};

use cfx_types::H256;
use lazy_static::lazy_static;
use log::warn;
use lru_time_cache::LruCache;
use parking_lot::Mutex;

pub(crate) struct Recorder(LruCache<H256, BlockRecord>);

lazy_static! {
    pub(crate) static ref BLOCK_EVENT_RECORDER: Mutex<Recorder> =
        Mutex::new(Recorder(LruCache::with_capacity(2000)));
}

impl Deref for Recorder {
    type Target = LruCache<H256, BlockRecord>;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl DerefMut for Recorder {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

impl Recorder {
    pub(crate) fn get_instance_mut() -> impl DerefMut<Target = Recorder> {
        BLOCK_EVENT_RECORDER.lock()
    }

    pub fn record_event(&mut self, hash: &H256, event: Event) {
        let block_record = if let Some(r) = self.0.get_mut(hash) {
            r
        } else {
            warn!(
                "Block events record error: unknown hash {:?} on event {:?}",
                hash, event
            );
            return;
        };

        let record_completed = block_record.record_event(event);

        if record_completed {
            block_record.log(hash);
            self.0.remove(&hash);
        }
    }

    pub fn insert_new_block(&mut self, hash: &H256, record: BlockRecord) {
        if self.0.contains_key(hash) {
            return;
        }
        let (_, eliminated_items) = self.0.notify_insert(*hash, record);

        Self::process_eliminated_items(eliminated_items)
    }

    pub fn clear(&mut self) { self.0.clear() }

    fn process_eliminated_items(
        items: impl IntoIterator<Item = (H256, BlockRecord)>,
    ) {
        for (hash, record) in items {
            record.log(&hash)
        }
    }
}
