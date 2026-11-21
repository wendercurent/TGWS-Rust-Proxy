use crate::config::*;
use crate::crypto::xor_mask_in_place;
use crate::{ldebug};
use base64::Engine;
use byteorder::{BigEndian, ByteOrder};
use rand::RngCore;
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::ServerName;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

// ---------------------------------------------------------------------------
// WS opcodes — proxy/raw_websocket.py
// ---------------------------------------------------------------------------

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

pub const MAX_MESSAGE_LEN: u64 = 16 * 1024 * 1024;
const MAX_FRAME_PAYLOAD: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// TLS: Mozilla CA roots plus hostname verification against the SNI.
// Fronting uses the same verifier; its SNI is the front name, so that
// certificate must match. Plaintext WS/80 is never an implicit fallback.
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;

fn verified_tls_config() -> ClientConfig {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.resumption = rustls::client::Resumption::in_memory_sessions(100);
    cfg
}

static TLS_CONFIG: Lazy<Arc<ClientConfig>> = Lazy::new(|| Arc::new(verified_tls_config()));

#[cfg(test)]
static TLS_CONFIG_12: Lazy<Arc<ClientConfig>> = Lazy::new(|| {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_root_certificates(roots)
        .with_no_client_auth())
});

#[derive(Clone, Copy, Debug)]
pub(crate) enum WsDialProfile {
    Standard,
    Preconnect,
    #[cfg(test)]
    SplitClientHello,
    #[cfg(test)]
    Tls12,
}

// Same verifier and session cache as direct WSS. Fronting still checks the
// certificate against its own SNI.
static TLS_CONFIG_FRONTING: Lazy<Arc<ClientConfig>> = Lazy::new(|| TLS_CONFIG.clone());

#[cfg(test)]
pub(crate) static LOOPBACK_FIXTURE: parking_lot::Mutex<Option<(u16, Arc<ClientConfig>)>> =
    parking_lot::const_mutex(None);

// ---------------------------------------------------------------------------
// WsHandshakeError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WsHandshakeError {
    pub status_code: i32,
    pub status_line: String,
    pub headers: HashMap<String, String>,
    pub location: String,
}

impl WsHandshakeError {
    pub fn is_redirect(&self) -> bool {
        matches!(self.status_code, 301 | 302 | 303 | 307 | 308)
    }
}

#[derive(Debug)]
pub enum WsError {
    Io(std::io::Error),
    Handshake(WsHandshakeError),
    Timeout,
    Dns(std::io::Error),
    Tcp(std::io::Error),
    Tls(std::io::Error),
    Upgrade(Box<WsError>),
    Busy,
    Canceled,
    NetworkChanged,
    Closed(Option<u16>),
    Other(String),
}

impl WsError {
    pub fn compact(&self) -> String {
        match self {
            WsError::Busy => "dial_busy".to_string(),
            WsError::Closed(code) => format!("ws_close {code:?}"),
            WsError::Canceled => "canceled".to_string(),
            WsError::NetworkChanged => "network_changed".to_string(),
            WsError::Timeout => "timeout".to_string(),
            WsError::Dns(error) => format!("dns {}", error.kind()),
            WsError::Handshake(h) => format!("http {}", h.status_code),
            WsError::Tcp(error) => format!("tcp {}", error.kind()),
            WsError::Tls(error) => format!("tls {error}"),
            WsError::Upgrade(error) => format!("ws_upgrade {}", error.compact()),
            WsError::Io(e) => {
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock
                {
                    "timeout".to_string()
                } else {
                    e.to_string()
                }
            }
            WsError::Other(s) => s.clone(),
        }
    }
    pub fn handshake_status(&self) -> Option<i32> {
        if let WsError::Handshake(h) = self {
            Some(h.status_code)
        } else {
            None
        }
    }
    pub fn handshake(&self) -> Option<&WsHandshakeError> {
        if let WsError::Handshake(h) = self {
            Some(h)
        } else {
            None
        }
    }
}

pub fn is_http_status_error(err: &WsError, code: i32) -> bool {
    err.handshake_status() == Some(code)
}

