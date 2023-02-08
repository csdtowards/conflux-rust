// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

pub struct SnapshotMptDbSqlite {
    // Option because we need an empty snapshot db for empty snapshot.
    maybe_db_connections: Option<Box<[SqliteConnection]>>,
    already_open_snapshots: AlreadyOpenSnapshots<Self>,
    open_semaphore: Arc<Semaphore>,
    path: PathBuf,
    remove_on_close: AtomicBool,
}

pub struct SnapshotMptDbStatements {
    mpt_statements: Arc<KvdbSqliteStatements>,
}

lazy_static! {
    pub static ref SNAPSHOT_MPT_DB_STATEMENTS: SnapshotMptDbStatements = {
        let mpt_statements = Arc::new(
            KvdbSqliteStatements::make_statements(
                &["node_rlp"],
                &["BLOB"],
                SnapshotMptDbSqlite::SNAPSHOT_MPT_TABLE_NAME,
                false,
            )
            .unwrap(),
        );

        SnapshotMptDbStatements {

            mpt_statements,

        }
    };
}

impl Drop for SnapshotMptDbSqlite {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            self.maybe_db_connections.take();
            // SnapshotDbManagerSqlite::on_close(
            //     &self.already_open_snapshots,
            //     &self.open_semaphore,
            //     &self.path,
            //     self.remove_on_close.load(Ordering::Relaxed),
            // )
        }
    }
}

impl SnapshotMptDbSqlite {
    pub const DB_SHARDS: u16 = 32;
    /// These two tables are temporary table for the merging process, but they
    /// remain to help other nodes to do 1-step syncing.

    // FIXME: Archive node will have different db schema to support versioned
    // FIXME: read and to provide incremental syncing.
    // FIXME:
    // FIXME: for archive mode, the delete table may live in its own db file
    // FIXME: which contains delete table for a version range.
    // FIXME: model this fact and refactor.
    /*
    pub const KVV_PUT_STATEMENT: &'static str =
        "INSERT OR REPLACE INTO :table_name VALUES (:key, :value, :version)";
    /// Key is not unique, because the same key can appear with different
    /// version number.
    pub const SNAPSHOT_KV_DELETE_TABLE_NAME: &'static str =
        "snapshot_key_value_delete";
    */
    /// MPT Table.
    pub const SNAPSHOT_MPT_TABLE_NAME: &'static str = "snapshot_mpt";
}

impl KeyValueDbTypes for SnapshotMptDbSqlite {
    type ValueType = Box<[u8]>;
}

/// Automatically implement KeyValueDbTraitRead with the same code of
/// KvdbSqlite.
impl ReadImplFamily for SnapshotMptDbSqlite {
    type FamilyRepresentative = KvdbSqliteSharded<Box<[u8]>>;
}

impl OwnedReadImplFamily for SnapshotMptDbSqlite {
    type FamilyRepresentative = KvdbSqliteSharded<Box<[u8]>>;
}

impl SingleWriterImplFamily for SnapshotMptDbSqlite {
    type FamilyRepresentative = KvdbSqliteSharded<Box<[u8]>>;
}

impl<'db> OpenSnapshotMptTrait<'db> for SnapshotMptDbSqlite {
    type SnapshotDbAsOwnedType = SnapshotMpt<
        KvdbSqliteSharded<SnapshotMptDbValue>,
        KvdbSqliteSharded<SnapshotMptDbValue>,
    >;
    /// The 'static lifetime is for for<'db> KeyValueDbIterableTrait<'db, ...>.
    type SnapshotDbBorrowMutType = SnapshotMpt<
        KvdbSqliteShardedBorrowMut<'static, SnapshotMptDbValue>,
        KvdbSqliteShardedBorrowMut<'static, SnapshotMptDbValue>,
    >;
    type SnapshotDbBorrowSharedType = SnapshotMpt<
        KvdbSqliteShardedBorrowShared<'static, SnapshotMptDbValue>,
        KvdbSqliteShardedBorrowShared<'static, SnapshotMptDbValue>,
    >;

    fn open_snapshot_mpt_owned(
        &'db mut self,
    ) -> Result<Self::SnapshotDbBorrowMutType> {
        Ok(SnapshotMpt::new(unsafe {
            std::mem::transmute(
                KvdbSqliteShardedBorrowMut::<SnapshotMptDbValue>::new(
                    self.maybe_db_connections.as_mut().map(|b| &mut **b),
                    &SNAPSHOT_MPT_DB_STATEMENTS.mpt_statements,
                ),
            )
        })?)
    }

    fn open_snapshot_mpt_as_owned(
        &'db self,
    ) -> Result<Self::SnapshotDbAsOwnedType> {
        Ok(SnapshotMpt::new(
            KvdbSqliteSharded::<SnapshotMptDbValue>::new(
                self.try_clone_connections()?,
                SNAPSHOT_MPT_DB_STATEMENTS.mpt_statements.clone(),
            ),
        )?)
    }

