#[allow(clippy::upper_case_acronyms)]
type BEU64 = heed::zerocopy::U64<heed::byteorder::BE>;

const UID_TASK_IDS: &str = "uid_task_id";
const TASKS: &str = "tasks";

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::convert::TryInto;
use std::mem::size_of;
use std::path::Path;
use std::result::Result as StdResult;

use heed::types::{ByteSlice, OwnedType, SerdeJson, Unit};
use heed::{BytesDecode, BytesEncode, Database, Env, EnvOpenOptions, RoTxn, RwTxn};

use crate::tasks::task::{Task, TaskId};

use super::super::Result;

use super::TaskFilter;

enum IndexUidTaskIdCodec {}

impl<'a> BytesEncode<'a> for IndexUidTaskIdCodec {
    type EItem = (&'a str, TaskId);

    fn bytes_encode((s, id): &'a Self::EItem) -> Option<Cow<'a, [u8]>> {
        let size = s.len() + std::mem::size_of::<TaskId>() + 1;
        if size > 512 {
            return None;
        }
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(s.as_bytes());
        // null terminate the string
        b.push(0);
        b.extend_from_slice(&id.to_be_bytes());
        Some(Cow::Owned(b))
    }
}

impl<'a> BytesDecode<'a> for IndexUidTaskIdCodec {
    type DItem = (&'a str, TaskId);

    fn bytes_decode(bytes: &'a [u8]) -> Option<Self::DItem> {
        let len = bytes.len();
        let s_end = len.checked_sub(size_of::<TaskId>())?.checked_sub(1)?;
        let str_bytes = &bytes[..s_end];
        let str = std::str::from_utf8(str_bytes).ok()?;
        let id = TaskId::from_be_bytes(bytes[(len - size_of::<TaskId>())..].try_into().ok()?);
        Some((str, id))
    }
}

pub struct Store {
    env: Env,
    uids_task_ids: Database<IndexUidTaskIdCodec, Unit>,
    tasks: Database<OwnedType<BEU64>, SerdeJson<Task>>,
}

impl Store {
    pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
        let mut options = EnvOpenOptions::new();
        options.map_size(size);
        options.max_dbs(1000);
        let env = options.open(path)?;

        let uids_task_ids = env.create_database(Some(UID_TASK_IDS))?;
        let tasks = env.create_database(Some(TASKS))?;

        Ok(Self {
            env,
            uids_task_ids,
            tasks,
        })
    }

    pub fn wtxn(&self) -> Result<RwTxn> {
        Ok(self.env.write_txn()?)
    }

    pub fn rtxn(&self) -> Result<RoTxn> {
        Ok(self.env.read_txn()?)
    }

    /// Returns the id for the next task.
    ///
    /// The required `mut txn` acts as a reservation system. It guarantees that as long as you commit
    /// the task to the store in the same transaction, no one else will hav this task id.
    pub fn next_task_id(&self, txn: &mut RwTxn) -> Result<TaskId> {
        let id = self
            .tasks
            .lazily_decode_data()
            .last(txn)?
            .map(|(id, _)| id.get() + 1)
            .unwrap_or(0);
        Ok(id)
    }

    pub fn put(&self, txn: &mut RwTxn, task: &Task) -> Result<()> {
        self.tasks.put(txn, &BEU64::new(task.id), task)?;
        self.uids_task_ids
            .put(txn, &(&task.index_uid, task.id), &())?;

        Ok(())
    }

    pub fn get(&self, txn: &RoTxn, id: TaskId) -> Result<Option<Task>> {
        let task = self.tasks.get(txn, &BEU64::new(id))?;
        Ok(task)
    }

    pub fn task_count(&self, txn: &RoTxn) -> Result<usize> {
        Ok(self.tasks.len(txn)?)
    }

    pub fn list_tasks<'a>(
        &self,
        txn: &'a RoTxn,
        from: Option<TaskId>,
        filter: Option<TaskFilter>,
        limit: Option<usize>,
    ) -> Result<Vec<Task>> {
        let iter: Box<dyn Iterator<Item = StdResult<_, heed::Error>>> = match filter {
            Some(filter) => {
                let iter = self
                    .compute_candidates(txn, filter)?
                    .into_iter()
                    .skip_while(|&id| from.map(|from| from == id).unwrap_or_default())
                    .filter_map(|id| self.tasks.get(txn, &BEU64::new(id)).transpose());

                Box::new(iter)
            }
            None => match from {
                Some(id) => Box::new(
                    self.tasks
                        .rev_range(txn, &(..BEU64::new(id)))?
                        .map(|r| r.map(|(_, t)| t)),
                ),
                None => Box::new(self.tasks.rev_iter(txn)?.map(|r| r.map(|(_, t)| t))),
            },
        };

        // Collect 'limit' task if it exists or all of them.
        let tasks = iter
            .take(limit.unwrap_or(usize::MAX))
            .try_fold::<_, _, StdResult<_, heed::Error>>(Vec::new(), |mut v, task| {
                v.push(task?);
                Ok(v)
            })?;

        Ok(tasks)
    }

    fn compute_candidates(
        &self,
        txn: &heed::RoTxn,
        filter: TaskFilter,
    ) -> Result<BTreeSet<TaskId>> {
        let mut candidates = BTreeSet::new();
        if let Some(indexes) = filter.indexes {
            for index in indexes {
                // We need to prefix search the null terminated string to make sure that we only
                // get exact matches for the index, and not other uids that would share the same
                // prefix, i.e test and test1.
                let mut index_uid = index.as_bytes().to_vec();
                index_uid.push(0);
                self.uids_task_ids
                    .remap_key_type::<ByteSlice>()
                    .prefix_iter(txn, &index_uid)?
                    .try_fold(
                        &mut candidates,
                        |candidates, entry| -> StdResult<_, heed::Error> {
                            let (key, _) = entry?;
                            let (_, id) = IndexUidTaskIdCodec::bytes_decode(key)
                                .ok_or(heed::Error::Decoding)?;
                            candidates.insert(id);

                            Ok(candidates)
                        },
                    )?;
            }
        }

        Ok(candidates)
    }
}

#[cfg(test)]
pub mod test {
    use nelson::Mocker;
    use proptest::prelude::*;
    use proptest::collection::vec;

