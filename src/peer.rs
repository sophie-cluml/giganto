#![allow(clippy::module_name_repetitions)]

use crate::{
    graphql::status::{insert_toml_peers, read_toml_file, write_toml_file, TomlPeers},
    ingest::Sources,
    server::{
        certificate_info, config_client, config_server, extract_cert_from_conn,
        SERVER_CONNNECTION_DELAY, SERVER_ENDPOINT_DELAY,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use giganto_client::{
    connection::{client_handshake, server_handshake},
    frame::{self, recv_bytes, recv_raw, send_bytes},
};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use quinn::{
    ClientConfig, Connection, ConnectionError, Endpoint, RecvStream, SendStream, ServerConfig,
};
use rustls::{Certificate, PrivateKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    mem,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    select,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Notify, RwLock,
    },
    time::sleep,
};
use toml_edit::Document;
use tracing::{error, info, warn};

const PEER_VERSION_REQ: &str = ">=0.12.0,<0.16.0";
const PEER_RETRY_INTERVAL: u64 = 5;

pub type PeerSources = Arc<RwLock<HashMap<String, HashSet<String>>>>;

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, IntoPrimitive, PartialEq, Serialize, TryFromPrimitive,
)]
#[repr(u32)]
#[non_exhaustive]
pub enum PeerCode {
    UpdatePeerList = 0,
    UpdateSourceList = 1,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub address: SocketAddr,
    pub host_name: String,
}

impl TomlPeers for PeerInfo {
    fn get_host_name(&self) -> String {
        self.host_name.clone()
    }
    fn get_address(&self) -> String {
        self.address.to_string()
    }
}

#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Debug)]
pub struct PeerConnInfo {
    peer_conn: Arc<RwLock<HashMap<String, Connection>>>, //key: hostname, value: connection
    peer_list: Arc<RwLock<HashSet<PeerInfo>>>,
    sources: Sources,
    peer_sources: PeerSources, //key: address(for request graphql/publish), value: peer's collect sources(hash set)
    peer_sender: Sender<PeerInfo>,
    local_address: SocketAddr,
    notify_source: Arc<Notify>,
    config_doc: Document,
    config_path: String,
}

pub struct Peer {
    client_config: ClientConfig,
    server_config: ServerConfig,
    local_address: SocketAddr,
    local_host_name: String,
}

impl Peer {
    pub fn new(
        local_address: SocketAddr,
        certs: Vec<Certificate>,
        key: PrivateKey,
        files: Vec<Vec<u8>>,
    ) -> Result<Self> {
        let (_, local_host_name) = certificate_info(&certs)?;

        let server_config = config_server(certs.clone(), key.clone(), files.clone())
            .expect("server configuration error with cert, key or root");

        let client_config = config_client(certs, key, files)
            .expect("client configuration error with cert, key or root");

        Ok(Peer {
            client_config,
            server_config,
            local_address,
            local_host_name,
        })
    }

