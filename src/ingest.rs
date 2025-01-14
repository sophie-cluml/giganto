pub mod implement;
#[cfg(test)]
mod tests;

use crate::publish::send_direct_stream;
use crate::server::{
    certificate_info, config_server, extract_cert_from_conn, SERVER_CONNNECTION_DELAY,
    SERVER_ENDPOINT_DELAY,
};
use crate::storage::{Database, RawEventStore, StorageKey};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use giganto_client::ingest::log::SecuLog;
use giganto_client::{
    connection::server_handshake,
    frame::{self, RecvError, SendError},
    ingest::{
        log::{Log, OpLog},
        receive_event, receive_record_header,
        statistics::Statistics,
        timeseries::PeriodicTimeSeries,
        Packet,
    },
    RawEventKind,
};
use quinn::{Connection, Endpoint, RecvStream, SendStream, ServerConfig};
use rustls::{Certificate, PrivateKey};
use std::sync::atomic::AtomicU16;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    select,
    sync::{
        mpsc::{channel, Receiver, Sender, UnboundedSender},
        Mutex, Notify, RwLock,
    },
    task, time,
    time::sleep,
};
use tracing::{error, info};
use x509_parser::nom::AsBytes;

const ACK_ROTATION_CNT: u16 = 1024;
const ACK_INTERVAL_TIME: u64 = 60;
const CHANNEL_CLOSE_MESSAGE: &[u8; 12] = b"channel done";
const CHANNEL_CLOSE_TIMESTAMP: i64 = -1;
const NO_TIMESTAMP: i64 = 0;
const SOURCE_INTERVAL: u64 = 60 * 60 * 24;
const INGEST_VERSION_REQ: &str = ">=0.15.0,<0.16.0";

type SourceInfo = (String, DateTime<Utc>, ConnState, bool);
pub type PacketSources = Arc<RwLock<HashMap<String, Connection>>>;
pub type Sources = Arc<RwLock<HashMap<String, DateTime<Utc>>>>;
pub type StreamDirectChannel = Arc<RwLock<HashMap<String, UnboundedSender<Vec<u8>>>>>;

enum ConnState {
    Connected,
    Disconnected,
}

pub struct Server {
    server_config: ServerConfig,
    server_address: SocketAddr,
}

impl Server {
    pub fn new(
        addr: SocketAddr,
        certs: Vec<Certificate>,
        key: PrivateKey,
        files: Vec<Vec<u8>>,
    ) -> Self {
        let server_config = config_server(certs, key, files)
            .expect("server configuration error with cert, key or root");
        Server {
            server_config,
            server_address: addr,
        }
    }

    pub async fn run(
        self,
        db: Database,
        packet_sources: PacketSources,
        sources: Sources,
        stream_direct_channel: StreamDirectChannel,
        wait_shutdown: Arc<Notify>,
        notify_source: Option<Arc<Notify>>,
    ) {
        let endpoint = Endpoint::server(self.server_config, self.server_address).expect("endpoint");
        info!(
            "listening on {}",
            endpoint.local_addr().expect("for local addr display")
        );

        let (tx, rx): (Sender<SourceInfo>, Receiver<SourceInfo>) = channel(100);
        let source_db = db.clone();
        task::spawn(check_sources_conn(
            source_db,
            packet_sources.clone(),
            sources,
            rx,
            notify_source,
        ));

        let shutdown_signal = Arc::new(AtomicBool::new(false));

        loop {
            select! {
                Some(conn) = endpoint.accept()  => {
                    let sender = tx.clone();
                    let db = db.clone();
                    let packet_sources = packet_sources.clone();
                    let stream_direct_channel = stream_direct_channel.clone();
                    let shutdown_notify = wait_shutdown.clone();
                    let shutdown_sig = shutdown_signal.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_connection(conn, db, packet_sources, sender, stream_direct_channel,shutdown_notify,shutdown_sig).await
                        {
                            error!("connection failed: {}", e);
                        }
                    });
                },
                () = wait_shutdown.notified() => {
                    shutdown_signal.store(true,Ordering::SeqCst); // Setting signal to handle termination on each channel.
                    sleep(Duration::from_millis(SERVER_ENDPOINT_DELAY)).await;      // Wait time for channels,connection to be ready for shutdown.
                    endpoint.close(0_u32.into(), &[]);
                    info!("Shutting down ingest");
                    wait_shutdown.notify_one();
                    break;
                },
            }
        }
    }
}