    use super::*;

    pub enum MockStore {
        Real(Store),
        Fake(Mocker),
    }

    impl MockStore {
        pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
            Ok(Self::Real(Store::new(path, size)?))
        }

        pub fn wtxn(&self) -> Result<RwTxn> {
            match self {
                MockStore::Real(index) => index.wtxn(),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn rtxn(&self) -> Result<RoTxn> {
            match self {
                MockStore::Real(index) => index.rtxn(),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn next_task_id(&self, txn: &mut RwTxn) -> Result<TaskId> {
            match self {
                MockStore::Real(index) => index.next_task_id(txn),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn put(&self, txn: &mut RwTxn, task: &Task) -> Result<()> {
            match self {
                MockStore::Real(index) => index.put(txn, task),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn get(&self, txn: &RoTxn, id: TaskId) -> Result<Option<Task>> {
            match self {
                MockStore::Real(index) => index.get(txn, id),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn task_count(&self, txn: &RoTxn) -> Result<usize> {
            match self {
                MockStore::Real(index) => index.task_count(txn),
                MockStore::Fake(_) => todo!(),
            }
        }

        pub fn list_tasks<'a>(
            &self,
            txn: &'a RoTxn,
            from: Option<TaskId>,
            filter: Option<TaskFilter>,
            limit: Option<usize>,
        ) -> Result<Vec<Task>> {
            match self {
                MockStore::Real(index) => index.list_tasks(txn, from, filter, limit),
                MockStore::Fake(_) => todo!(),
            }
        }
    }

    proptest! {
        #[test]
        fn encode_decode_roundtrip(index_uid in "[a-zA-Z0-9_-]*", task_id in 0..TaskId::MAX) {
            let value = (index_uid.as_ref(), task_id);
            let bytes = IndexUidTaskIdCodec::bytes_encode(&value).unwrap();
            let (index, id) = IndexUidTaskIdCodec::bytes_decode(bytes.as_ref()).unwrap();
            assert_eq!(index_uid, index);
            assert_eq!(task_id, id);
        }

        #[test]
        fn encode_doesnt_crash(index_uid in "\\PC*", task_id in 0..TaskId::MAX) {
            let value = (index_uid.as_ref(), task_id);
            IndexUidTaskIdCodec::bytes_encode(&value);
        }

        #[test]
        fn decode_doesnt_crash(bytes in vec(any::<u8>(), 0..1000)) {
            IndexUidTaskIdCodec::bytes_decode(&bytes);
        }
    }
}