impl From<std::io::Error> for WsError {
    fn from(e: std::io::Error) -> Self {
        WsError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// RawWebSocket — порт proxy/raw_websocket.py
// Поддерживает TLS (443) и plain (80, disable_secure), SNI-fronting,
// фрагментацию (CONT/TEXT/BINARY reassembly), close reasons.
// ---------------------------------------------------------------------------

enum WsReader {
    Tls(BufReader<tokio::io::ReadHalf<TlsStream<TcpStream>>>),
    Plain(BufReader<tokio::io::ReadHalf<TcpStream>>),
}

enum WsWriter {
    Tls(tokio::io::WriteHalf<TlsStream<TcpStream>>),
    Plain(tokio::io::WriteHalf<TcpStream>),
}

pub struct RawWebSocket {
    reader: tokio::sync::Mutex<WsReader>,
    writer: tokio::sync::Mutex<WsWriter>,
    frag: tokio::sync::Mutex<Option<Vec<u8>>>,
    pub closed: AtomicBool,
    pub secure: bool,
    pub peer_domain: String,
    pub(crate) network: u64,
    pub(crate) peer: Option<std::net::SocketAddr>,
}

impl RawWebSocket {
    fn new(reader: WsReader, writer: WsWriter, host: &str, secure: bool) -> Self {
        Self {
            reader: tokio::sync::Mutex::new(reader),
            writer: tokio::sync::Mutex::new(writer),
            frag: tokio::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
            secure,
            peer_domain: host.to_owned(),
            network: crate::trace::network(),
            peer: None,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Совместимость: закрыт ли транспорт (аналог writer.transport.is_closing()).
    pub fn transport_closing(&self) -> bool {
        self.is_closed()
    }

    pub async fn send(&self, data: &[u8]) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        let frame = build_frame(OP_BINARY, data, true);
        self.write_frame(&frame, WS_WRITE_TIMEOUT).await
    }

    pub async fn send_batch(&self, parts: &[Vec<u8>]) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        if parts.is_empty() {
            return Ok(());
        }
        self.write_with_timeout(WS_WRITE_TIMEOUT, async |writer| {
            for part in parts {
                let frame = build_frame(OP_BINARY, part, true);
                write_all_match(writer, &frame).await?;
            }
            flush_match(writer).await
        }).await
    }

    pub async fn send_ping(&self) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        let frame = build_frame(OP_PING, &[], true);
        self.write_frame(&frame, WS_CONTROL_TIMEOUT).await
    }

    pub async fn check_idle(&self) -> bool {
        let mut nonce = [0; 8];
        rand::thread_rng().fill_bytes(&mut nonce);
        let result = tokio::time::timeout(Duration::from_millis(650), async {
            self.write_frame(&build_frame(OP_PING, &nonce, true), WS_CONTROL_TIMEOUT).await?;
            loop {
                match self.read_frame().await? {
                    (OP_PONG, data, _) if data == nonce => return Ok::<bool, WsError>(true),
                    (OP_PONG, _, _) => {}
                    (OP_PING, data, _) => {
                        self.write_frame(&build_frame(OP_PONG, &data, true), WS_CONTROL_TIMEOUT).await?;
                    }
                    _ => return Ok(false),
                }
            }
        }).await;
        matches!(result, Ok(Ok(true)))
    }

    async fn write_frame(&self, frame: &[u8], timeout: Duration) -> Result<(), WsError> {
        self.write_with_timeout(timeout, async |writer| {
            write_all_match(writer, frame).await?;
            flush_match(writer).await
        }).await
    }

    async fn write_with_timeout(
        &self,
        timeout: Duration,
        write: impl AsyncFnOnce(&mut WsWriter) -> std::io::Result<()>,
    ) -> Result<(), WsError> {
        let result = tokio::time::timeout(timeout, async {
            let mut writer = self.writer.lock().await;
            write(&mut writer).await
        }).await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.closed.store(true, Ordering::Relaxed);
                Err(WsError::Io(error))
            }
            Err(_) => {
                self.closed.store(true, Ordering::Relaxed);
                Err(WsError::Timeout)
            }
        }
    }

    // Recv с реассемблированием фрагментов (как Go/Python Recv).
    pub async fn recv(&self) -> Result<Vec<u8>, WsError> {
        loop {
            if self.is_closed() {
                return Err(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF",
                )));
            }
            let (opcode, payload, fin) = match self.read_frame().await {
                Ok(v) => v,
                Err(e) => {
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            };
            match opcode {
                OP_CLOSE => {
                    self.closed.store(true, Ordering::Relaxed);
                    let (code, _) = parse_close(&payload);
                    crate::trace::event("ws_close", format_args!("opcode=8 code={code:?}"));
                    let mut close_payload = payload;
                    if close_payload.len() > 2 {
                        close_payload.truncate(2);
                    }
                    let reply = build_frame(OP_CLOSE, &close_payload, true);
                    let _ = self.write_frame(&reply, WS_CONTROL_TIMEOUT).await;
                    return Err(WsError::Closed(code));
                }
                OP_PING => {
                    let pong = build_frame(OP_PONG, &payload, true);
                    self.write_frame(&pong, WS_CONTROL_TIMEOUT).await?;
                    continue;
                }
                OP_PONG => continue,
                OP_CONT => {
                    let mut fragment = self.frag.lock().await;
                    let Some(message) = fragment.as_mut() else {
                        return Err(WsError::Other("unexpected WS continuation".into()));
                    };
                    if message.len() + payload.len() > MAX_MESSAGE_LEN as usize {
                        return Err(WsError::Other("WS message too large".into()));
                    }
                    message.extend_from_slice(&payload);
                    if fin {
                        return Ok(fragment.take().expect("fragment in progress"));
                    }
                }
                OP_BINARY => {
                    let mut fragment = self.frag.lock().await;
                    if fragment.is_some() {
                        return Err(WsError::Other("new WS message before final continuation".into()));
                    }
                    if fin {
                        return Ok(payload);
                    }
                    *fragment = Some(payload);
                }
                _ => return Err(WsError::Other("unexpected WS opcode".into())),
            }
        }
    }

    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        // Best-effort close frame + shutdown транспорта.
        let frame = build_frame(OP_CLOSE, &[], true);
        let _ = self.write_frame(&frame, WS_CONTROL_TIMEOUT).await;
        let _ = self.write_with_timeout(WS_CONTROL_TIMEOUT, async |writer| {
            shutdown_match(writer).await
        }).await;
    }

    async fn read_frame(&self) -> Result<(u8, Vec<u8>, bool), WsError> {
        let mut reader = self.reader.lock().await;
        read_frame_match(&mut reader).await
    }
}

