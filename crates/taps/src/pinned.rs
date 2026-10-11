//! Sending a tool's request to the address the policy checked.
//!
//! The policy decides a request on what its host resolves to: a name that resolves
//! inward is refused. Were the request then to connect by name, it would resolve again,
//! and a name that answers outward to the check and inward to the connect would get
//! through. So the request connects to the addresses the check saw, and only those;
//! TLS still verifies the certificate against the name in the URL.

use http::uri::Scheme;
use http::{Request, Response};
use http_body_util::BodyExt;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody};

/// How long a connect or a handshake may take when the request names no timeout.
const CONNECT: Duration = Duration::from_secs(30);

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static {}
impl<T> Stream for T where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static
{
}

/// The name TLS verifies: the host in the URL, an address literal as an address.
fn server_name(host: &str) -> Result<rustls::pki_types::ServerName<'static>, Error> {
    use rustls::pki_types::ServerName;
    let bare = host.trim_matches(['[', ']']);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(ServerName::from(ip));
    }
    Ok(ServerName::try_from(bare)?.to_owned())
}

/// Send `request` over a connection to one of `addrs`, the addresses the policy was
/// shown, never to whatever the URL's host resolves to now. The response streams as
/// [`wasmtime_wasi_http::default_send_request`]'s does.
///
/// # Errors
///
/// No address, a connect, TLS or protocol failure, or a timeout.
pub async fn send(
    mut request: Request<WasiBody>,
    options: Option<RequestOptions>,
    addrs: &[SocketAddr],
) -> Result<
    (
        Response<WasiBody>,
        Box<dyn Future<Output = Result<(), Error>> + Send>,
    ),
    Error,
> {
    if addrs.is_empty() {
        return Err(Error::DnsError {
            rcode: None,
            info_code: None,
        });
    }
    let connect = options.and_then(|o| o.connect_timeout).unwrap_or(CONNECT);
    let first_byte = options
        .and_then(|o| o.first_byte_timeout)
        .unwrap_or(Duration::from_secs(600));
    let uri = request.uri().clone();
    let tcp = tokio::time::timeout(connect, TcpStream::connect(addrs))
        .await
        .map_err(|_| Error::ConnectionTimeout)?
        .map_err(Error::Connect)?;
    let stream: Box<dyn Stream> = if uri.scheme() == Some(&Scheme::HTTPS) {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let name = server_name(uri.host().unwrap_or_default())?;
        Box::new(
            tokio::time::timeout(connect, tls.connect(name, tcp))
                .await
                .map_err(|_| Error::ConnectionTimeout)?
                .map_err(Error::Tls)?,
        )
    } else {
        Box::new(tcp)
    };
    let (mut sender, conn) = tokio::time::timeout(
        connect,
        hyper::client::conn::http1::Builder::new()
            .handshake(wasmtime_wasi_http::io::TokioIo::new(stream)),
    )
    .await
    .map_err(|_| Error::ConnectionTimeout)??;
    // the request goes out in origin form; the authority stays in the Host header
    *request.uri_mut() = http::Uri::builder()
        .path_and_query(uri.path_and_query().map_or("/", |p| p.as_str()))
        .build()
        .map_err(|e| Error::InternalError(Some(e.to_string())))?;
    if !request.headers().contains_key(http::header::HOST) {
        if let Some(a) = uri.authority() {
            let host = http::HeaderValue::from_str(a.as_str())
                .map_err(|e| Error::InternalError(Some(e.to_string())))?;
            request.headers_mut().insert(http::header::HOST, host);
        }
    }
    let conn = tokio::spawn(conn);
    let response = tokio::time::timeout(first_byte, sender.send_request(request))
        .await
        .map_err(|_| Error::ConnectionReadTimeout)??;
    let io = async move {
        match conn.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Error::from(e)),
            Err(e) => Err(Error::InternalError(Some(e.to_string()))),
        }
    };
    Ok((
        response.map(|b| b.map_err(Error::from).boxed_unsync()),
        Box::new(io),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_request_goes_to_the_checked_address_not_the_name() {
        // a server on loopback, and a URL whose name resolves nowhere: the request
        // arrives all the same, so the name was never looked up again
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let body = http_body_util::Empty::<hyper::body::Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync();
        let request = Request::get("http://pinned.invalid:8080/page?q=1")
            .body(body)
            .unwrap();
        let (response, _io) = send(request, None, &[addr]).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"ok");
        let seen = served.await.unwrap();
        assert!(seen.starts_with("GET /page?q=1 HTTP/1.1\r\n"), "{seen}");
        assert!(
            seen.to_ascii_lowercase()
                .contains("host: pinned.invalid:8080"),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn no_checked_address_is_no_request() {
        let body = http_body_util::Empty::<hyper::body::Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync();
        let request = Request::get("http://example.invalid/").body(body).unwrap();
        assert!(send(request, None, &[]).await.is_err());
    }

    #[test]
    fn tls_verifies_the_name_in_the_url() {
        use rustls::pki_types::ServerName;
        assert_eq!(
            server_name("example.com").unwrap(),
            ServerName::try_from("example.com").unwrap()
        );
        assert_eq!(
            server_name("[::1]").unwrap(),
            ServerName::from(std::net::Ipv6Addr::LOCALHOST)
        );
    }
}
