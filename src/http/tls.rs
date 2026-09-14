//! TLS backend selection.
//!
//! Two backends are available, because `ring` — the crypto provider behind
//! rustls — supports only x86, x86_64, aarch64, arm and wasm32, and fails the
//! build outright on anything else. Platforms outside that set (notably the
//! 32-bit PowerPC e500v2 in CZ.NIC Turris 1.x routers) can link against the
//! system OpenSSL instead:
//!
//! ```sh
//! cargo build --release --no-default-features --features native-tls
//! ```

use crate::http::connector::{BindOptions, BoundConnector, WriteMeter};

/// What a connection's TLS handshake settled on.
///
/// Carried into the report as well as logged: on hardware without AES
/// acceleration the cipher, not the link, can bound the result, and under TLS
/// 1.3 the server picks it from the set the client offers. Two otherwise
/// identical runs can therefore differ several-fold for a reason the numbers
/// alone do not show.
#[derive(Clone, Debug)]
pub struct TlsFacts {
    pub version: String,
    pub cipher: String,
}

/// TLS trust configuration.
pub struct TlsSettings<'a> {
    /// PEM bundle replacing the system trust store (`--ca-cert`).
    pub ca_cert: Option<&'a std::path::Path>,
    /// Accept any certificate (`--skip-cert-verify`).
    pub skip_verify: bool,
    /// Offer h2 in ALPN alongside http/1.1 (`--http2`).
    pub http2: bool,
}

/// Splits a PEM bundle into its individual certificates.
#[cfg(all(feature = "native-tls", not(feature = "rustls-tls")))]
fn split_pem(pem: &[u8]) -> Vec<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut rest = text.as_ref();

    while let Some(start) = rest.find(BEGIN) {
        let Some(end) = rest[start..].find(END) else {
            break;
        };
        let end = start + end + END.len();
        out.push(rest.as_bytes()[start..end].to_vec());
        rest = &rest[end..];
    }
    out
}

#[cfg(feature = "rustls-tls")]
mod imp {
    use std::sync::Arc;

    use anyhow::{bail, Context as _};

    use super::{BindOptions, BoundConnector, TlsSettings, WriteMeter};

    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use hyper_util::client::legacy::connect::{Connected, Connection};

    use super::TlsFacts;
    use crate::http::connector::TrackedStream;

    type Inner = hyper_rustls::HttpsConnector<BoundConnector>;
    type InnerStream = hyper_rustls::MaybeHttpsStream<TrackedStream>;

    pub type Connector = ReportingConnector;

    /// Names a protocol version the way Go's `tls.VersionName` does, so the two
    /// clients report the same string for the same connection.
    fn version_name(v: rustls::ProtocolVersion) -> String {
        match v {
            rustls::ProtocolVersion::TLSv1_3 => "TLS 1.3".to_string(),
            rustls::ProtocolVersion::TLSv1_2 => "TLS 1.2".to_string(),
            rustls::ProtocolVersion::TLSv1_1 => "TLS 1.1".to_string(),
            rustls::ProtocolVersion::TLSv1_0 => "TLS 1.0".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// Names a cipher suite the way Go's `tls.CipherSuiteName` does. rustls
    /// spells the TLS 1.3 suites `TLS13_...` where the registry and Go spell
    /// them `TLS_...`.
    fn cipher_name(c: rustls::CipherSuite) -> String {
        match c.as_str() {
            Some(name) => match name.strip_prefix("TLS13_") {
                Some(rest) => format!("TLS_{rest}"),
                None => name.to_string(),
            },
            None => format!("{c:?}"),
        }
    }

    fn facts(stream: &InnerStream) -> Option<TlsFacts> {
        let InnerStream::Https(io) = stream else {
            return None;
        };
        let (_, session) = io.inner().get_ref();
        Some(TlsFacts {
            version: version_name(session.protocol_version()?),
            cipher: cipher_name(session.negotiated_cipher_suite()?.suite()),
        })
    }

    /// Wraps the TLS connector so a connection's negotiated parameters travel
    /// with it. hyper copies a connection's extras onto the response, which is
    /// the only way to learn them for the connection a request actually used
    /// rather than for a throwaway one opened to ask.
    #[derive(Clone)]
    pub struct ReportingConnector(Inner);

    /// A connected stream that carries what its handshake settled on.
    pub struct ReportingStream {
        inner: InnerStream,
        facts: Option<TlsFacts>,
    }

    impl Connection for ReportingStream {
        fn connected(&self) -> Connected {
            let c = self.inner.connected();
            match &self.facts {
                Some(f) => c.extra(f.clone()),
                None => c,
            }
        }
    }

    impl hyper::rt::Read for ReportingStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl hyper::rt::Write for ReportingStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }

        fn is_write_vectored(&self) -> bool {
            self.inner.is_write_vectored()
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
        }
    }

    impl tower_service::Service<http::Uri> for ReportingConnector {
        type Response = ReportingStream;
        type Error = <Inner as tower_service::Service<http::Uri>>::Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            tower_service::Service::poll_ready(&mut self.0, cx)
        }

