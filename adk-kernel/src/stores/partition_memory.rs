//! The in-memory [`PartitionStore`] (spec 3.2 §7.8, §13 Q8).

use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::error::Result;
use crate::partition::{CrossingFilter, CrossingRecord, Partition, PartitionId, PartitionStore};

/// A [`PartitionStore`] in process memory, for tests and a kernel with no durable store
/// (spec 3.2 §7.8). Its records end with the process.
///
/// # Example
///
/// ```rust
/// use adk_kernel::{InMemoryPartitionStore, Partition, PartitionStore};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> adk_kernel::Result<()> {
/// let store = InMemoryPartitionStore::new();
/// store.put(Partition::new("work", "work")).await?;
/// store.put(Partition::new("home", "home")).await?;
/// let ids: Vec<_> = store.list().await?.into_iter().map(|p| p.id.to_string()).collect();
/// assert_eq!(ids, ["home", "work"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct InMemoryPartitionStore {
    partitions: RwLock<BTreeMap<PartitionId, Partition>>,
    crossings: RwLock<Vec<CrossingRecord>>,
}

impl InMemoryPartitionStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PartitionStore for InMemoryPartitionStore {
    async fn put(&self, partition: Partition) -> Result<()> {
        self.partitions.write().await.insert(partition.id.clone(), partition);
        Ok(())
    }

    async fn get(&self, id: &PartitionId) -> Result<Option<Partition>> {
        Ok(self.partitions.read().await.get(id).cloned())
    }

    async fn list(&self) -> Result<Vec<Partition>> {
        Ok(self.partitions.read().await.values().cloned().collect())
    }

    async fn delete(&self, id: &PartitionId) -> Result<bool> {
        Ok(self.partitions.write().await.remove(id).is_some())
    }

    async fn record_crossing(&self, record: CrossingRecord) -> Result<()> {
        self.crossings.write().await.push(record);
        Ok(())
    }

    async fn crossings(&self, filter: &CrossingFilter) -> Result<Vec<CrossingRecord>> {
        let mut records: Vec<CrossingRecord> = self
            .crossings
            .read()
            .await
            .iter()
            .filter(|record| filter.matches(record))
            .cloned()
            .collect();
        records.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        records.truncate(filter.limit.unwrap_or(usize::MAX));
        Ok(records)
    }
}
