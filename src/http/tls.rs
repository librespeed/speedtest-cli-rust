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

use crate::http::connector::{BindOptions, BoundConnector};

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
#[cfg(feature = "native-tls")]
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

    use super::{BindOptions, BoundConnector, TlsSettings};

    pub type Connector = hyper_rustls::HttpsConnector<BoundConnector>;

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

    pub fn build(bind: BindOptions, tls: &TlsSettings<'_>) -> anyhow::Result<Connector> {
        let builder = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(client_config(tls)?)
            .https_or_http();

        // ALPN decides the protocol, so h2 is only reachable when offered here.
        Ok(if tls.http2 {
            builder
                .enable_all_versions()
                .wrap_connector(BoundConnector::new(bind))
        } else {
            builder
                .enable_http1()
                .wrap_connector(BoundConnector::new(bind))
        })
    }
}

#[cfg(all(feature = "native-tls", not(feature = "rustls-tls")))]
mod imp {
    use anyhow::{bail, Context as _};

    use super::{split_pem, BindOptions, BoundConnector, TlsSettings};

    pub type Connector = hyper_tls::HttpsConnector<BoundConnector>;

    pub fn build(bind: BindOptions, tls: &TlsSettings<'_>) -> anyhow::Result<Connector> {
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
            BoundConnector::new(bind),
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
