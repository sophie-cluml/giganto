use crate::{ingestion, storage::Database};
use anyhow::{bail, Result};
use async_graphql::{Context, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};

pub struct Query;

#[derive(SimpleObject, Debug)]
pub struct ConnRawEvent {
    orig_addr: String,
    resp_addr: String,
    orig_port: u16,
    resp_port: u16,
    proto: u8,
    duration: i64,
    orig_bytes: u64,
    resp_bytes: u64,
    orig_pkts: u64,
    resp_pkts: u64,
}

#[derive(SimpleObject, Debug)]
pub struct DnsRawEvent {
    orig_addr: String,
    resp_addr: String,
    orig_port: u16,
    resp_port: u16,
    proto: u8,
    query: String,
}

#[derive(SimpleObject, Debug)]
pub struct HttpRawEvent {
    orig_addr: String,
    resp_addr: String,
    orig_port: u16,
    resp_port: u16,
    method: String,
    host: String,
    uri: String,
    referrer: String,
    user_agent: String,
    status_code: u16,
}

#[derive(SimpleObject, Debug)]
pub struct RdpRawEvent {
    orig_addr: String,
    resp_addr: String,
    orig_port: u16,
    resp_port: u16,
    cookie: String,
}

impl From<ingestion::Conn> for ConnRawEvent {
    fn from(c: ingestion::Conn) -> ConnRawEvent {
        ConnRawEvent {
            orig_addr: c.orig_addr.to_string(),
            resp_addr: c.resp_addr.to_string(),
            orig_port: c.orig_port,
            resp_port: c.resp_port,
            proto: c.proto,
            duration: c.duration,
            orig_bytes: c.orig_bytes,
            resp_bytes: c.resp_bytes,
            orig_pkts: c.orig_pkts,
            resp_pkts: c.resp_pkts,
        }
    }
}

impl From<ingestion::DnsConn> for DnsRawEvent {
    fn from(d: ingestion::DnsConn) -> DnsRawEvent {
        DnsRawEvent {
            orig_addr: d.orig_addr.to_string(),
            resp_addr: d.resp_addr.to_string(),
            orig_port: d.orig_port,
            resp_port: d.resp_port,
            proto: d.proto,
            query: d.query,
        }
    }
}

impl From<ingestion::HttpConn> for HttpRawEvent {
    fn from(h: ingestion::HttpConn) -> HttpRawEvent {
        HttpRawEvent {
            orig_addr: h.orig_addr.to_string(),
            resp_addr: h.resp_addr.to_string(),
            orig_port: h.orig_port,
            resp_port: h.resp_port,
            method: h.method,
            host: h.host,
            uri: h.uri,
            referrer: h.referrer,
            user_agent: h.user_agent,
            status_code: h.status_code,
        }
    }
}

impl From<ingestion::RdpConn> for RdpRawEvent {
    fn from(r: ingestion::RdpConn) -> RdpRawEvent {
        RdpRawEvent {
            orig_addr: r.orig_addr.to_string(),
            resp_addr: r.resp_addr.to_string(),
            orig_port: r.orig_port,
            resp_port: r.resp_port,
            cookie: r.cookie,
        }
    }
}

#[Object]
impl Query {
    pub async fn conn_raw_events<'ctx>(
        &self,
        ctx: &Context<'ctx>,
        source: String,
    ) -> Result<Vec<ConnRawEvent>> {
        let mut raw_vec = Vec::new();
        let db = match ctx.data::<Database>() {
            Ok(r) => r,
            Err(e) => bail!("{:?}", e),
        };
        for raw_data in db.conn_store()?.src_raw_events(&source) {
            let de_conn = bincode::deserialize::<ingestion::Conn>(&raw_data)?;
            raw_vec.push(ConnRawEvent::from(de_conn));
        }

        Ok(raw_vec)
    }

    pub async fn log_raw_events<'ctx>(
        &self,
        ctx: &Context<'ctx>,
        source: String,
        kind: String,
    ) -> Result<Vec<String>> {
        let mut raw_vec = Vec::new();
        let db = match ctx.data::<Database>() {
            Ok(r) => r,
            Err(e) => bail!("{:?}", e),
        };
        for raw_data in db.log_store()?.log_events(&source, &kind) {
            let de_log = bincode::deserialize::<ingestion::Log>(&raw_data)?;
            let (k, r) = de_log.log;
            if k == kind {
                raw_vec.push(base64::encode(r));
            }
        }
        Ok(raw_vec)
    }

    pub async fn dns_raw_events<'ctx>(
        &self,
        ctx: &Context<'ctx>,
        source: String,
    ) -> Result<Vec<DnsRawEvent>> {
        let mut raw_vec = Vec::new();
        let db = match ctx.data::<Database>() {
            Ok(r) => r,
            Err(e) => bail!("{:?}", e),
        };
        for raw_data in db.dns_store()?.src_raw_events(&source) {
            let de_dns = bincode::deserialize::<ingestion::DnsConn>(&raw_data)?;
            raw_vec.push(DnsRawEvent::from(de_dns));
        }

        Ok(raw_vec)
    }

    pub async fn http_raw_events<'ctx>(
        &self,
        ctx: &Context<'ctx>,
        source: String,
    ) -> Result<Vec<HttpRawEvent>> {
        let mut raw_vec = Vec::new();
        let db = match ctx.data::<Database>() {
            Ok(r) => r,
            Err(e) => bail!("{:?}", e),
        };
        for raw_data in db.http_store()?.src_raw_events(&source) {
            let de_http = bincode::deserialize::<ingestion::HttpConn>(&raw_data)?;
            raw_vec.push(HttpRawEvent::from(de_http));
        }

        Ok(raw_vec)
    }

    pub async fn rdp_raw_events<'ctx>(
        &self,
        ctx: &Context<'ctx>,
        source: String,
    ) -> Result<Vec<RdpRawEvent>> {
        let mut raw_vec = Vec::new();
        let db = match ctx.data::<Database>() {
            Ok(r) => r,
            Err(e) => bail!("{:?}", e),
        };
        for raw_data in db.rdp_store()?.src_raw_events(&source) {
            let de_rdp = bincode::deserialize::<ingestion::RdpConn>(&raw_data)?;
            raw_vec.push(RdpRawEvent::from(de_rdp));
        }

        Ok(raw_vec)
    }
}

pub fn schema(database: Database) -> Schema<Query, EmptyMutation, EmptySubscription> {
    Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(database)
        .finish()
}
