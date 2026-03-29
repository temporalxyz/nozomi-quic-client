//! Ultra-low-latency QUIC/H3 client for Nozomi Solana transaction submission.
//!
//! Optimized for HFT:
//! - Persistent QUIC connection with automatic reconnection
//! - Pre-resolved DNS cached across reconnects
//! - Keep-alive pings to maintain warm connection
//! - Binary length-prefixed batch wire format (no JSON overhead)
//! - Pre-parsed URI and headers (zero per-send parsing)
//! - No internal locking — single owner, caller controls concurrency
//! - No per-send timer registration — relies on QUIC transport timeouts
//!
//! # Hot path
//!
//! For minimum latency, pre-encode outside your hot loop and use
//! [`send_raw`](NozomiQuicClient::send_raw):
//!
//! ```no_run
//! use nozomi_quic_client::{NozomiQuicClient, NozomiConfig};
//! use bytes::Bytes;
//!
//! # async fn example() -> nozomi_quic_client::Result<()> {
//! let mut client = NozomiQuicClient::new(NozomiConfig::new(
//!     "pit1.nozomi.temporal.xyz",
//!     "your-api-key",
//! ))?;
//! client.warmup().await?;
//!
//! // Encode once (outside hot loop)
//! let tx = vec![1u8; 200];
//! let body = NozomiQuicClient::encode_batch(&[&tx])?;
//!
//! // Hot loop — send_raw is the fastest path
//! client.send_raw(body).await?;
//! # Ok(())
//! # }
//! ```

