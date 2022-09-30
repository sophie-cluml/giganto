//! Raw event storage based on RocksDB.
use crate::ingestion;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
pub use rocksdb::Direction;
use rocksdb::{ColumnFamily, DBIteratorWithThreadMode, Options, DB};
use serde::de::DeserializeOwned;
use std::{cmp, marker::PhantomData, path::Path, sync::Arc, time::Duration};
use tokio::time;
use tracing::error;

const RAW_DATA_COLUMN_FAMILY_NAMES: [&str; 5] = ["conn", "dns", "log", "http", "rdp"];
const META_DATA_COLUMN_FAMILY_NAMES: [&str; 1] = ["sources"];
const TIMESTAMP_SIZE: usize = 8;

#[derive(Clone)]
pub struct Database {
    db: Arc<DB>,
}

impl Database {
    /// Opens the database at the given path.
    pub fn open(path: &Path) -> Result<Database> {
        let mut opts = Options::default();
        let mut cfs: Vec<&str> = Vec::with_capacity(
            RAW_DATA_COLUMN_FAMILY_NAMES.len() + META_DATA_COLUMN_FAMILY_NAMES.len(),
        );
        cfs.extend(&RAW_DATA_COLUMN_FAMILY_NAMES);
        cfs.extend(&META_DATA_COLUMN_FAMILY_NAMES);

        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let db = DB::open_cf(&opts, path, cfs).context("cannot open database")?;

        Ok(Database { db: Arc::new(db) })
    }

    /// Returns the raw event store for all type. (exclude log type)
    pub fn retain_period_store(&self) -> Result<Vec<RawEventStore>> {
        let mut stores: Vec<RawEventStore> = Vec::new();
        for store in RAW_DATA_COLUMN_FAMILY_NAMES {
            if !store.eq("log") {
                let cf = self
                    .db
                    .cf_handle(store)
                    .context("cannot access column family")?;
                stores.push(RawEventStore { db: &self.db, cf });
            }
        }
        Ok(stores)
    }

    /// Returns the raw event store for connections.
    pub fn conn_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("conn")
            .context("cannot access conn column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }

    /// Returns the raw event store for dns.
    pub fn dns_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("dns")
            .context("cannot access dns column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }

    /// Returns the raw event store for log.
    pub fn log_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("log")
            .context("cannot access log column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }

    /// Returns the raw event store for http.
    pub fn http_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("http")
            .context("cannot access http column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }

    /// Returns the raw event store for rdp.
    pub fn rdp_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("rdp")
            .context("cannot access rdp column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }

    /// Returns the raw event store for connection sources
    pub fn sources_store(&self) -> Result<RawEventStore> {
        let cf = self
            .db
            .cf_handle("sources")
            .context("cannot access sources column family")?;
        Ok(RawEventStore { db: &self.db, cf })
    }
}

pub struct RawEventStore<'db> {
    db: &'db DB,
    cf: &'db ColumnFamily,
}

unsafe impl<'db> Send for RawEventStore<'db> {}

impl<'db> RawEventStore<'db> {
    pub fn append(&self, key: &[u8], raw_event: &[u8]) -> Result<()> {
        self.db.put_cf(self.cf, key, raw_event)?;
        Ok(())
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.db.delete_cf(self.cf, key)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        self.db.flush_cf(self.cf)?;
        Ok(())
    }

    pub fn conn_iter(
        &self,
        from: &[u8],
        to: &[u8],
        direction: Direction,
    ) -> Iter<'db, ingestion::Conn> {
        Iter::new(
            self.db
                .iterator_cf(self.cf, rocksdb::IteratorMode::From(from, direction)),
            to.to_vec(),
            direction,
        )
    }

    pub fn dns_iter(
        &self,
        from: &[u8],
        to: &[u8],
        direction: Direction,
    ) -> Iter<'db, ingestion::DnsConn> {
        Iter::new(
            self.db
                .iterator_cf(self.cf, rocksdb::IteratorMode::From(from, direction)),
            to.to_vec(),
            direction,
        )
    }

    pub fn http_iter(
        &self,
        from: &[u8],
        to: &[u8],
        direction: Direction,
    ) -> Iter<'db, ingestion::HttpConn> {
        Iter::new(
            self.db
                .iterator_cf(self.cf, rocksdb::IteratorMode::From(from, direction)),
            to.to_vec(),
            direction,
        )
    }

    pub fn log_iter(
        &self,
        from: &[u8],
        to: &[u8],
        direction: Direction,
    ) -> Iter<'db, ingestion::Log> {
        Iter::new(
            self.db
                .iterator_cf(self.cf, rocksdb::IteratorMode::From(from, direction)),
            to.to_vec(),
            direction,
        )
    }