    fn open_snapshot_mpt_shared(
        &'db self,
    ) -> Result<Self::SnapshotDbBorrowSharedType> {
        Ok(SnapshotMpt::new(unsafe {
            std::mem::transmute(KvdbSqliteShardedBorrowShared::<
                SnapshotMptDbValue,
            >::new(
                self.maybe_db_connections.as_ref().map(|b| &**b),
                &SNAPSHOT_MPT_DB_STATEMENTS.mpt_statements,
            ))
        })?)
    }
}

impl SnapshotMptDbSqlite {
    // FIXME: Do not clone connections.
    // FIXME: 1. we shouldn't not clone connections without acquire the
    // FIXME: semaphore; 2. we should implement the range iter for
    // FIXME: shared reading connections.
    fn try_clone_connections(&self) -> Result<Option<Box<[SqliteConnection]>>> {
        match &self.maybe_db_connections {
            None => Ok(None),
            Some(old_connections) => {
                let mut connections = Vec::with_capacity(old_connections.len());
                for old_connection in old_connections.iter() {
                    let new_connection = old_connection.try_clone()?;
                    connections.push(new_connection);
                }
                Ok(Some(connections.into_boxed_slice()))
            }
        }
    }

    pub fn set_remove_on_last_close(&self) {
        self.remove_on_close.store(true, Ordering::Relaxed);
    }

    pub fn open(
        snapshot_path: &Path, readonly: bool,
        // already_open_snapshots: &AlreadyOpenSnapshots<Self>,
        // open_semaphore: &Arc<Semaphore>,
    ) -> Result<SnapshotMptDbSqlite>
    {
        let kvdb_sqlite_sharded = KvdbSqliteSharded::<Box<[u8]>>::open(
            Self::DB_SHARDS,
            snapshot_path,
            readonly,
            SNAPSHOT_MPT_DB_STATEMENTS.mpt_statements.clone(),
        )?;

        Ok(Self {
            maybe_db_connections: kvdb_sqlite_sharded.into_connections(),
            already_open_snapshots: Default::default(),
            open_semaphore:  Arc::new(Semaphore::new(0)),
            path: snapshot_path.to_path_buf(),
            remove_on_close: Default::default(),
        })
    }

    pub fn create(
        snapshot_path: &Path,
        // already_open_snapshots: &AlreadyOpenSnapshots<Self>,
        // open_snapshots_semaphore: &Arc<Semaphore>,
    ) -> Result<SnapshotMptDbSqlite>
    {
        fs::create_dir_all(snapshot_path)?;
        let create_result = (|| -> Result<Box<[SqliteConnection]>> {
            let kvdb_sqlite_sharded =
                KvdbSqliteSharded::<Box<[u8]>>::create_and_open(
                    Self::DB_SHARDS,
                    snapshot_path,
                    SNAPSHOT_MPT_DB_STATEMENTS.mpt_statements.clone(),
                    /* create_table = */ true,
                    /* unsafe_mode = */ true,
                )?;
            let mut connections =
                // Safe to unwrap since the connections are newly created.
                kvdb_sqlite_sharded.into_connections().unwrap();
            // Create Snapshot MPT table.
            // KvdbSqliteSharded::<Self::ValueType>::create_table(
            //     &mut connections,
            //     &SNAPSHOT_DB_STATEMENTS.mpt_statements,
            // )?;
            Ok(connections)
        })();
        match create_result {
            Err(e) => {
                fs::remove_dir_all(&snapshot_path)?;
                bail!(e);
            }
            Ok(connections) => Ok(SnapshotMptDbSqlite {
                maybe_db_connections: Some(connections),
                already_open_snapshots: Default::default(),
                open_semaphore:  Arc::new(Semaphore::new(0)),
                path: snapshot_path.to_path_buf(),
                remove_on_close: Default::default(),
            }),
        }
    }
}

use crate::{
    impls::{
        delta_mpt::DeltaMptIterator,
        errors::*,
        merkle_patricia_trie::{MptKeyValue, MptMerger},
        storage_db::{
            kvdb_sqlite::KvdbSqliteStatements,
            kvdb_sqlite_sharded::{
                KvdbSqliteSharded, KvdbSqliteShardedBorrowMut,
                KvdbSqliteShardedBorrowShared,
                KvdbSqliteShardedDestructureTrait,
                KvdbSqliteShardedIteratorTag,
                KvdbSqliteShardedRefDestructureTrait,
            },
            snapshot_db_manager_sqlite::AlreadyOpenSnapshots,
            snapshot_mpt::{SnapshotMpt, SnapshotMptLoadNode},
            sqlite::SQLITE_NO_PARAM,
        },
    },
    storage_db::{
        KeyValueDbIterableTrait, KeyValueDbTraitSingleWriter, KeyValueDbTypes,
        OpenSnapshotMptTrait, OwnedReadImplByFamily, OwnedReadImplFamily,
        ReadImplByFamily, ReadImplFamily, SingleWriterImplByFamily,
        SingleWriterImplFamily, SnapshotDbTrait, SnapshotMptDbValue,
        SnapshotMptTraitReadAndIterate, SnapshotMptTraitRw,
    },
    utils::wrap::Wrap,
    KVInserter, SnapshotDbManagerSqlite, SqliteConnection,
};
use fallible_iterator::FallibleIterator;
use primitives::{MerkleHash, StorageKeyWithSpace};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Semaphore;