    pub async fn run(
        self,
        peers: HashSet<PeerInfo>,
        sources: Sources,
        peer_sources: PeerSources,
        notify_source: Arc<Notify>,
        wait_shutdown: Arc<Notify>,
        config_path: String,
    ) -> Result<()> {
        let server_endpoint =
            Endpoint::server(self.server_config, self.local_address).expect("endpoint");
        info!(
            "listening on {}",
            server_endpoint
                .local_addr()
                .expect("for local addr display")
        );

        let client_socket = SocketAddr::new(self.local_address.ip(), 0);
        let client_endpoint = {
            let mut e = Endpoint::client(client_socket).expect("endpoint");
            e.set_default_client_config(self.client_config);
            e
        };

        let (sender, mut receiver): (Sender<PeerInfo>, Receiver<PeerInfo>) = channel(100);

        let Ok(config_doc) = read_toml_file(&config_path) else {
            bail!("Failed to open/read config's toml file");
        };

        // A structure of values common to peer connections.
        let peer_conn_info = PeerConnInfo {
            peer_conn: Arc::new(RwLock::new(HashMap::new())),
            peer_list: Arc::new(RwLock::new(peers)),
            peer_sources,
            sources,
            peer_sender: sender,
            local_address: self.local_address,
            notify_source,
            config_doc,
            config_path,
        };

        tokio::spawn(client_run(
            client_endpoint.clone(),
            peer_conn_info.clone(),
            self.local_host_name.clone(),
            wait_shutdown.clone(),
        ));

        loop {
            select! {
                Some(conn) = server_endpoint.accept()  => {
                    let peer_conn_info = peer_conn_info.clone();
                    let wait_shutdown = wait_shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server_connection(
                            conn,
                            peer_conn_info,
                            wait_shutdown,
                        )
                        .await
                        {
                            error!("connection failed: {}", e);
                        }
                    });
                },
                Some(peer) = receiver.recv()  => {
                    tokio::spawn(client_connection(
                        client_endpoint.clone(),
                        peer,
                        peer_conn_info.clone(),
                        self.local_host_name.clone(),
                        wait_shutdown.clone(),
                    ));
                },
                () = wait_shutdown.notified() => {
                    sleep(Duration::from_millis(SERVER_ENDPOINT_DELAY)).await;      // Wait time for connection to be ready for shutdown.
                    server_endpoint.close(0_u32.into(), &[]);
                    info!("Shutting down peer");
                    return Ok(())
                }

            }
        }
    }
}

async fn client_run(
    client_endpoint: Endpoint,
    peer_conn_info: PeerConnInfo,
    local_host_name: String,
    wait_shutdown: Arc<Notify>,
) {
    for peer in &*peer_conn_info.peer_list.read().await {
        tokio::spawn(client_connection(
            client_endpoint.clone(),
            peer.clone(),
            peer_conn_info.clone(),
            local_host_name.clone(),
            wait_shutdown.clone(),
        ));
    }
}

async fn connect(
    client_endpoint: &Endpoint,
    peer_info: &PeerInfo,
) -> Result<(Connection, SendStream, RecvStream)> {
    let connection = client_endpoint
        .connect(peer_info.address, &peer_info.host_name)?
        .await?;
    let (send, recv) = client_handshake(&connection, PEER_VERSION_REQ).await?;
    Ok((connection, send, recv))
}