async fn write_all_match(writer: &mut WsWriter, frame: &[u8]) -> std::io::Result<()> {
    match writer {
        WsWriter::Tls(w) => w.write_all(frame).await,
        WsWriter::Plain(w) => w.write_all(frame).await,
    }
}

async fn flush_match(writer: &mut WsWriter) -> std::io::Result<()> {
    match writer {
        WsWriter::Tls(w) => w.flush().await,
        WsWriter::Plain(w) => w.flush().await,
    }
}

async fn shutdown_match(writer: &mut WsWriter) -> std::io::Result<()> {
    match writer {
        WsWriter::Tls(w) => w.shutdown().await,
        WsWriter::Plain(w) => w.shutdown().await,
    }
}

async fn read_frame_match(reader: &mut WsReader) -> Result<(u8, Vec<u8>, bool), WsError> {
    match reader {
        WsReader::Tls(r) => read_frame_generic(r).await,
        WsReader::Plain(r) => read_frame_generic(r).await,
    }
}

// Чтение одного фрейма (generic над TLS/plain reader).
async fn read_frame_generic<R>(reader: &mut BufReader<R>) -> Result<(u8, Vec<u8>, bool), WsError>
where
    R: AsyncReadExt + Unpin,
{
    let mut hdr = [0u8; 2];
    reader.read_exact(&mut hdr).await?;

    let fin = (hdr[0] & 0x80) != 0;
    let opcode = hdr[0] & 0x0F;
    let mut length = (hdr[1] & 0x7F) as u64;
    if hdr[0] & 0x70 != 0 || hdr[1] & 0x80 != 0 || (opcode >= 8 && (!fin || length > 125)) {
        return Err(WsError::Other("invalid upstream WS frame header".into()));
    }

    if length == 126 {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).await?;
        length = BigEndian::read_u16(&buf) as u64;
    } else if length == 127 {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).await?;
        length = BigEndian::read_u64(&buf);
    }

    if length > MAX_FRAME_PAYLOAD {
        return Err(WsError::Other(format!("frame too large: {} bytes", length)));
    }
    let mut payload = vec![0u8; length as usize];
    if length > 0 {
        reader.read_exact(&mut payload).await?;
    }
    Ok((opcode, payload, fin))
}

fn parse_close(payload: &[u8]) -> (Option<u16>, String) {
    if payload.len() < 2 {
        return (None, String::new());
    }
    let code = BigEndian::read_u16(&payload[..2]);
    let text = String::from_utf8_lossy(&payload[2..]).to_string();
    let name = match code {
        1000 => "normal",
        1001 => "going_away",
        1002 => "protocol_error",
        1003 => "unsupported_data",
        1006 => "abnormal",
        1007 => "bad_data",
        1008 => "policy_violation",
        1009 => "too_big",
        1010 => "missing_extension",
        1011 => "internal_error",
        _ => "",
    };
    if name.is_empty() {
        (Some(code), text)
    } else if text.is_empty() {
        (Some(code), name.to_string())
    } else {
        (Some(code), format!("{} ({})", text, name))
    }
}

// ---------------------------------------------------------------------------
// Frame builder (mask=true всегда для клиента)
// ---------------------------------------------------------------------------

pub fn build_frame(opcode: u8, data: &[u8], mask: bool) -> Vec<u8> {
    let length = data.len();
    let fb = 0x80 | opcode;

    let mut header_size = 2;
    if mask {
        header_size += 4;
    }
    if length >= 126 && length < 65536 {
        header_size += 2;
    } else if length >= 65536 {
        header_size += 8;
    }

    let total_size = header_size + length;
    let mut result = vec![0u8; total_size];

    let mut pos = 0;
    result[pos] = fb;
    pos += 1;

    let mut mask_key = [0u8; 4];
    if mask {
        rand::thread_rng().fill_bytes(&mut mask_key);
    }

    if length < 126 {
        let mut lb = length as u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
    } else if length < 65536 {
        let mut lb = 126u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
        BigEndian::write_u16(&mut result[pos..], length as u16);
        pos += 2;
    } else {
        let mut lb = 127u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
        BigEndian::write_u64(&mut result[pos..], length as u64);
        pos += 8;
    }

    if mask {
        result[pos..pos + 4].copy_from_slice(&mask_key);
        pos += 4;
        result[pos..pos + length].copy_from_slice(data);
        xor_mask_in_place(&mut result[pos..pos + length], &mask_key);
    } else {
        result[pos..pos + length].copy_from_slice(data);
    }
    result
}