async fn handle_connection(
    conn: quinn::Connecting,
    db: Database,
    packet_sources: PacketSources,
    sender: Sender<SourceInfo>,
    stream_direct_channel: StreamDirectChannel,
    wait_shutdown: Arc<Notify>,
    shutdown_signal: Arc<AtomicBool>,
) -> Result<()> {
    let connection = conn.await?;
    match server_handshake(&connection, INGEST_VERSION_REQ).await {
        Ok((mut send, _)) => {
            info!("Compatible version");
            send.finish().await?;
        }
        Err(e) => {
            info!("Incompatible version");
            connection.close(quinn::VarInt::from_u32(0), e.to_string().as_bytes());
            bail!("{e}")
        }
    };

    let (agent, source) = certificate_info(&extract_cert_from_conn(&connection)?)?;
    let rep = agent.contains("reproduce");

    if !rep {
        packet_sources
            .write()
            .await
            .insert(source.clone(), connection.clone());
    }

    if let Err(error) = sender
        .send((source.clone(), Utc::now(), ConnState::Connected, rep))
        .await
    {
        error!("Failed to send channel data : {}", error);
    }
    loop {
        select! {
            stream = connection.accept_bi()  => {
                let stream = match stream {
                    Err(conn_err) => {
                        if let Err(error) = sender
                            .send((source, Utc::now(), ConnState::Disconnected, rep))
                            .await
                        {
                            error!("Failed to send internal channel data : {}", error);
                        }
                        match conn_err {
                            quinn::ConnectionError::ApplicationClosed(_) => {
                                info!("application closed");
                                return Ok(());
                            }
                            _ => return Err(conn_err.into()),
                        }
                    }
                    Ok(s) => s,
                };
                let source = source.clone();
                let db = db.clone();
                let stream_direct_channel = stream_direct_channel.clone();
                let shutdown_signal = shutdown_signal.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_request(source, stream, db, stream_direct_channel,shutdown_signal).await {
                        error!("failed: {}", e);
                    }
                });
            },
            () = wait_shutdown.notified() => {
                // Wait time for channels to be ready for shutdown.
                sleep(Duration::from_millis(SERVER_CONNNECTION_DELAY)).await;
                connection.close(0_u32.into(), &[]);
                return Ok(())
            },
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_request(
    source: String,
    (send, mut recv): (SendStream, RecvStream),
    db: Database,
    stream_direct_channel: StreamDirectChannel,
    shutdown_signal: Arc<AtomicBool>,
) -> Result<()> {
    let mut buf = [0; 4];
    receive_record_header(&mut recv, &mut buf)
        .await
        .map_err(|e| anyhow!("failed to read record type: {}", e))?;
    match RawEventKind::try_from(u32::from_le_bytes(buf)).context("unknown raw event kind")? {
        RawEventKind::Conn => {
            handle_data(
                send,
                recv,
                RawEventKind::Conn,
                Some(NetworkKey::new(&source, "conn")),
                source,
                db.conn_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Dns => {
            handle_data(
                send,
                recv,
                RawEventKind::Dns,
                Some(NetworkKey::new(&source, "dns")),
                source,
                db.dns_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Log => {
            handle_data(
                send,
                recv,
                RawEventKind::Log,
                Some(NetworkKey::new(&source, "log")),
                source,
                db.log_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Http => {
            handle_data(
                send,
                recv,
                RawEventKind::Http,
                Some(NetworkKey::new(&source, "http")),
                source,
                db.http_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Rdp => {
            handle_data(
                send,
                recv,
                RawEventKind::Rdp,
                Some(NetworkKey::new(&source, "rdp")),
                source,
                db.rdp_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::PeriodicTimeSeries => {
            handle_data(
                send,
                recv,
                RawEventKind::PeriodicTimeSeries,
                None,
                source,
                db.periodic_time_series_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Smtp => {
            handle_data(
                send,
                recv,
                RawEventKind::Smtp,
                Some(NetworkKey::new(&source, "smtp")),
                source,
                db.smtp_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Ntlm => {
            handle_data(
                send,
                recv,
                RawEventKind::Ntlm,
                Some(NetworkKey::new(&source, "ntlm")),
                source,
                db.ntlm_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Kerberos => {
            handle_data(
                send,
                recv,
                RawEventKind::Kerberos,
                Some(NetworkKey::new(&source, "kerberos")),
                source,
                db.kerberos_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Ssh => {
            handle_data(
                send,
                recv,
                RawEventKind::Ssh,
                Some(NetworkKey::new(&source, "ssh")),
                source,
                db.ssh_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::DceRpc => {
            handle_data(
                send,
                recv,
                RawEventKind::DceRpc,
                Some(NetworkKey::new(&source, "dce rpc")),
                source,
                db.dce_rpc_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Statistics => {
            handle_data(
                send,
                recv,
                RawEventKind::Statistics,
                None,
                source,
                db.statistics_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::OpLog => {
            handle_data(
                send,
                recv,
                RawEventKind::OpLog,
                None,
                source,
                db.op_log_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Packet => {
            handle_data(
                send,
                recv,
                RawEventKind::Packet,
                None,
                source,
                db.packet_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Ftp => {
            handle_data(
                send,
                recv,
                RawEventKind::Ftp,
                Some(NetworkKey::new(&source, "ftp")),
                source,
                db.ftp_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Mqtt => {
            handle_data(
                send,
                recv,
                RawEventKind::Mqtt,
                Some(NetworkKey::new(&source, "mqtt")),
                source,
                db.mqtt_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Ldap => {
            handle_data(
                send,
                recv,
                RawEventKind::Ldap,
                Some(NetworkKey::new(&source, "ldap")),
                source,
                db.ldap_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Tls => {
            handle_data(
                send,
                recv,
                RawEventKind::Tls,
                Some(NetworkKey::new(&source, "tls")),
                source,
                db.tls_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Smb => {
            handle_data(
                send,
                recv,
                RawEventKind::Smb,
                Some(NetworkKey::new(&source, "smb")),
                source,
                db.smb_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Nfs => {
            handle_data(
                send,
                recv,
                RawEventKind::Nfs,
                Some(NetworkKey::new(&source, "nfs")),
                source,
                db.nfs_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::ProcessCreate => {
            handle_data(
                send,
                recv,
                RawEventKind::ProcessCreate,
                None,
                source,
                db.process_create_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::FileCreateTime => {
            handle_data(
                send,
                recv,
                RawEventKind::FileCreateTime,
                None,
                source,
                db.file_create_time_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::NetworkConnect => {
            handle_data(
                send,
                recv,
                RawEventKind::NetworkConnect,
                None,
                source,
                db.network_connect_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::ProcessTerminate => {
            handle_data(
                send,
                recv,
                RawEventKind::ProcessTerminate,
                None,
                source,
                db.process_terminate_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::ImageLoad => {
            handle_data(
                send,
                recv,
                RawEventKind::ImageLoad,
                None,
                source,
                db.image_load_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::FileCreate => {
            handle_data(
                send,
                recv,
                RawEventKind::FileCreate,
                None,
                source,
                db.file_create_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::RegistryValueSet => {
            handle_data(
                send,
                recv,
                RawEventKind::RegistryValueSet,
                None,
                source,
                db.registry_value_set_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::RegistryKeyRename => {
            handle_data(
                send,
                recv,
                RawEventKind::RegistryKeyRename,
                None,
                source,
                db.registry_key_rename_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::FileCreateStreamHash => {
            handle_data(
                send,
                recv,
                RawEventKind::FileCreateStreamHash,
                None,
                source,
                db.file_create_stream_hash_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::PipeEvent => {
            handle_data(
                send,
                recv,
                RawEventKind::PipeEvent,
                None,
                source,
                db.pipe_event_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::DnsQuery => {
            handle_data(
                send,
                recv,
                RawEventKind::DnsQuery,
                None,
                source,
                db.dns_query_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::FileDelete => {
            handle_data(
                send,
                recv,
                RawEventKind::FileDelete,
                None,
                source,
                db.file_delete_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::ProcessTamper => {
            handle_data(
                send,
                recv,
                RawEventKind::ProcessTamper,
                None,
                source,
                db.process_tamper_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::FileDeleteDetected => {
            handle_data(
                send,
                recv,
                RawEventKind::FileDeleteDetected,
                None,
                source,
                db.file_delete_detected_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Netflow5 => {
            handle_data(
                send,
                recv,
                RawEventKind::Netflow5,
                None,
                source,
                db.netflow5_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::Netflow9 => {
            handle_data(
                send,
                recv,
                RawEventKind::Netflow9,
                None,
                source,
                db.netflow9_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        RawEventKind::SecuLog => {
            handle_data(
                send,
                recv,
                RawEventKind::SecuLog,
                None,
                source,
                db.secu_log_store()?,
                stream_direct_channel,
                shutdown_signal,
            )
            .await?;
        }
        _ => {
            error!("The record type message could not be processed.");
        }
    };
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn handle_data<T>(
    send: SendStream,
    mut recv: RecvStream,
    raw_event_kind: RawEventKind,
    network_key: Option<NetworkKey>,
    source: String,
    store: RawEventStore<'_, T>,
    stream_direct_channel: StreamDirectChannel,
    shutdown_signal: Arc<AtomicBool>,
) -> Result<()> {
    let sender_rotation = Arc::new(Mutex::new(send));
    let sender_interval = Arc::clone(&sender_rotation);

    let ack_cnt_rotation = Arc::new(AtomicU16::new(0));
    let ack_cnt_interval = Arc::clone(&ack_cnt_rotation);

    let ack_time_rotation = Arc::new(AtomicI64::new(NO_TIMESTAMP));
    let ack_time_interval = Arc::clone(&ack_time_rotation);

    let mut itv = time::interval(time::Duration::from_secs(ACK_INTERVAL_TIME));
    itv.reset();
    let ack_time_notify = Arc::new(Notify::new());
    let ack_time_notified = ack_time_notify.clone();

    #[cfg(feature = "benchmark")]
    let mut count = 0_usize;
    #[cfg(feature = "benchmark")]
    let mut size = 0_usize;
    #[cfg(feature = "benchmark")]
    let mut packet_size = 0_u64;
    #[cfg(feature = "benchmark")]
    let mut packet_count = 0_u64;
    #[cfg(feature = "benchmark")]
    let mut start = std::time::Instant::now();

    let handler = task::spawn(async move {
        loop {
            select! {
                _ = itv.tick() => {
                    let last_timestamp = ack_time_interval.load(Ordering::SeqCst);
                    if last_timestamp !=  NO_TIMESTAMP {
                        if send_ack_timestamp(&mut (*sender_interval.lock().await),last_timestamp).await.is_err()
                        {
                            break;
                        }

                        ack_cnt_interval.store(0, Ordering::SeqCst);
                    }
                }

                () = ack_time_notified.notified() => {
                    itv.reset();
                }
            }
        }
    });
    loop {
        match receive_event(&mut recv).await {
            Ok((mut raw_event, timestamp)) => {
                if (timestamp == CHANNEL_CLOSE_TIMESTAMP)
                    && (raw_event.as_bytes() == CHANNEL_CLOSE_MESSAGE)
                {
                    send_ack_timestamp(&mut (*sender_rotation.lock().await), timestamp).await?;
                    continue;
                }
                let key_builder = StorageKey::builder().start_key(&source);
                let key_builder = match raw_event_kind {
                    RawEventKind::Log => {
                        let log = bincode::deserialize::<Log>(&raw_event)?;
                        key_builder
                            .mid_key(Some(log.kind.as_bytes().to_vec()))
                            .end_key(timestamp)
                    }
                    RawEventKind::PeriodicTimeSeries => {
                        let time_series = bincode::deserialize::<PeriodicTimeSeries>(&raw_event)?;
                        StorageKey::builder()
                            .start_key(&time_series.id)
                            .end_key(timestamp)
                    }
                    RawEventKind::OpLog => {
                        let op_log = bincode::deserialize::<OpLog>(&raw_event)?;
                        let agent_id = format!("{}@{source}", op_log.agent_name);
                        StorageKey::builder()
                            .start_key(&agent_id)
                            .end_key(timestamp)
                    }
                    RawEventKind::Packet => {
                        let packet = bincode::deserialize::<Packet>(&raw_event)?;
                        key_builder
                            .mid_key(Some(timestamp.to_be_bytes().to_vec()))
                            .end_key(packet.packet_timestamp)
                    }
                    RawEventKind::Statistics => {
                        let statistics = bincode::deserialize::<Statistics>(&raw_event)?;
                        #[cfg(feature = "benchmark")]
                        {
                            (packet_count, packet_size) = statistics
                                .stats
                                .iter()
                                .fold((0, 0), |(sumc, sums), c| (sumc + c.1, sums + c.2));
                        }
                        key_builder
                            .mid_key(Some(statistics.core.to_be_bytes().to_vec()))
                            .end_key(timestamp)
                    }
                    RawEventKind::SecuLog => {
                        let mut secu_log = bincode::deserialize::<SecuLog>(&raw_event)?;
                        secu_log.source = source.clone();
                        raw_event = bincode::serialize(&secu_log)?;
                        StorageKey::builder()
                            .start_key(&secu_log.kind)
                            .end_key(timestamp)
                    }
                    _ => key_builder.end_key(timestamp),
                };
                let storage_key = key_builder.build();
                store.append(&storage_key.key(), &raw_event)?;
                if let Some(network_key) = network_key.as_ref() {
                    send_direct_stream(
                        network_key,
                        &raw_event,
                        timestamp,
                        &source,
                        stream_direct_channel.clone(),
                    )
                    .await?;
                }
                ack_cnt_rotation.fetch_add(1, Ordering::SeqCst);
                ack_time_rotation.store(timestamp, Ordering::SeqCst);
                if ACK_ROTATION_CNT <= ack_cnt_rotation.load(Ordering::SeqCst) {
                    send_ack_timestamp(&mut (*sender_rotation.lock().await), timestamp).await?;
                    ack_cnt_rotation.store(0, Ordering::SeqCst);
                    ack_time_notify.notify_one();
                    store.flush()?;
                }
                #[cfg(feature = "benchmark")]
                {
                    if raw_event_kind == RawEventKind::Statistics {
                        count += usize::try_from(packet_count).unwrap_or_default();
                        size += usize::try_from(packet_size).unwrap_or_default();
                    } else {
                        count += 1;
                        size += raw_event.len();
                    }
                    if start.elapsed().as_secs() > 3600 {
                        info!(
                            "Ingest: source = {source} type = {raw_event_kind:?} count = {count} size = {size}, duration = {}",
                            start.elapsed().as_secs()
                        );
                        count = 0;
                        size = 0;
                        start = std::time::Instant::now();
                    }
                }

                if shutdown_signal.load(Ordering::SeqCst) {
                    store.flush()?;
                    handler.abort();
                    break;
                }
            }
            Err(RecvError::ReadError(quinn::ReadExactError::FinishedEarly)) => {
                handler.abort();
                break;
            }
            Err(e) => {
                store.flush()?;
                handler.abort();
                bail!("handle {:?} error: {}", raw_event_kind, e)
            }
        }
    }
    store.flush()?;

    Ok(())
}

/// Sends a cumulative acknowledgement message up to the given timestamp over the given send
/// stream.
///
/// # Errors
///
/// Returns a `SendError` if an error occurs while sending the acknowledgement.
async fn send_ack_timestamp(send: &mut SendStream, timestamp: i64) -> Result<(), SendError> {
    frame::send_bytes(send, &timestamp.to_be_bytes()).await?;
    Ok(())
}

async fn check_sources_conn(
    source_db: Database,
    packet_sources: PacketSources,
    sources: Sources,
    mut rx: Receiver<SourceInfo>,
    notify_source: Option<Arc<Notify>>,
) -> Result<()> {
    let mut itv = time::interval(time::Duration::from_secs(SOURCE_INTERVAL));
    itv.reset();
    let source_store = source_db
        .sources_store()
        .expect("Failed to open source store");
    loop {
        select! {
            _ = itv.tick() => {
                let mut sources = sources.write().await;
                let keys: Vec<String> = sources.keys().map(std::borrow::ToOwned::to_owned).collect();

                for source_key in keys {
                    let timestamp = Utc::now();
                    if source_store.insert(&source_key, timestamp).is_err(){
                        error!("Failed to append source store");
                    }
                    sources.insert(source_key, timestamp);
                }
            }

            Some((source_key,timestamp_val,conn_state, rep)) = rx.recv() => {
                match conn_state {
                    ConnState::Connected => {
                        if source_store.insert(&source_key, timestamp_val).is_err() {
                            error!("Failed to append source store");
                        }
                        if !rep {
                            sources.write().await.insert(source_key, timestamp_val);
                            if let Some(ref notify) = notify_source {
                                notify.notify_one();
                            }
                        }
                    }
                    ConnState::Disconnected => {
                        if source_store.insert(&source_key, timestamp_val).is_err() {
                            error!("Failed to append source store");
                        }
                        if !rep {
                            sources.write().await.remove(&source_key);
                            packet_sources.write().await.remove(&source_key);
                            if let Some(ref notify) = notify_source {
                                notify.notify_one();
                            }
                        }
                    }
                }
            }
        }
    }
}

pub struct NetworkKey {
    pub(crate) source_key: String,
    pub(crate) all_key: String,
}

impl NetworkKey {
    pub fn new(source: &str, protocol: &str) -> Self {
        let source_key = format!("{source}\0{protocol}");
        let all_key = format!("all\0{protocol}");

        Self {
            source_key,
            all_key,
        }
    }
}