    pub fn rdp_iter(
        &self,
        from: &[u8],
        to: &[u8],
        direction: Direction,
    ) -> Iter<'db, ingestion::RdpConn> {
        Iter::new(
            self.db
                .iterator_cf(self.cf, rocksdb::IteratorMode::From(from, direction)),
            to.to_vec(),
            direction,
        )
    }

    /// Returns the all key values ​​of column family.
    pub fn all_keys(&self) -> Vec<Vec<u8>> {
        let mut keys = Vec::new();
        let iter = self
            .db
            .iterator_cf(self.cf, rocksdb::IteratorMode::Start)
            .flatten();
        for (key, _val) in iter {
            keys.push(key.to_vec());
        }
        keys
    }
}

/// Creates a key that precedes the key calculated from the given `prefix` and
/// `time`.
pub fn lower_bound_key(prefix: &[u8], time: Option<DateTime<Utc>>) -> Vec<u8> {
    let mut lower_bound = Vec::with_capacity(prefix.len() + 1);
    lower_bound.extend(prefix);
    lower_bound.push(0);
    if let Some(time) = time {
        let ns = time.timestamp_nanos();
        if let Some(ns) = ns.checked_sub(1) {
            if ns >= 0 {
                lower_bound.extend(ns.to_be_bytes());
            }
        }
    }
    lower_bound
}

/// Creates a key that corresponds to the key calculated from the given `prefix`
/// and `time`.
pub fn upper_bound_key(prefix: &[u8], time: Option<DateTime<Utc>>) -> Vec<u8> {
    let mut upper_bound = Vec::with_capacity(prefix.len() + 1);
    upper_bound.extend(prefix);
    if let Some(time) = time {
        let ns = time.timestamp_nanos();
        upper_bound.push(0);
        upper_bound.extend(ns.to_be_bytes());
    } else {
        upper_bound.push(1);
    }
    upper_bound
}

pub type KeyValue<T> = (Box<[u8]>, T);

pub struct Iter<'d, T> {
    inner: DBIteratorWithThreadMode<'d, DB>,
    boundary: Vec<u8>,
    cond: cmp::Ordering,
    phantom: PhantomData<T>,
}

impl<'d, T> Iter<'d, T> {
    pub fn new(
        inner: DBIteratorWithThreadMode<'d, DB>,
        boundary: Vec<u8>,
        direction: Direction,
    ) -> Self {
        let cond = match direction {
            Direction::Forward => cmp::Ordering::Less,
            Direction::Reverse => cmp::Ordering::Greater,
        };

        Self {
            inner,
            boundary,
            cond,
            phantom: PhantomData,
        }
    }
}

impl<'d, T> Iterator for Iter<'d, T>
where
    T: DeserializeOwned,
{
    type Item = anyhow::Result<KeyValue<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().and_then(|item| match item {
            Ok((key, value)) => {
                if key.as_ref().cmp(&self.boundary) == self.cond {
                    None
                } else {
                    Some(
                        bincode::deserialize::<T>(&value)
                            .map(|value| (key, value))
                            .map_err(Into::into),
                    )
                }
            }
            Err(e) => Some(Err(e.into())),
        })
    }
}

pub fn gen_key(args: Vec<Vec<u8>>) -> Vec<u8> {
    let mut key: Vec<u8> = Vec::new();
    for arg in args {
        key.extend_from_slice(&arg);
        key.push(0x00);
    }
    key.pop();
    key
}

pub async fn retain_periodically(
    duration: Duration,
    retention_period: Duration,
    db: Database,
) -> Result<()> {
    let mut itv = time::interval(duration);
    let retention_duration = i64::try_from(retention_period.as_nanos())?;
    let from_timestamp = DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(61, 0), Utc)
        .timestamp_nanos()
        .to_be_bytes()
        .to_vec();
    loop {
        itv.tick().await;
        let standard_duration = Utc::now().timestamp_nanos() - retention_duration;
        let standard_duration_vec = standard_duration.to_be_bytes().to_vec();
        let sources = db.sources_store()?.all_keys();
        let all_store = db.retain_period_store()?;
        let log_store = db.log_store()?;

        for source in sources {
            let mut from: Vec<u8> = source.clone();
            from.push(0x00);
            from.extend_from_slice(&from_timestamp);

            let mut to: Vec<u8> = source.clone();
            to.push(0x00);
            to.extend_from_slice(&standard_duration_vec);

            for store in &all_store {
                if store.db.delete_range_cf(store.cf, &from, &to).is_err() {
                    error!("Failed to delete range data");
                }
            }

            for (key, _) in log_store
                .db
                .prefix_iterator_cf(log_store.cf, source.clone())
                .flatten()
                .filter(|(key, _)| {
                    let store_duration =
                        i64::from_be_bytes(key[(key.len() - TIMESTAMP_SIZE)..].try_into().unwrap());
                    standard_duration > store_duration
                })
            {
                if log_store.delete(&key).is_err() {
                    error!("Failed to delete log data");
                }
            }
        }
    }
}