// ---------------------------------------------------------------------------
// Connection helpers — порт RawWebSocket.connect(host, domain, timeout,
// path, sni, secure) из proxy/raw_websocket.py
// ---------------------------------------------------------------------------

fn set_sock_opts(stream: &TcpStream) {
    if TCP_NODELAY {
        let _ = stream.set_nodelay(true);
    }
    // Аналог Go/Python: keepalive 30s — детект мёртвых соединений на мобиле.
    let sock = socket2::SockRef::from(stream);
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30));
    let _ = sock.set_tcp_keepalive(&ka);
    // proxy/raw_websocket.py::set_sock_opts — buffer_size из конфига.
    let buf = BUFFER_SIZE.load(std::sync::atomic::Ordering::Relaxed);
    if buf > 0 {
        let b = buf as usize;
        let _ = sock.set_recv_buffer_size(b);
        let _ = sock.set_send_buffer_size(b);
    }
}

pub fn ws_connect_timeout(timeout: f64) -> Duration {
    if timeout <= 0.0 {
        Duration::from_secs(5)
    } else {
        Duration::from_secs_f64(timeout)
    }
}

pub fn ws_handshake_timeout(total: Duration) -> Duration {
    if total <= Duration::ZERO {
        Duration::from_secs(3)
    } else if total > Duration::from_secs(3) {
        Duration::from_secs(3)
    } else {
        total
    }
}

// Типизированный handshake: возвращает готовый RawWebSocket без type-erasure.
pub(crate) fn expected_ws_accept(key: &str) -> String {
    // RFC 6455 uses SHA-1 here as an upgrade challenge, not as TLS authentication.
    let mut digest = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    digest.update(key.as_bytes());
    digest.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(digest.finish())
}

async fn upgrade<R, W>(
    read: R,
    write: &mut W,
    host: &str,
    path: &str,
    timeout: Duration,
) -> Result<BufReader<R>, WsError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut random = [0; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let key = base64::engine::general_purpose::STANDARD.encode(random);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n"
    );
    tokio::time::timeout(timeout, async {
        write.write_all(request.as_bytes()).await?;
        write.flush().await
    }).await.map_err(|_| WsError::Timeout)??;

    let mut reader = BufReader::with_capacity(4096, read);
    let (status, status_line, raw_headers) = read_http_response(&mut reader, timeout).await?;
    if status != 101 {
        crate::trace::event("ws_upgrade", format_args!("http={status} verified=false"));
        let location = raw_headers
            .get("location")
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or_default();
        let headers = raw_headers
            .into_iter()
            .map(|(k, v)| (k, v.join(", ")))
            .collect();
        return Err(WsError::Handshake(WsHandshakeError { status_code: status, status_line, headers, location }));
    }

    let matches_token = |header: &str, token: &str| {
        raw_headers.get(header).is_some_and(|values| {
            values
                .iter()
                .flat_map(|value| value.split(','))
                .any(|word| word.trim().eq_ignore_ascii_case(token))
        })
    };
    let accept_ok = raw_headers.get("sec-websocket-accept").is_some_and(|values| {
        values.len() == 1 && values[0] == expected_ws_accept(&key)
    });
    let protocol_values = raw_headers.get("sec-websocket-protocol");
    let protocol_ok = match protocol_values {
        None => true,
        Some(values) => values.len() == 1 && values[0] == "binary",
    };
    let extensions_present = raw_headers
        .get("sec-websocket-extensions")
        .is_some_and(|values| values.iter().any(|value| !value.trim().is_empty()));
    if !status_line.starts_with("HTTP/1.1 ")
        || !matches_token("connection", "upgrade")
        || !matches_token("upgrade", "websocket")
        || !accept_ok
        || !protocol_ok
        || extensions_present
    {
        crate::trace::event("ws_upgrade", format_args!("http=101 verified=false"));
        return Err(WsError::Other("invalid WebSocket upgrade response".into()));
    }
    let protocol = protocol_values
        .and_then(|values| values.first())
        .map(String::as_str)
        .unwrap_or("none");
    crate::trace::event("ws_upgrade", format_args!("http=101 verified=true protocol={protocol}"));
    Ok(reader)
}

fn upgrade_error(error: WsError) -> WsError {
    if matches!(error, WsError::Handshake(_)) { error } else { WsError::Upgrade(Box::new(error)) }
}

async fn handshake_tls(
    read: tokio::io::ReadHalf<TlsStream<TcpStream>>,
    mut write: tokio::io::WriteHalf<TlsStream<TcpStream>>,
    host: &str,
    path: &str,
    timeout: Duration,
) -> Result<RawWebSocket, WsError> {
    let reader = upgrade(read, &mut write, host, path, timeout).await.map_err(upgrade_error)?;
    Ok(RawWebSocket::new(WsReader::Tls(reader), WsWriter::Tls(write), host, true))
}