#[allow(clippy::too_many_lines)]
async fn client_connection(
    client_endpoint: Endpoint,
    peer_info: PeerInfo,
    peer_conn_info: PeerConnInfo,
    local_host_name: String,
    wait_shutdown: Arc<Notify>,
) -> Result<()> {
    'connection: loop {
        match connect(&client_endpoint, &peer_info).await {
            Ok((connection, mut send, mut recv)) => {
                // Remove duplicate connections.
                let (remote_addr, remote_host_name) = match check_for_duplicate_connections(
                    &connection,
                    peer_conn_info.peer_conn.clone(),
                )
                .await
                {
                    Ok((addr, name)) => {
                        info!("Connection established to {}/{} (client role)", addr, name);
                        (addr, name)
                    }
                    Err(_) => {
                        return Ok(());
                    }
                };

                let send_source_list: HashSet<String> = peer_conn_info
                    .sources
                    .read()
                    .await
                    .keys()
                    .cloned()
                    .collect();

                // Add my peer info to the peer list.
                let mut send_peer_list = peer_conn_info.peer_list.read().await.clone();
                send_peer_list.insert(PeerInfo {
                    address: peer_conn_info.local_address,
                    host_name: local_host_name.clone(),
                });

                // Exchange peer list/source list.
                let (recv_peer_list, recv_source_list) =
                    request_init_info::<(HashSet<PeerInfo>, HashSet<String>)>(
                        &mut send,
                        &mut recv,
                        PeerCode::UpdatePeerList,
                        (send_peer_list, send_source_list),
                    )
                    .await?;

                // Update to the list of received sources.
                update_to_new_source_list(
                    recv_source_list,
                    remote_addr.clone(),
                    peer_conn_info.peer_sources.clone(),
                )
                .await;

                // Update to the list of received peers.
                update_to_new_peer_list(
                    recv_peer_list,
                    peer_conn_info.local_address,
                    peer_conn_info.peer_list.clone(),
                    peer_conn_info.peer_sender.clone(),
                    peer_conn_info.config_doc.clone(),
                    &peer_conn_info.config_path,
                )
                .await?;

                // Share the received peer list with connected peers.
                for conn in (*peer_conn_info.peer_conn.read().await).values() {
                    tokio::spawn(update_peer_info::<HashSet<PeerInfo>>(
                        conn.clone(),
                        PeerCode::UpdatePeerList,
                        peer_conn_info.peer_list.read().await.clone(),
                    ));
                }

                // Update my peer list
                peer_conn_info
                    .peer_conn
                    .write()
                    .await
                    .insert(remote_host_name.clone(), connection.clone());

                loop {
                    select! {
                        stream = connection.accept_bi()  => {
                            let stream = match stream {
                                Err(e) => {
                                    peer_conn_info.peer_conn.write().await.remove(&remote_host_name);
                                    peer_conn_info.peer_sources.write().await.remove(&remote_addr);
                                    if let quinn::ConnectionError::ApplicationClosed(_) = e {
                                        info!("giganto peer({}/{}) closed",remote_host_name, remote_addr);
                                        return Ok(());
                                    }
                                    continue 'connection;
                                }
                                Ok(s) => s,
                            };

                            let peer_list = peer_conn_info.peer_list.clone();
                            let sender = peer_conn_info.peer_sender.clone();
                            let remote_addr =remote_addr.clone();
                            let peer_sources = peer_conn_info.peer_sources.clone();
                            let doc = peer_conn_info.config_doc.clone();
                            let path= peer_conn_info.config_path.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_request(stream,peer_conn_info.local_address,remote_addr,peer_list,peer_sources,sender,doc,path).await {
                                    error!("failed: {}", e);
                                }
                            });
                        },
                        () = peer_conn_info.notify_source.notified() => {
                            let source_list: HashSet<String> = peer_conn_info.sources.read().await.keys().cloned().collect();
                            for conn in (*peer_conn_info.peer_conn.write().await).values() {
                                tokio::spawn(update_peer_info::<HashSet<String>>(
                                    conn.clone(),
                                    PeerCode::UpdateSourceList,
                                    source_list.clone(),
                                ));
                            }
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
            Err(e) => {
                if let Some(e) = e.downcast_ref::<ConnectionError>() {
                    match e {
                        ConnectionError::ConnectionClosed(_)
                        | ConnectionError::ApplicationClosed(_)
                        | ConnectionError::Reset
                        | ConnectionError::TimedOut => {
                            warn!(
                                "Retry connection to {} after {} seconds.",
                                peer_info.address, PEER_RETRY_INTERVAL,
                            );
                            sleep(Duration::from_secs(PEER_RETRY_INTERVAL)).await;
                            continue 'connection;
                        }
                        _ => {}
                    }
                } else {
                    return Ok(());
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn server_connection(
    conn: quinn::Connecting,
    peer_conn_info: PeerConnInfo,
    wait_shutdown: Arc<Notify>,
) -> Result<()> {
    let connection = conn.await?;

    let (mut send, mut recv) = match server_handshake(&connection, PEER_VERSION_REQ).await {
        Ok((send, recv)) => (send, recv),
        Err(e) => {
            connection.close(quinn::VarInt::from_u32(0), e.to_string().as_bytes());
            bail!("{e}")
        }
    };

    // Remove duplicate connections.
    let (remote_addr, remote_host_name) = match check_for_duplicate_connections(
        &connection,
        peer_conn_info.peer_conn.clone(),
    )
    .await
    {
        Ok((addr, name)) => {
            info!("Connection established to {}/{} (server role)", addr, name);
            (addr, name)
        }
        Err(_) => {
            return Ok(());
        }
    };

    let source_list: HashSet<String> = peer_conn_info
        .sources
        .read()
        .await
        .keys()
        .cloned()
        .collect();

    // Exchange peer list/source list.
    let (recv_peer_list, recv_source_list) =
        response_init_info::<(HashSet<PeerInfo>, HashSet<String>)>(
            &mut send,
            &mut recv,
            PeerCode::UpdatePeerList,
            (peer_conn_info.peer_list.read().await.clone(), source_list),
        )
        .await?;

    // Update to the list of received sources.
    update_to_new_source_list(
        recv_source_list.clone(),
        remote_addr.clone(),
        peer_conn_info.peer_sources.clone(),
    )
    .await;

    // Update to the list of received peers.
    update_to_new_peer_list(
        recv_peer_list.clone(),
        peer_conn_info.local_address,
        peer_conn_info.peer_list.clone(),
        peer_conn_info.peer_sender.clone(),
        peer_conn_info.config_doc.clone(),
        &peer_conn_info.config_path,
    )
    .await?;

    // Share the received peer list with your connected peers.
    for conn in (*peer_conn_info.peer_conn.read().await).values() {
        tokio::spawn(update_peer_info::<HashSet<PeerInfo>>(
            conn.clone(),
            PeerCode::UpdatePeerList,
            peer_conn_info.peer_list.read().await.clone(),
        ));
    }

    // Update my peer list
    peer_conn_info
        .peer_conn
        .write()
        .await
        .insert(remote_host_name.clone(), connection.clone());

    loop {
        select! {
            stream = connection.accept_bi()  => {
                let stream = match stream {
                    Err(e) => {
                        peer_conn_info.peer_conn.write().await.remove(&remote_host_name);
                        peer_conn_info.peer_sources.write().await.remove(&remote_addr);
                        if let quinn::ConnectionError::ApplicationClosed(_) = e {
                            info!("giganto peer({}/{}) closed",remote_host_name, remote_addr);
                            return Ok(());
                        }
                        return Err(e.into());
                    }
                    Ok(s) => s,
                };

                let peer_list = peer_conn_info.peer_list.clone();
                let sender = peer_conn_info.peer_sender.clone();
                let remote_addr =remote_addr.clone();
                let peer_sources = peer_conn_info.peer_sources.clone();
                let doc = peer_conn_info.config_doc.clone();
                let path= peer_conn_info.config_path.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_request(stream,peer_conn_info.local_address,remote_addr,peer_list,peer_sources,sender,doc,path).await {
                        error!("failed: {}", e);
                    }
                });
            },
            () = peer_conn_info.notify_source.notified() => {
                let source_list: HashSet<String> = peer_conn_info.sources.read().await.keys().cloned().collect();
                for conn in (*peer_conn_info.peer_conn.read().await).values() {
                    tokio::spawn(update_peer_info::<HashSet<String>>(
                        conn.clone(),
                        PeerCode::UpdateSourceList,
                        source_list.clone(),
                    ));
                }
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

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    (_, mut recv): (SendStream, RecvStream),
    local_addr: SocketAddr,
    remote_addr: String,
    peer_list: Arc<RwLock<HashSet<PeerInfo>>>,
    peer_sources: PeerSources,
    sender: Sender<PeerInfo>,
    doc: Document,
    path: String,
) -> Result<()> {
    let (msg_type, msg_buf) = receive_peer_data(&mut recv).await?;
    match msg_type {
        PeerCode::UpdatePeerList => {
            let update_peer_list = bincode::deserialize::<HashSet<PeerInfo>>(&msg_buf)
                .map_err(|e| anyhow!("Failed to deserialize peer list: {}", e))?;
            update_to_new_peer_list(update_peer_list, local_addr, peer_list, sender, doc, &path)
                .await?;
        }
        PeerCode::UpdateSourceList => {
            let update_source_list = bincode::deserialize::<HashSet<String>>(&msg_buf)
                .map_err(|e| anyhow!("Failed to deserialize source list: {}", e))?;
            update_to_new_source_list(update_source_list, remote_addr, peer_sources).await;
        }
    }
    Ok(())
}

pub async fn send_peer_data<T>(send: &mut SendStream, msg: PeerCode, update_data: T) -> Result<()>
where
    T: Serialize,
{
    // send PeerCode
    let msg_type: u32 = msg.into();
    send_bytes(send, &msg_type.to_le_bytes()).await?;

    // send the peer data to be updated
    let mut buf = Vec::new();
    frame::send(send, &mut buf, update_data).await?;
    Ok(())
}

pub async fn receive_peer_data(recv: &mut RecvStream) -> Result<(PeerCode, Vec<u8>)> {
    // receive PeerCode
    let mut buf = [0; mem::size_of::<u32>()];
    recv_bytes(recv, &mut buf).await?;
    let msg_type = PeerCode::try_from(u32::from_le_bytes(buf)).context("unknown peer code")?;

    // receive the peer data to be updated
    let mut buf = Vec::new();
    recv_raw(recv, &mut buf).await?;
    Ok((msg_type, buf))
}

async fn request_init_info<T>(
    send: &mut SendStream,
    recv: &mut RecvStream,
    init_type: PeerCode,
    init_data: T,
) -> Result<T>
where
    T: Serialize + DeserializeOwned,
{
    send_peer_data::<T>(send, init_type, init_data).await?;
    let (_, recv_data) = receive_peer_data(recv).await?;
    let recv_init_data = bincode::deserialize::<T>(&recv_data)?;
    Ok(recv_init_data)
}

async fn response_init_info<T>(
    send: &mut SendStream,
    recv: &mut RecvStream,
    init_type: PeerCode,
    init_data: T,
) -> Result<T>
where
    T: Serialize + DeserializeOwned,
{
    let (_, recv_data) = receive_peer_data(recv).await?;
    let recv_init_data = bincode::deserialize::<T>(&recv_data)?;
    send_peer_data::<T>(send, init_type, init_data).await?;
    Ok(recv_init_data)
}

async fn update_peer_info<T>(connection: Connection, msg_type: PeerCode, peer_data: T) -> Result<()>
where
    T: Serialize + DeserializeOwned,
{
    match connection.open_bi().await {
        Ok((mut send, _)) => {
            send_peer_data::<T>(&mut send, msg_type, peer_data).await?;
            Ok(())
        }
        Err(_) => {
            bail!("Failed to send peer data");
        }
    }
}

async fn check_for_duplicate_connections(
    connection: &Connection,
    peer_conn: Arc<RwLock<HashMap<String, Connection>>>,
) -> Result<(String, String)> {
    let remote_addr = connection.remote_address().ip().to_string();
    let (_, remote_host_name) = certificate_info(&extract_cert_from_conn(connection)?)?;
    if peer_conn.read().await.contains_key(&remote_host_name) {
        connection.close(
            quinn::VarInt::from_u32(0),
            "exist connection close".as_bytes(),
        );
        bail!("Duplicated connection close:{:?}", remote_host_name);
    }
    Ok((remote_addr, remote_host_name))
}

async fn update_to_new_peer_list(
    recv_peer_list: HashSet<PeerInfo>,
    local_address: SocketAddr,
    peer_list: Arc<RwLock<HashSet<PeerInfo>>>,
    sender: Sender<PeerInfo>,
    mut doc: Document,
    path: &str,
) -> Result<()> {
    let mut is_change = false;
    for recv_peer_info in recv_peer_list {
        if local_address.ip() != recv_peer_info.address.ip()
            && !peer_list.read().await.contains(&recv_peer_info)
        {
            is_change = true;
            peer_list.write().await.insert(recv_peer_info.clone());
            sender.send(recv_peer_info).await?;
        }
    }

    if is_change {
        let data: Vec<PeerInfo> = peer_list.read().await.iter().cloned().collect();
        if let Err(e) = insert_toml_peers(&mut doc, Some(data)) {
            error!("{e:?}");
        }
        if let Err(e) = write_toml_file(&doc, path) {
            error!("{e:?}");
        }
    }

    Ok(())
}

async fn update_to_new_source_list(
    recv_source_list: HashSet<String>,
    remote_addr: String,
    peer_sources: Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    peer_sources
        .write()
        .await
        .insert(remote_addr, recv_source_list);
}

#[cfg(test)]
mod tests {
    use super::Peer;
    use crate::{
        peer::{receive_peer_data, request_init_info, PeerCode, PeerInfo},
        to_cert_chain, to_private_key,
    };
    use chrono::Utc;
    use giganto_client::connection::client_handshake;
    use quinn::{Connection, Endpoint, RecvStream, SendStream};
    use std::{
        collections::{HashMap, HashSet},
        fs::{self, File},
        net::{IpAddr, Ipv6Addr, SocketAddr},
        path::Path,
        sync::{Arc, OnceLock},
    };
    use tempfile::TempDir;
    use tokio::sync::{Mutex, Notify, RwLock};

    fn get_token() -> &'static Mutex<u32> {
        static TOKEN: OnceLock<Mutex<u32>> = OnceLock::new();

        TOKEN.get_or_init(|| Mutex::new(0))
    }

    const CERT_PATH: &str = "tests/cert.pem";
    const KEY_PATH: &str = "tests/key.pem";
    const CA_CERT_PATH: &str = "tests/root.pem";
    const HOST: &str = "localhost";
    const TEST_PORT: u16 = 60191;
    const PROTOCOL_VERSION: &str = "0.14.0";

    struct TestClient {
        send: SendStream,
        recv: RecvStream,
        conn: Connection,
    }

    impl TestClient {
        async fn new() -> Self {
            let endpoint = init_client();
            let conn = endpoint
            .connect(
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), TEST_PORT),
                HOST,
            )
            .expect(
                "Failed to connect server's endpoint, Please check if the setting value is correct",
            )
            .await
            .expect("Failed to connect server's endpoint, Please make sure the Server is alive");
            let (send, recv) = client_handshake(&conn, PROTOCOL_VERSION).await.unwrap();
            Self { send, recv, conn }
        }
    }

    fn init_client() -> Endpoint {
        let (cert, key) = match fs::read(CERT_PATH)
            .map(|x| (x, fs::read(KEY_PATH).expect("Failed to Read key file")))
        {
            Ok(x) => x,
            Err(_) => {
                panic!(
                "failed to read (cert, key) file, {}, {} read file error. Cert or key doesn't exist in default test folder",
                CERT_PATH,
                KEY_PATH,
            );
            }
        };

        let pv_key = if Path::new(KEY_PATH)
            .extension()
            .map_or(false, |x| x == "der")
        {
            rustls::PrivateKey(key)
        } else {
            let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
                .expect("malformed PKCS #8 private key");
            match pkcs8.into_iter().next() {
                Some(x) => rustls::PrivateKey(x),
                None => {
                    let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                        .expect("malformed PKCS #1 private key");
                    match rsa.into_iter().next() {
                        Some(x) => rustls::PrivateKey(x),
                        None => {
                            panic!(
                            "no private keys found. Private key doesn't exist in default test folder"
                        );
                        }
                    }
                }
            }
        };
        let cert_chain = if Path::new(CERT_PATH)
            .extension()
            .map_or(false, |x| x == "der")
        {
            vec![rustls::Certificate(cert)]
        } else {
            rustls_pemfile::certs(&mut &*cert)
                .expect("invalid PEM-encoded certificate")
                .into_iter()
                .map(rustls::Certificate)
                .collect()
        };

        let mut server_root = rustls::RootCertStore::empty();
        let file = fs::read(CA_CERT_PATH).expect("Failed to read file");
        let root_cert: Vec<rustls::Certificate> = rustls_pemfile::certs(&mut &*file)
            .expect("invalid PEM-encoded certificate")
            .into_iter()
            .map(rustls::Certificate)
            .collect();

        if let Some(cert) = root_cert.get(0) {
            server_root.add(cert).expect("Failed to add cert");
        }

        let client_crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(server_root)
            .with_client_auth_cert(cert_chain, pv_key)
            .expect("the server root, cert chain or private key are not valid");

        let mut endpoint =
            quinn::Endpoint::client("[::]:0".parse().expect("Failed to parse Endpoint addr"))
                .expect("Failed to create endpoint");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_crypto)));
        endpoint
    }

    fn peer_init() -> Peer {
        let cert_pem = fs::read(CERT_PATH).unwrap();
        let cert = to_cert_chain(&cert_pem).unwrap();
        let key_pem = fs::read(KEY_PATH).unwrap();
        let key = to_private_key(&key_pem).unwrap();
        let ca_cert = fs::read("tests/root.pem").unwrap();

        Peer::new(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), TEST_PORT),
            cert,
            key,
            vec![ca_cert],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn recv_peer_data() {
        let _lock = get_token().lock().await;

        // peer server's peer list
        let peer_addr = SocketAddr::new("123.123.123.123".parse::<IpAddr>().unwrap(), TEST_PORT);
        let peer_name = String::from("einsis_peer");
        let mut peers = HashSet::new();
        peers.insert(PeerInfo {
            address: peer_addr,
            host_name: peer_name.clone(),
        });

        // peer server's source list
        let source_name = String::from("einsis_source");
        let mut source_info = HashMap::new();
        source_info.insert(source_name.clone(), Utc::now());

        let sources = Arc::new(RwLock::new(source_info));
        let peer_sources = Arc::new(RwLock::new(HashMap::new()));
        let notify_source = Arc::new(Notify::new());

        // create temp config file
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("config.toml");
        File::create(&file_path).unwrap();

        // run peer
        tokio::spawn(peer_init().run(
            peers,
            sources.clone(),
            peer_sources,
            notify_source.clone(),
            Arc::new(Notify::new()),
            file_path.to_str().unwrap().to_string(),
        ));

        // run peer client
        let mut peer_client_one = TestClient::new().await;
        let (recv_peer_list, recv_source_list) =
            request_init_info::<(HashSet<PeerInfo>, HashSet<String>)>(
                &mut peer_client_one.send,
                &mut peer_client_one.recv,
                PeerCode::UpdatePeerList,
                (HashSet::new(), HashSet::new()),
            )
            .await
            .unwrap();

        // compare server's peer list/source list
        assert!(recv_peer_list.contains(&PeerInfo {
            address: peer_addr,
            host_name: peer_name,
        }));
        assert!(recv_source_list.contains(&source_name));

        // insert peer server's source value & notify to server
        let source_name2 = String::from("einsis_source2");
        sources
            .write()
            .await
            .insert(source_name2.clone(), Utc::now());
        notify_source.notify_one();

        // receive source list
        let (_, mut recv_pub_resp) = peer_client_one
            .conn
            .accept_bi()
            .await
            .expect("failed to open stream");
        let (msg_type, msg_buf) = receive_peer_data(&mut recv_pub_resp).await.unwrap();
        let update_source_list = bincode::deserialize::<HashSet<String>>(&msg_buf).unwrap();

        // compare server's source list
        assert_eq!(msg_type, PeerCode::UpdateSourceList);
        assert!(update_source_list.contains(&source_name));
        assert!(update_source_list.contains(&source_name2));
    }
}
