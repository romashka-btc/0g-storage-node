use std::collections::HashSet;

use anyhow::Result;
use rand::seq::IteratorRandom;
use rand::Rng;
use storage::log_store::config::{ConfigTx, ConfigurableExt};
use storage::log_store::log_manager::DATA_DB_KEY;
use storage::log_store::Store;
use tokio::sync::RwLock;

/// TxStore is used to store pending transactions that to be synchronized in advance.
///
/// Basically, this store maintains an enumerable map data structure for `tx_seq`.
#[derive(Clone)]
pub struct TxStore {
    /// To allow multiple `TxStore` with different priority.
    name: &'static str,

    /// DB key for `count` value.
    key_count: String,
}

impl TxStore {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            key_count: format!("sync.manager.txs.{}.count", name),
        }
    }

    fn key_seq_to_index(&self, tx_seq: u64) -> String {
        format!("sync.manager.txs.{}.seq2index.{}", self.name, tx_seq)
    }

    fn key_index_to_seq(&self, index: usize) -> String {
        format!("sync.manager.txs.{}.index2seq.{}", self.name, index)
    }

    fn index_of(&self, store: &dyn Store, tx_seq: u64) -> Result<Option<usize>> {
        store.get_config_decoded(&self.key_seq_to_index(tx_seq), DATA_DB_KEY)
    }

    fn at(&self, store: &dyn Store, index: usize) -> Result<Option<u64>> {
        store.get_config_decoded(&self.key_index_to_seq(index), DATA_DB_KEY)
    }

    pub fn has(&self, store: &dyn Store, tx_seq: u64) -> Result<bool> {
        self.index_of(store, tx_seq).map(|idx| idx.is_some())
    }

    pub fn count(&self, store: &dyn Store) -> Result<usize> {
        store
            .get_config_decoded(&self.key_count, DATA_DB_KEY)
            .map(|x| x.unwrap_or(0))
    }

    pub fn add(
        &self,
        store: &dyn Store,
        db_tx: Option<&mut ConfigTx>,
        tx_seq: u64,
    ) -> Result<bool> {
        // already exists
        if self.has(store, tx_seq)? {
            return Ok(false);
        }

        let count = self.count(store)?;

        let mut tx = ConfigTx::default();
        tx.set_config(&self.key_index_to_seq(count), &tx_seq);
        tx.set_config(&self.key_seq_to_index(tx_seq), &count);
        tx.set_config(&self.key_count, &(count + 1));

        if let Some(db_tx) = db_tx {
            db_tx.append(&mut tx);
        } else {
            store.exec_configs(tx, DATA_DB_KEY)?;
        }

        Ok(true)
    }

    pub fn random(&self, store: &dyn Store) -> Result<Option<u64>> {
        let count = self.count(store)?;
        if count == 0 {
            return Ok(None);
        }

        let index = rand::thread_rng().gen_range(0..count);
        let tx_seq = self.at(store, index)?.expect("data corruption");

        Ok(Some(tx_seq))
    }

    pub fn remove(
        &self,
        store: &dyn Store,
        db_tx: Option<&mut ConfigTx>,
        tx_seq: u64,
    ) -> Result<bool> {
        let index = match self.index_of(store, tx_seq)? {
            Some(val) => val,
            None => return Ok(false),
        };

        let count = self.count(store)?;
        assert!(count > 0, "data corruption");

        let mut tx = ConfigTx::default();

        // update `count` value
        tx.set_config(&self.key_count, &(count - 1));

        // remove `seq2index` index
        tx.remove_config(&self.key_seq_to_index(tx_seq));

        if index == count - 1 {
            // remove `index2seq` index for the last element
            tx.remove_config(&self.key_index_to_seq(index));
        } else {
            // swap `back` to the `removed` slot
            let last_tx = self.at(store, count - 1)?.expect("data corruption");

            // update the `index2seq` for the removed element
            tx.set_config(&self.key_index_to_seq(index), &last_tx);

            // remove the last slot
            tx.remove_config(&self.key_index_to_seq(count - 1));

            // update `seq2index` index for the last tx
            tx.set_config(&self.key_seq_to_index(last_tx), &index);
        }

        if let Some(db_tx) = db_tx {
            db_tx.append(&mut tx);
        } else {
            store.exec_configs(tx, DATA_DB_KEY)?;
        }

        Ok(true)
    }
}

/// Cache the recent inserted tx in memory for random pick with priority.
pub struct CachedTxStore {
    tx_store: TxStore,
    cache_cap: usize,
    cache: RwLock<HashSet<u64>>,
}

impl CachedTxStore {
    pub fn new(name: &'static str, cache_cap: usize) -> Self {
        Self {
            tx_store: TxStore::new(name),
            cache_cap,
            cache: Default::default(),
        }
    }

    pub fn has(&self, store: &dyn Store, tx_seq: u64) -> Result<bool> {
        self.tx_store.has(store, tx_seq)
    }

    pub async fn count(&self, store: &dyn Store) -> Result<(usize, usize)> {
        if self.cache_cap == 0 {
            return Ok((self.tx_store.count(store)?, 0));
        }

        let cache = self.cache.read().await;

        Ok((self.tx_store.count(store)?, cache.len()))
    }

    pub async fn add(
        &self,
        store: &dyn Store,
        db_tx: Option<&mut ConfigTx>,
        tx_seq: u64,
    ) -> Result<bool> {
        if self.cache_cap == 0 {
            return self.tx_store.add(store, db_tx, tx_seq);
        }

        let mut cache = self.cache.write().await;

        let added = self.tx_store.add(store, db_tx, tx_seq)?;

        if added {
            cache.insert(tx_seq);

            if cache.len() > self.cache_cap {
                if let Some(popped) = cache.iter().choose(&mut rand::thread_rng()).cloned() {
                    cache.remove(&popped);
                }
            }
        }

        Ok(added)
    }