async fn handshake_plain(
    read: tokio::io::ReadHalf<TcpStream>,
    mut write: tokio::io::WriteHalf<TcpStream>,
    host: &str,
    path: &str,
    timeout: Duration,
) -> Result<RawWebSocket, WsError> {
    let reader = upgrade(read, &mut write, host, path, timeout).await.map_err(upgrade_error)?;
    Ok(RawWebSocket::new(WsReader::Plain(reader), WsWriter::Plain(write), host, false))
}

async fn read_http_response<R: AsyncReadExt + Unpin>(
    bufreader: &mut BufReader<R>,
    timeout: Duration,
) -> Result<(i32, String, HashMap<String, Vec<String>>), WsError> {
    let mut response_lines: Vec<String> = Vec::new();
    let read_result = tokio::time::timeout(timeout, async {
        loop {
            let line = read_line(bufreader).await?;
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            response_lines.push(line);
            if response_lines.len() > 100 {
                return Err(WsError::Other("too many HTTP headers".to_string()));
            }
        }
        Ok::<(), WsError>(())
    })
    .await;
    match read_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(WsError::Timeout),
    }
    if response_lines.is_empty() {
        return Err(WsError::Handshake(WsHandshakeError {
            status_code: 0,
            status_line: "empty response".to_string(),
            headers: HashMap::new(),
            location: String::new(),
        }));
    }
    let first_line = response_lines[0].clone();
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    let mut status_code = 0;
    if parts.len() >= 2 {
        status_code = parts[1].parse::<i32>().unwrap_or(0);
    }
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    for hl in &response_lines[1..] {
        if let Some(idx) = hl.find(':') {
            let name = hl[..idx].trim().to_lowercase();
            let value = hl[idx + 1..].trim().to_string();
            headers.entry(name).or_default().push(value);
        }
    }
    Ok((status_code, first_line, headers))
}