use bytes::Bytes;
use h3::client::SendRequest;
use h3_quinn::OpenStreams;
use http::HeaderValue;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Direct Nozomi endpoints (no Cloudflare proxy — lowest latency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NozomiEndpoint {
    /// Auto-routed via geo-DNS (`edge.nozomi.temporal.xyz`).
    Auto,
    /// Pittsburgh (`pit1.nozomi.temporal.xyz`).
    Pittsburgh,
    /// Tokyo (`tyo1.nozomi.temporal.xyz`).
    Tokyo,
    /// Singapore (`sgp1.nozomi.temporal.xyz`).
    Singapore,
    /// Newark (`ewr1.nozomi.temporal.xyz`).
    Newark,
    /// Amsterdam (`ams1.nozomi.temporal.xyz`).
    Amsterdam,
    /// Frankfurt (`fra2.nozomi.temporal.xyz`).
    Frankfurt,
    /// Ashburn (`ash1.nozomi.temporal.xyz`).
    Ashburn,
    /// Los Angeles (`lax1.nozomi.temporal.xyz`).
    LosAngeles,
    /// London (`lon1.nozomi.temporal.xyz`).
    London,
}

impl NozomiEndpoint {
    /// Hostname for this endpoint.
    pub const fn host(self) -> &'static str {
        match self {
            Self::Auto => "edge.nozomi.temporal.xyz",
            Self::Pittsburgh => "pit1.nozomi.temporal.xyz",
            Self::Tokyo => "tyo1.nozomi.temporal.xyz",
            Self::Singapore => "sgp1.nozomi.temporal.xyz",
            Self::Newark => "ewr1.nozomi.temporal.xyz",
            Self::Amsterdam => "ams1.nozomi.temporal.xyz",
            Self::Frankfurt => "fra2.nozomi.temporal.xyz",
            Self::Ashburn => "ash1.nozomi.temporal.xyz",
            Self::LosAngeles => "lax1.nozomi.temporal.xyz",
            Self::London => "lon1.nozomi.temporal.xyz",
        }
    }

    /// All direct endpoints (excludes Auto).
    pub const ALL_DIRECT: &[NozomiEndpoint] = &[
        Self::Pittsburgh,
        Self::Tokyo,
        Self::Singapore,
        Self::Newark,
        Self::Amsterdam,
        Self::Frankfurt,
        Self::Ashburn,
        Self::LosAngeles,
        Self::London,
    ];
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Client configuration.
pub struct NozomiConfig {
    /// Hostname without protocol (e.g. `"pit1.nozomi.temporal.xyz"`).
    pub host: String,
    /// Port. Defaults to `443`.
    pub port: u16,
    /// API key for authentication (passed as `?c=<key>` query parameter).
    pub api_key: String,
}

/// Maximum transactions per batch (server limit).
pub const MAX_BATCH_SIZE: usize = 16;
/// Maximum size of a single transaction in bytes.
const MAX_TX_SIZE: usize = 1232;
/// Maximum batch body size in bytes.
const MAX_BATCH_BODY_SIZE: usize = 19_744;

impl NozomiConfig {
    /// Create config with defaults (port 443).
    pub fn new(host: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 443,
            api_key: api_key.into(),
        }
    }

    /// Create config from a [`NozomiEndpoint`].
    pub fn from_endpoint(endpoint: NozomiEndpoint, api_key: impl Into<String>) -> Self {
        Self {
            host: endpoint.host().into(),
            port: 443,
            api_key: api_key.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Connection(String),
    Request(String),
    NotConnected,
    Batch(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "connection: {msg}"),
            Self::Request(msg) => write!(f, "request: {msg}"),
            Self::NotConnected => f.write_str("not connected"),
            Self::Batch(msg) => write!(f, "batch: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct CachedConnection {
    send_request: SendRequest<OpenStreams, Bytes>,
    _driver: JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// High-performance QUIC/H3 client for Nozomi transaction submission.
///
/// Single-owner — no internal locking. The caller controls concurrency.
pub struct NozomiQuicClient {
    endpoint: quinn::Endpoint,
    conn: Option<CachedConnection>,
    cached_addr: Option<SocketAddr>,

    // Immutable after construction.
    host: String,
    port: u16,
    batch_uri: http::Uri,
    /// Pre-built headers containing HOST + CONTENT_TYPE.
    /// Cloned per send (~40ns for 2 entries), then CONTENT_LENGTH inserted.
    static_headers: http::HeaderMap,
}

impl NozomiQuicClient {
    /// Create a new client. Does **not** connect yet — call [`warmup`](Self::warmup) first.
    pub fn new(config: NozomiConfig) -> Result<Self> {
        // Install the rustls crypto provider (idempotent).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // --- TLS ---
        let mut roots = rustls::RootCertStore::empty();
        let native_certs = rustls_native_certs::load_native_certs();
        for cert in native_certs.certs {
            roots.add(cert).ok();
        }

        let mut tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        // --- QUIC ---
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|e| Error::Connection(format!("QUIC TLS config: {e}")))?;

        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(Duration::from_secs(120).try_into().unwrap()));
        transport.keep_alive_interval(Some(Duration::from_secs(15)));

        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| Error::Connection(format!("endpoint bind: {e}")))?;
        endpoint.set_default_client_config(client_config);

        // Pre-parse URI and host header once — used on every send.
        let batch_uri: http::Uri = format!("/api/sendBatch?c={}", config.api_key)
            .parse()
            .map_err(|e: http::uri::InvalidUri| {
                Error::Connection(format!("invalid batch URI: {e}"))
            })?;

        let host_header = if config.port == 443 {
            HeaderValue::from_str(&config.host)
        } else {
            HeaderValue::from_str(&format!("{}:{}", config.host, config.port))
        }
        .map_err(|e| Error::Connection(format!("invalid host header: {e}")))?;

        // Pre-build the static headers. Per-send we clone this (~40ns for 2
        // entries) and insert only CONTENT_LENGTH — no builder overhead.
        let mut static_headers = http::HeaderMap::with_capacity(3);
        static_headers.insert(http::header::HOST, host_header);
        static_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );

        Ok(Self {
            endpoint,
            conn: None,
            cached_addr: None,
            host: config.host,
            port: config.port,
            batch_uri,
            static_headers,
        })
    }

    /// Resolve DNS and pre-establish the QUIC/H3 connection.
    ///
    /// Call this **once** before you start trading so the first
    /// `send_batch` doesn't pay the handshake cost.
    pub async fn warmup(&mut self) -> Result<()> {
        self.resolve_dns().await?;
        self.connect().await
    }

    /// Encode transactions into the batch wire format.
    ///
    /// Call this outside your hot loop, then pass the result to
    /// [`send_raw`](Self::send_raw) for minimum-latency sending.
    pub fn encode_batch(transactions: &[&[u8]]) -> Result<Bytes> {
        if transactions.is_empty() {
            return Err(Error::Batch("empty batch".into()));
        }
        if transactions.len() > MAX_BATCH_SIZE {
            return Err(Error::Batch(format!(
                "too many transactions: {} (max {MAX_BATCH_SIZE})",
                transactions.len()
            )));
        }

        let body_len: usize = transactions.iter().map(|tx| 2 + tx.len()).sum();
        if body_len > MAX_BATCH_BODY_SIZE {
            return Err(Error::Batch(format!(
                "batch body too large: {body_len} bytes (max {MAX_BATCH_BODY_SIZE})"
            )));
        }

        let mut body = Vec::with_capacity(body_len);
        for tx in transactions {
            if tx.len() > MAX_TX_SIZE {
                return Err(Error::Batch(format!(
                    "transaction too large: {} bytes (max {MAX_TX_SIZE})",
                    tx.len()
                )));
            }
            body.extend_from_slice(&(tx.len() as u16).to_be_bytes());
            body.extend_from_slice(tx);
        }
        Ok(Bytes::from(body))
    }

    /// Send a pre-encoded batch body. **This is the fastest send path.**
    ///
    /// Use [`encode_batch`](Self::encode_batch) to produce the `Bytes` body.
    /// On connection failure, automatically reconnects and retries once.
    pub async fn send_raw(&mut self, body: Bytes) -> Result<()> {
        match self.try_send(body.clone()).await {
            Ok(()) => Ok(()),
            Err(_) => {
                self.invalidate();
                self.connect().await?;
                self.try_send(body).await
            }
        }
    }

    /// Encode and send a batch of raw serialized transactions.
    ///
    /// Convenience wrapper around [`encode_batch`](Self::encode_batch) +
    /// [`send_raw`](Self::send_raw). For maximum performance in a hot loop,
    /// pre-encode and call `send_raw` directly.
    pub async fn send_batch(&mut self, transactions: &[&[u8]]) -> Result<()> {
        self.send_raw(Self::encode_batch(transactions)?).await
    }

    /// Returns `true` if there is an active QUIC connection.
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Close the current connection (if any). A subsequent send will
    /// re-establish it automatically.
    pub fn disconnect(&mut self) {
        self.invalidate();
    }

    // -----------------------------------------------------------------------
    // Private
    // -----------------------------------------------------------------------

    async fn resolve_dns(&mut self) -> Result<SocketAddr> {
        let key = format!("{}:{}", self.host, self.port);
        let addr = tokio::net::lookup_host(&key)
            .await
            .map_err(|e| Error::Connection(format!("DNS lookup {key}: {e}")))?
            .next()
            .ok_or_else(|| Error::Connection(format!("no addresses for {key}")))?;
        self.cached_addr = Some(addr);
        Ok(addr)
    }

    async fn connect(&mut self) -> Result<()> {
        let addr = match self.cached_addr {
            Some(a) => a,
            None => self.resolve_dns().await?,
        };

        let quic_conn = self
            .endpoint
            .connect(addr, &self.host)
            .map_err(|e| Error::Connection(format!("QUIC connect: {e}")))?
            .await
            .map_err(|e| Error::Connection(format!("QUIC handshake: {e}")))?;

        let h3_conn = h3_quinn::Connection::new(quic_conn);
        let (mut driver, send_request) = h3::client::new(h3_conn)
            .await
            .map_err(|e| Error::Connection(format!("H3 handshake: {e}")))?;

        let driver_handle = tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        self.conn = Some(CachedConnection {
            send_request,
            _driver: driver_handle,
        });

        Ok(())
    }

    fn invalidate(&mut self) {
        self.conn = None;
        self.cached_addr = None;
    }

    #[inline]
    async fn try_send(&mut self, body: Bytes) -> Result<()> {
        let sender = self.conn.as_mut().ok_or(Error::NotConnected)?;

        // Clone pre-built headers (~40ns), insert only the per-request field.
        let mut headers = self.static_headers.clone();
        headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from(body.len() as u64),
        );

        // Construct request directly from parts — no builder overhead.
        let (mut parts, _) = http::Request::new(()).into_parts();
        parts.method = http::Method::POST;
        parts.uri = self.batch_uri.clone();
        parts.headers = headers;
        let req = http::Request::from_parts(parts, ());

        let mut stream = sender
            .send_request
            .send_request(req)
            .await
            .map_err(|e| Error::Request(format!("send headers: {e}")))?;

        stream
            .send_data(body)
            .await
            .map_err(|e| Error::Request(format!("send body: {e}")))?;

        stream
            .finish()
            .await
            .map_err(|e| Error::Request(format!("finish stream: {e}")))?;

        Ok(())
    }
}