    pub async fn random(&self, store: &dyn Store) -> Result<Option<u64>> {
        if self.cache_cap == 0 {
            return self.tx_store.random(store);
        }

        let cache = self.cache.read().await;

        if let Some(v) = cache.iter().choose(&mut rand::thread_rng()).cloned() {
            return Ok(Some(v));
        }

        self.tx_store.random(store)
    }

    pub async fn remove(
        &self,
        store: &dyn Store,
        db_tx: Option<&mut ConfigTx>,
        tx_seq: u64,
    ) -> Result<bool> {
        if self.cache_cap == 0 {
            return self.tx_store.remove(store, db_tx, tx_seq);
        }

        let mut cache: tokio::sync::RwLockWriteGuard<'_, HashSet<u64>> = self.cache.write().await;

        let removed = self.tx_store.remove(store, db_tx, tx_seq)?;

        if removed {
            cache.remove(&tx_seq);
        }

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::tests::TestStoreRuntime;

    use super::TxStore;

    #[test]
    fn test_add() {
        let store = TestStoreRuntime::new_store();
        let tx_store = TxStore::new("foo");

        // count is 0 by default
        assert_eq!(tx_store.count(&store).unwrap(), 0);

        // add 3 txs
        assert!(tx_store.add(&store, None, 1).unwrap());
        assert!(tx_store.add(&store, None, 2).unwrap());
        assert!(tx_store.add(&store, None, 3).unwrap());

        // cannot add again
        assert!(!tx_store.add(&store, None, 1).unwrap());
        assert!(!tx_store.add(&store, None, 2).unwrap());
        assert!(!tx_store.add(&store, None, 3).unwrap());

        // count is 3 after insertion
        assert_eq!(tx_store.count(&store).unwrap(), 3);

        // check index of tx
        assert_eq!(tx_store.index_of(&store, 1).unwrap(), Some(0));
        assert_eq!(tx_store.index_of(&store, 2).unwrap(), Some(1));
        assert_eq!(tx_store.index_of(&store, 3).unwrap(), Some(2));
        assert_eq!(tx_store.index_of(&store, 4).unwrap(), None);

        // check tx of index
        assert_eq!(tx_store.at(&store, 0).unwrap(), Some(1));
        assert_eq!(tx_store.at(&store, 1).unwrap(), Some(2));
        assert_eq!(tx_store.at(&store, 2).unwrap(), Some(3));
        assert_eq!(tx_store.at(&store, 3).unwrap(), None);
    }

    #[test]
    fn test_random() {
        let store = TestStoreRuntime::new_store();
        let tx_store = TxStore::new("foo");

        assert_eq!(tx_store.random(&store).unwrap(), None);

        assert!(tx_store.add(&store, None, 1).unwrap());
        assert!(tx_store.add(&store, None, 2).unwrap());
        assert!(tx_store.add(&store, None, 3).unwrap());

        let tx_seq = tx_store
            .random(&store)
            .unwrap()
            .expect("should randomly pick one");
        assert!((1..=3).contains(&tx_seq));
    }

    #[test]
    fn test_remove_tail() {
        let store = TestStoreRuntime::new_store();
        let tx_store = TxStore::new("foo");

        assert!(tx_store.add(&store, None, 1).unwrap());
        assert!(tx_store.add(&store, None, 2).unwrap());
        assert!(tx_store.add(&store, None, 3).unwrap());

        assert!(tx_store.remove(&store, None, 3).unwrap());

        assert_eq!(tx_store.count(&store).unwrap(), 2);
        assert_eq!(tx_store.index_of(&store, 1).unwrap(), Some(0));
        assert_eq!(tx_store.index_of(&store, 2).unwrap(), Some(1));
        assert_eq!(tx_store.index_of(&store, 3).unwrap(), None);

        assert_eq!(tx_store.at(&store, 0).unwrap(), Some(1));
        assert_eq!(tx_store.at(&store, 1).unwrap(), Some(2));
        assert_eq!(tx_store.at(&store, 2).unwrap(), None);
    }

    #[test]
    fn test_remove_swap() {
        let store = TestStoreRuntime::new_store();
        let tx_store = TxStore::new("foo");

        assert!(tx_store.add(&store, None, 1).unwrap());
        assert!(tx_store.add(&store, None, 2).unwrap());
        assert!(tx_store.add(&store, None, 3).unwrap());
        assert!(tx_store.add(&store, None, 4).unwrap());

        assert!(tx_store.remove(&store, None, 2).unwrap());

        assert_eq!(tx_store.count(&store).unwrap(), 3);
        assert_eq!(tx_store.index_of(&store, 1).unwrap(), Some(0));
        assert_eq!(tx_store.index_of(&store, 2).unwrap(), None);
        assert_eq!(tx_store.index_of(&store, 3).unwrap(), Some(2));
        assert_eq!(tx_store.index_of(&store, 4).unwrap(), Some(1));

        assert_eq!(tx_store.at(&store, 0).unwrap(), Some(1));
        assert_eq!(tx_store.at(&store, 1).unwrap(), Some(4));
        assert_eq!(tx_store.at(&store, 2).unwrap(), Some(3));
        assert_eq!(tx_store.at(&store, 3).unwrap(), None);
    }
}