async fn read_line<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<String, WsError> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(WsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF",
            )));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() > 16384 {
            return Err(WsError::Other("header line too long".to_string()));
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

// wsConnectOnce — заголовки 1-в-1 как в Python raw_websocket.py (без User-Agent).
pub async fn ws_connect_once(
    dial_addr: &str,
    domain: &str,
    path: &str,
    timeout: Duration,
    secure: bool,
) -> Result<RawWebSocket, WsError> {
    ws_connect_once_full(dial_addr, domain, path, timeout, None, secure, WsDialProfile::Standard).await
}

#[cfg(test)]
pub(crate) async fn ws_connect_profile(
    dial_addr: &str,
    domain: &str,
    path: &str,
    timeout: Duration,
    profile: WsDialProfile,
) -> Result<RawWebSocket, WsError> {
    ws_connect_once_full(dial_addr, domain, path, timeout, None, true, profile).await
}

async fn ws_connect_once_full(
    dial_addr: &str,
    domain: &str,
    path: &str,
    timeout: Duration,
    sni: Option<&str>,
    secure: bool,
    profile: WsDialProfile,
) -> Result<RawWebSocket, WsError> {
    if dial_addr.is_empty() {
        return Err(WsError::Other("empty dial address".to_string()));
    }
    let path = if path.is_empty() { WS_PATH } else { path };
    let network = crate::trace::network();
    crate::trace::attempt();
    let literal_ip = dial_addr.parse::<std::net::IpAddr>().is_ok();
    let target_kind = if literal_ip { "literal_ip" } else { "hostname" };
    crate::trace::event("ws_dial", format_args!("target={target_kind} host={} sni={} secure={secure}",
        crate::trace::allowlisted_host(domain), crate::trace::allowlisted_host(sni.unwrap_or(domain))));
    let _permit = if matches!(profile, WsDialProfile::Preconnect) {
        crate::dial::try_preconnect(domain).ok_or(WsError::Busy)?
    } else {
        crate::dial::acquire(domain, 3).await.map_err(|_| WsError::Busy)?
    };
    #[cfg(test)]
    let fixture = if dial_addr == "127.0.0.1" { LOOPBACK_FIXTURE.lock().clone() } else { None };
    #[cfg(not(test))]
    let port = upstream_ws_port(secure);
    #[cfg(test)]
    let port = fixture.as_ref().map_or(upstream_ws_port(secure), |(port, _)| *port);
    let target_addr = (dial_addr, port);

    let addresses = match tokio::time::timeout(timeout, tokio::net::lookup_host(target_addr)).await {
        Ok(Ok(addresses)) => addresses
            .filter(|address| literal_ip || crate::cfproxy::public_fallback_ip(address.ip()))
            .collect::<Vec<_>>(),
        Ok(Err(error)) => {
            crate::trace::event("dns", format_args!("outcome=error kind={} errno={:?}", error.kind(), error.raw_os_error()));
            return Err(WsError::Dns(error));
        }
        Err(_) => {
            crate::trace::event("dns", format_args!("outcome=timeout"));
            return Err(WsError::Dns(std::io::ErrorKind::TimedOut.into()));
        }
    };
    if addresses.is_empty() {
        crate::trace::event("dns", format_args!("outcome=no_usable_address"));
        return Err(WsError::Dns(std::io::ErrorKind::NotFound.into()));
    }
    let ipv4 = addresses.iter().filter(|address| address.is_ipv4()).count();
    crate::trace::event("dns", format_args!("outcome=resolved candidates={} ipv4={ipv4} ipv6={}",
        addresses.len(), addresses.len() - ipv4));
    let raw_conn = match tokio::time::timeout(timeout, crate::dial::connect_addresses(&addresses)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            crate::trace::event("tcp_connect", format_args!("outcome=error kind={} errno={:?}", e.kind(), e.raw_os_error()));
            return Err(WsError::Tcp(e));
        }
        Err(_) => {
            crate::trace::event("tcp_connect", format_args!("outcome=timeout"));
            return Err(WsError::Tcp(std::io::ErrorKind::TimedOut.into()));
        }
    };
    let peer = raw_conn.peer_addr().ok();
    let family = match peer {
        Some(std::net::SocketAddr::V4(_)) => "ipv4",
        Some(std::net::SocketAddr::V6(_)) => "ipv6",
        None => "unknown",
    };
    crate::trace::event("tcp_connect", format_args!("outcome=connected family={family}"));
    set_sock_opts(&raw_conn);

    if !secure {
        let (read_half, write_half) = tokio::io::split(raw_conn);
        let mut ws = match handshake_plain(read_half, write_half, domain, path, timeout).await {
            Ok(ws) => ws,
            Err(e) => {
                if matches!(e, WsError::Upgrade(_)) {
                    crate::trace::event("ws_upgrade", format_args!("outcome=io_or_timeout"));
                }
                return Err(e);
            }
        };
        let established = crate::trace::network();
        crate::trace::event(
            "ws_established",
            format_args!(
                "dial_epoch={} established_epoch={} peer={}",
                network >> 3,
                established >> 3,
                crate::trace::allowlisted_peer(peer)
            ),
        );
        if network != established {
            return Err(WsError::NetworkChanged);
        }
        ws.network = network;
        ws.peer = peer;
        return Ok(ws);
    }

    let sni_name = sni.unwrap_or(domain);
    let use_fronting = sni.is_some();
    let cfg = if use_fronting {
        TLS_CONFIG_FRONTING.clone()
    } else {
        TLS_CONFIG.clone()
    };
    #[cfg(test)]
    let cfg = if matches!(profile, WsDialProfile::Tls12) { TLS_CONFIG_12.clone() } else { cfg };
    #[cfg(test)]
    let cfg = fixture.map_or(cfg, |(_, config)| config);
    let connector = TlsConnector::from(cfg);
    let sni_server = ServerName::try_from(sni_name.to_owned())
        .map_err(|_| WsError::Other("invalid TLS server name".into()))?;

    let handshake_timeout = ws_handshake_timeout(timeout);
    let negotiate = async {
        #[cfg(test)]
        if matches!(profile, WsDialProfile::SplitClientHello) {
            crate::trace::event("tls_profile", format_args!("client_hello=split"));
            return crate::tls_hello::connect_fragmented(&connector, sni_server, sni_name, raw_conn).await;
        }
        connector.connect(sni_server, raw_conn).await
    };
    let tls_conn =
        match tokio::time::timeout(handshake_timeout, negotiate).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                let classified = match e.get_ref().and_then(|error| error.downcast_ref::<rustls::Error>()) {
                    Some(rustls::Error::InvalidCertificate(_)) => "certificate_rejected",
                    Some(_) => "tls_protocol_error",
                    None => "io_error",
                };
                crate::trace::event("tls", format_args!("outcome={classified} kind={} errno={:?}", e.kind(), e.raw_os_error()));
                if e.kind() != std::io::ErrorKind::ConnectionReset {
                    ldebug!(" ws tls fail {} via {}: {}", domain, dial_addr, e);
                }
                return Err(WsError::Tls(e));
            }
            Err(_) => {
                crate::trace::event("tls", format_args!("outcome=timeout"));
                ldebug!(" ws tls fail {} via {}: timeout", domain, dial_addr);
                return Err(WsError::Tls(std::io::ErrorKind::TimedOut.into()));
            }
        };

    let tls_state = tls_conn.get_ref().1;
    crate::trace::event("tls", format_args!("outcome=verified version={:?} alpn={}",
        tls_state.protocol_version(), if tls_state.alpn_protocol().is_some() { "negotiated" } else { "none" }));
    let (read_half, write_half) = tokio::io::split(tls_conn);
    let mut ws = match handshake_tls(read_half, write_half, domain, path, timeout).await {
        Ok(ws) => ws,
        Err(e) => {
            if matches!(e, WsError::Upgrade(_)) {
                crate::trace::event("ws_upgrade", format_args!("outcome=io_or_timeout"));
            }
            return Err(e);
        }
    };
    let established = crate::trace::network();
    crate::trace::event("ws_established", format_args!("dial_epoch={} established_epoch={} peer={}",
        network >> 3, established >> 3, crate::trace::allowlisted_peer(peer)));
    if network != established {
        return Err(WsError::NetworkChanged);
    }
    ws.network = network;
    ws.peer = peer;
    Ok(ws)
}

