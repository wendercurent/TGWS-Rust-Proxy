use rustls_pki_types::ServerName;
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_rustls::{client::TlsStream, TlsConnector};

const RECORD_HEADER: usize = 5;
const MAX_RECORD_PAYLOAD: usize = 16_384;
const RECORD_GAP: Duration = Duration::from_millis(30);

/// Divides a complete initial ClientHello record without modifying the handshake bytes.
fn split_record(records: &[u8], hostname: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = records.get(..RECORD_HEADER)?;
    if header[0] != 22 || header[1] != 3 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
    if !(5..=MAX_RECORD_PAYLOAD).contains(&length) {
        return None;
    }
    let payload = records.get(RECORD_HEADER..RECORD_HEADER + length)?;
    let hello_len =
        usize::from(u16::from_be_bytes([payload[2], payload[3]])) | (usize::from(payload[1]) << 16);
    if payload[0] != 1 || hello_len + 4 != length {
        return None;
    }

    let split = if hostname.len() >= 2 {
        payload
            .windows(hostname.len())
            .position(|window| window.eq_ignore_ascii_case(hostname.as_bytes()))
            .map(|offset| offset + hostname.len() / 2)
    } else {
        None
    }
    .unwrap_or(length / 2);
    let encode = |part: &[u8]| {
        let mut record = Vec::with_capacity(RECORD_HEADER + part.len());
        record.extend_from_slice(&header[..3]);
        record.extend_from_slice(&(part.len() as u16).to_be_bytes());
        record.extend_from_slice(part);
        record
    };
    let first = encode(&payload[..split]);
    let mut second = encode(&payload[split..]);
    second.extend_from_slice(&records[RECORD_HEADER + length..]);
    Some((first, second))
}

pub(crate) async fn connect_fragmented<S: AsyncRead + AsyncWrite + Unpin>(
    connector: &TlsConnector,
    server_name: ServerName<'static>,
    hostname: &str,
    socket: S,
) -> io::Result<TlsStream<S>> {
    let mut initial_records = Vec::new();
    let mut result = Ok(0);
    // Rustls owns the handshake transcript. Take only its already encoded first flight.
    let mut handshake = connector.connect_with(server_name, socket, |connection| {
        result = connection.write_tls(&mut initial_records);
    });
    result?;
    if let Some(socket) = handshake.get_mut() {
        if let Some((first, second)) = split_record(&initial_records, hostname) {
            socket.write_all(&first).await?;
            socket.flush().await?;
            tokio::time::sleep(RECORD_GAP).await;
            socket.write_all(&second).await?;
            crate::trace::event(
                "tls_client_hello",
                format_args!("records=2 delay_ms={}", RECORD_GAP.as_millis()),
            );
        } else {
            socket.write_all(&initial_records).await?;
            crate::trace::event("tls_client_hello", format_args!("profile=original_record"));
        }
        socket.flush().await?;
    }
    handshake.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use rustls_pki_types::PrivateKeyDer;
    use std::sync::Arc;
    use tokio_rustls::TlsAcceptor;

    #[test]
    fn splitting_preserves_handshake_and_following_records() {
        let hostname = "proxy.example";
        let body = hostname.as_bytes();
        let mut record = vec![
            22,
            3,
            1,
            0,
            (body.len() + 4) as u8,
            1,
            0,
            0,
            body.len() as u8,
        ];
        record.extend_from_slice(body);
        let next = [20, 3, 3, 0, 1, 1];
        record.extend_from_slice(&next);

        let (first, second) = split_record(&record, hostname).unwrap();
        assert_eq!(&first[..3], &record[..3]);
        assert_eq!(&second[..3], &record[..3]);
        assert_eq!(&second[second.len() - next.len()..], &next);
        assert_eq!(&first[9..], &body[..body.len() / 2]);
        assert_eq!(
            &second[5..second.len() - next.len()],
            &body[body.len() / 2..]
        );
        assert!(split_record(&record[..record.len() - next.len() - 1], hostname).is_none());
    }

    #[tokio::test]
    async fn fragmented_tls_still_checks_trust_and_hostname() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["proxy.example".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap()
            .der()
            .clone();
        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap();
        server_config.send_tls13_tickets = 0;
        let server = TlsAcceptor::from(Arc::new(server_config));
        for (hostname, trusted, expected) in [
            ("proxy.example", true, true),
            ("other.example", true, false),
            ("proxy.example", false, false),
        ] {
            let mut roots = RootCertStore::empty();
            if trusted {
                roots.add(cert.clone()).unwrap();
            }
            let connector = TlsConnector::from(Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ));
            // A small buffer forces partial writes while the peer negotiates concurrently.
            let (dial, peer) = tokio::io::duplex(32);
            let tls = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(
                    connect_fragmented(
                        &connector,
                        ServerName::try_from(hostname).unwrap(),
                        hostname,
                        dial
                    ),
                    server.accept(peer),
                )
            })
            .await
            .unwrap();
            assert_eq!(
                tls.0.is_ok(),
                expected,
                "hostname={hostname} trusted={trusted}"
            );
        }
    }
}