        fn call(&mut self, dst: http::Uri) -> Self::Future {
            let fut = tower_service::Service::call(&mut self.0, dst);
            Box::pin(async move {
                let inner = fut.await?;
                let facts = facts(&inner);
                Ok(ReportingStream { inner, facts })
            })
        }
    }

    /// Certificate verifier that accepts everything, for `--skip-cert-verify`.
    #[derive(Debug)]
    struct NoVerifier(Arc<rustls::crypto::CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for NoVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls_pki_types::CertificateDer<'_>,
            _intermediates: &[rustls_pki_types::CertificateDer<'_>],
            _server_name: &rustls_pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls_pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls_pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls_pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// Reads and parses a `--ca-cert` bundle, failing the run if it is
    /// unreadable or holds no certificate, as the Go client does.
    fn read_ca_bundle(
        path: &std::path::Path,
    ) -> anyhow::Result<Vec<rustls_pki_types::CertificateDer<'static>>> {
        use rustls_pki_types::pem::PemObject as _;

        let pem = std::fs::read(path)
            .with_context(|| format!("cannot read CA certificate bundle {}", path.display()))?;
        let certs = rustls_pki_types::CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()?;
        if certs.is_empty() {
            bail!("no certificates found in {}", path.display());
        }
        Ok(certs)
    }

    fn client_config(tls: &TlsSettings<'_>) -> anyhow::Result<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        // ALPN is left untouched: hyper-rustls sets it from `enable_http1()`.
        if tls.skip_verify {
            // Say so rather than ignoring the bundle in silence: a run that
            // named a CA file and verified nothing looks like a run that
            // verified against that file.
            if let Some(path) = tls.ca_cert {
                read_ca_bundle(path)?;
                crate::write_error!(
                    "--skip-cert-verify overrides --ca-cert: {} is not used\n",
                    path.display()
                );
            }
            return Ok(
                rustls::ClientConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()?
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerifier(provider)))
                    .with_no_client_auth(),
            );
        }

        let mut roots = rustls::RootCertStore::empty();
        match tls.ca_cert {
            // `--ca-cert` replaces the system trust store, as it does in the Go version.
            Some(path) => {
                for cert in read_ca_bundle(path)? {
                    roots.add(cert)?;
                }
            }
            None => {
                let native = rustls_native_certs::load_native_certs();
                for cert in native.certs {
                    // Ignore individual unparsable roots, like Go's system pool does.
                    let _ = roots.add(cert);
                }
                if roots.is_empty() {
                    bail!("could not load any certificate from the system trust store");
                }
            }
        }

        Ok(rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth())
    }

    pub fn build(
        bind: BindOptions,
        meter: WriteMeter,
        tls: &TlsSettings<'_>,
    ) -> anyhow::Result<Connector> {
        let builder = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(client_config(tls)?)
            .https_or_http();

        // ALPN decides the protocol, so h2 is only reachable when offered here.
        Ok(ReportingConnector(if tls.http2 {
            builder
                .enable_all_versions()
                .wrap_connector(BoundConnector::new(bind, meter.clone()))
        } else {
            builder
                .enable_http1()
                .wrap_connector(BoundConnector::new(bind, meter.clone()))
        }))
    }
}

#[cfg(all(feature = "native-tls", not(feature = "rustls-tls")))]
mod imp {
    use anyhow::{bail, Context as _};

    use super::{split_pem, BindOptions, BoundConnector, TlsSettings, WriteMeter};

    pub type Connector = hyper_tls::HttpsConnector<BoundConnector>;

    // No TlsFacts are produced here: native-tls exposes the negotiated ALPN
    // protocol and the peer certificate, but neither the protocol version nor
    // the cipher suite, and it does not hand out the underlying OpenSSL
    // session to ask directly. A run over this backend therefore reports
    // whether the connection was encrypted, but not with what.

    pub fn build(
        bind: BindOptions,
        meter: WriteMeter,
        tls: &TlsSettings<'_>,
    ) -> anyhow::Result<Connector> {
        let mut builder = native_tls::TlsConnector::builder();

        if tls.skip_verify {
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }

        if let Some(path) = tls.ca_cert {
            let pem = std::fs::read(path)
                .with_context(|| format!("cannot read CA certificate bundle {}", path.display()))?;
            let certs = split_pem(&pem);
            if certs.is_empty() {
                bail!("no certificates found in {}", path.display());
            }
            for cert in certs {
                builder.add_root_certificate(native_tls::Certificate::from_pem(&cert)?);
            }
            // `--ca-cert` replaces the system trust store, as it does in the Go version.
            builder.disable_built_in_roots(true);
        }

        if tls.http2 {
            builder.request_alpns(&["h2", "http/1.1"]);
        }

        let connector = builder.build().context("cannot initialise TLS")?;
        let mut https = hyper_tls::HttpsConnector::from((
            BoundConnector::new(bind, meter.clone()),
            tokio_native_tls::TlsConnector::from(connector),
        ));
        // Plain HTTP backends must keep working.
        https.https_only(false);
        Ok(https)
    }
}

#[cfg(not(any(feature = "rustls-tls", feature = "native-tls")))]
compile_error!("enable exactly one TLS backend: `rustls-tls` (default) or `native-tls`");

pub use imp::{build, Connector};