// wsConnect: пытается ip, при необходимости резолвит DoH
pub async fn ws_connect(
    ip: &str,
    domain: &str,
    path: &str,
    timeout: f64,
) -> Result<RawWebSocket, WsError> {
    ws_connect_full_opts(ip, domain, path, timeout, None, None).await
}

pub async fn ws_connect_full_opts(
    ip: &str,
    domain: &str,
    path: &str,
    timeout: f64,
    sni: Option<&str>,
    secure_opt: Option<bool>,
) -> Result<RawWebSocket, WsError> {
    let path = if path.is_empty() { WS_PATH } else { path };
    let attempt_timeout = ws_connect_timeout(timeout);
    let secure = secure_opt.unwrap_or(!DISABLE_SECURE.load(Ordering::Relaxed));

    let primary_addr = if ip.trim().is_empty() {
        domain.to_string()
    } else {
        ip.trim().to_string()
    };

    let result = ws_connect_once_full(&primary_addr, domain, path, attempt_timeout, sni, secure, WsDialProfile::Standard).await;
    if matches!(result, Err(WsError::Dns(_))) && primary_addr == domain {
        if let Some(resolved) = crate::cfproxy::resolve_doh(domain).await {
            return ws_connect_once_full(&resolved, domain, path, attempt_timeout, sni, secure, WsDialProfile::Standard).await;
        }
    }
    result
}

// connectOneWS: перебор доменов (pool refill path, fronting-first как в оригинале)
pub async fn connect_one_ws(ip: &str, domains: &[String]) -> Option<RawWebSocket> {
    connect_one_ws_full(ip, domains, WS_PATH, WS_POOL_CONNECT_TIMEOUT).await
}

pub async fn connect_one_ws_full(
    ip: &str,
    domains: &[String],
    path: &str,
    timeout: f64,
) -> Option<RawWebSocket> {
    for domain in domains {
        if let Ok(ws) = ws_connect_once_full(ip, domain, path, ws_connect_timeout(timeout), None, true, WsDialProfile::Preconnect).await {
            return Some(ws);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustls::ServerConfig;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    #[tokio::test]
    #[ignore = "requires DNS/TCP access to the existing public CF fallback hosts"]
    async fn cf_media_h2_alpn_preflight() {
        let domains = crate::cfproxy::default_cfproxy_domains();
        let mut tls_config = verified_tls_config();
        tls_config.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(tls_config));
        let mut negotiated = 0;
        for (dc, domain_index) in [(1, 0), (2, 0), (2, 1), (5, 0)] {
            let host = format!("kws{dc}.{}", domains[domain_index]);
            let started = std::time::Instant::now();
            let result = async {
                let addresses = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host.as_str(), 443)))
                    .await.map_err(|_| "dns_timeout")?.map_err(|_| "dns_error")?.collect::<Vec<_>>();
                let socket = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addresses.as_slice()))
                    .await.map_err(|_| "tcp_timeout")?.map_err(|_| "tcp_error")?;
                let sni = ServerName::try_from(host).map_err(|_| "invalid_sni")?;
                let tls = tokio::time::timeout(Duration::from_secs(4), connector.connect(sni, socket))
                    .await.map_err(|_| "tls_timeout")?.map_err(|error| {
                        match error.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
                            Some(rustls::Error::InvalidCertificate(_)) => "certificate_rejected",
                            _ => "tls_error",
                        }
                    })?;
                if tls.get_ref().1.alpn_protocol() == Some(b"h2") { Ok(()) } else { Err("h2_not_negotiated") }
            }.await;
            println!("cf_h2_preflight logical_dc={dc} domain_slot={domain_index} elapsed_ms={} outcome={result:?}", started.elapsed().as_millis());
            negotiated += usize::from(result.is_ok());
        }
        assert!(negotiated > 0, "no allowed candidate completed verified TLS with h2 ALPN");
    }

    #[tokio::test]
    async fn rejects_fragmented_control_frame() {
        let wire = [OP_PING, 1, 0];
        let mut reader = BufReader::new(wire.as_slice());
        assert!(matches!(read_frame_generic(&mut reader).await, Err(WsError::Other(_))));
    }

    #[tokio::test]
    async fn upgrade_rejects_a_101_with_an_invalid_accept() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (client, server) = tokio::join!(TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        let (read, write) = tokio::io::split(client.unwrap());
        let mut server = BufReader::new(server.unwrap().0);
        let fake_upgrade = async {
            while read_line(&mut server).await.unwrap() != "\r\n" {}
            server.get_mut().write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: wrong\r\n\r\n").await.unwrap();
        };
        let (result, ()) = tokio::join!(
            handshake_plain(read, write, "localhost", "/apiws", Duration::from_secs(1)),
            fake_upgrade
        );
        assert!(result.is_err(), "a status code alone must not establish a WebSocket");
    }

    pub(crate) async fn loopback_ws() -> (RawWebSocket, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (client, server) = tokio::join!(TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        let (reader, writer) = tokio::io::split(client.unwrap());
        let ws = RawWebSocket {
            reader: tokio::sync::Mutex::new(WsReader::Plain(BufReader::new(reader))),
            writer: tokio::sync::Mutex::new(WsWriter::Plain(writer)),
            frag: tokio::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
            secure: false,
            peer_domain: "loopback".into(),
            network: crate::trace::network(),
            peer: None,
        };
        (ws, server.unwrap().0)
    }

    #[tokio::test]
    async fn websocket_reassembles_large_fragmented_binary_and_replies_to_ping() {
        let (ws, mut server) = loopback_ws().await;
        let payload: Vec<_> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let server_exchange = async {
            let mut first = build_frame(OP_BINARY, &payload[..700_000], false);
            first[0] &= 0x7f;
            for chunk in first.chunks(8191) {
                server.write_all(chunk).await.unwrap();
            }
            server.write_all(&build_frame(OP_PING, b"probe", false)).await.unwrap();
            let mut reply = [0; 11];
            server.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply[..2], &[0x8a, 0x85]);
            for (i, expected) in b"probe".iter().enumerate() {
                assert_eq!(reply[i + 6] ^ reply[2 + i % 4], *expected);
            }
            server.write_all(&build_frame(OP_CONT, &payload[700_000..], false)).await.unwrap();
        };
        let (received, ()) = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(ws.recv(), server_exchange)
        }).await.unwrap();
        let received = received.unwrap();
        assert_eq!(received.len(), payload.len());
        assert!(received == payload);
    }

    #[tokio::test]
    async fn closed_idle_socket_is_not_valid_for_checkout() {
        let (ws, server) = loopback_ws().await;
        drop(server);
        assert!(!ws.check_idle().await);
    }

    #[tokio::test]
    async fn wss_rejects_untrusted_self_signed_certificate() {
        let key = rcgen::KeyPair::generate().expect("key");
        let params = rcgen::CertificateParams::new(vec!["kws1.web.telegram.org".to_string()])
            .expect("params");
        let cert = params.self_signed(&key).expect("cert");
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(key.serialize_der().into());
        let server_cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("server config");
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let _ = acceptor.accept(tcp).await;
        });

        let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let name = ServerName::try_from("kws1.web.telegram.org").expect("sni");
        let connector = TlsConnector::from(TLS_CONFIG.clone());
        let handshake = connector.connect(name, tcp).await;
        let _ = server.await;
        assert!(
            handshake.is_err(),
            "WSS must reject a certificate that is not signed by a trusted CA"
        );
    }

    async fn run_upgrade_with_response(build_resp: impl FnOnce(&str) -> String + Send + 'static) -> Result<(), WsError> {
        let (client, mut server) = tokio::io::duplex(4096);
        let (client_r, mut client_w) = tokio::io::split(client);
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = server.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let key = req
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap()
                .trim()
                .to_string();
            let accept = expected_ws_accept(&key);
            let resp = build_resp(&accept);
            server.write_all(resp.as_bytes()).await.unwrap();
            server.flush().await.unwrap();
        });
        let result = upgrade(client_r, &mut client_w, "kws2.example.org", "/apiws", Duration::from_secs(2)).await.map(|_| ());
        server_task.await.unwrap();
        result
    }

    #[tokio::test]
    async fn ws_upgrade_accepts_duplicate_connection_headers_with_exact_binary_protocol() {
        let result = run_upgrade_with_response(|accept| {
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Connection: keep-alive\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: binary\r\n\r\n"
            )
        }).await;
        assert!(result.is_ok(), "duplicate Connection list headers should be accepted per RFC 9110");
    }

    #[tokio::test]
    async fn ws_upgrade_rejects_repeated_accept_header() {
        let result = run_upgrade_with_response(|accept| {
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: binary\r\n\r\n"
            )
        }).await;
        assert!(result.is_err(), "repeated Sec-WebSocket-Accept must be rejected per RFC 6455");
    }

    #[tokio::test]
    async fn ws_upgrade_rejects_repeated_or_non_exact_protocol_header() {
        let repeated = run_upgrade_with_response(|accept| {
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: binary\r\n\
                 Sec-WebSocket-Protocol: binary\r\n\r\n"
            )
        }).await;
        assert!(repeated.is_err(), "repeated Sec-WebSocket-Protocol must be rejected per RFC 6455");

        let wrong_case = run_upgrade_with_response(|accept| {
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: Binary\r\n\r\n"
            )
        }).await;
        assert!(wrong_case.is_err(), "Sec-WebSocket-Protocol must match exact case 'binary'");
    }

    #[tokio::test]
    async fn ws_upgrade_rejects_unexpected_protocol_or_extensions() {
        let result = run_upgrade_with_response(|accept| {
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: binary\r\n\
                 Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
            )
        }).await;
        assert!(result.is_err(), "non-empty Sec-WebSocket-Extensions must be rejected");
    }
}
