//! Local Certificate Authority for TLS interception.
//!
//! To read HTTPS traffic from cloud APIs the proxy terminates TLS using a
//! certificate it mints on the fly for the requested host, signed by a local CA
//! the user trusts once via `replaykit setup`. Private keys never leave the
//! machine; the CA is only ever used to sign short-lived leaf certs for hosts
//! the agent itself is talking to.
//!
//! Certificate generation is delegated to `rcgen` (a mature, audited library) —
//! the matching/divergence/storage logic that is replaykit's real contribution
//! is all hand-written.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;

const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

/// The local CA: its certificate (PEM, for trust installation) plus an issuer
/// used to sign per-host leaf certificates.
pub struct LocalCa {
    #[allow(dead_code)] // surfaced via cert_pem(); used by tests and downstream tooling
    cert_pem: String,
    /// The CA certificate, used as the issuer when signing per-host leaves.
    ca_cert: Certificate,
    ca_key: KeyPair,
    dir: PathBuf,
    /// Cache of host -> ready rustls config, so each host's leaf is minted once.
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl LocalCa {
    /// Generate a fresh CA into `dir` (overwrites any existing material).
    pub fn generate(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).with_context(|| format!("creating CA dir {}", dir.display()))?;

        let key = KeyPair::generate().context("generating CA key")?;
        let mut params = CertificateParams::new(Vec::new()).context("CA params")?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "replaykit local CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "replaykit");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let cert = params.self_signed(&key).context("self-signing CA")?;
        let cert_pem = cert.pem();
        let key_pem = key.serialize_pem();

        fs::write(dir.join(CA_CERT_FILE), &cert_pem)?;
        write_private(&dir.join(CA_KEY_FILE), &key_pem)?;

        Ok(LocalCa {
            cert_pem,
            ca_cert: cert,
            ca_key: key,
            dir,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Load an existing CA from `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);
        if !cert_path.exists() || !key_path.exists() {
            bail!(
                "no CA found in {} — run `replaykit setup` first",
                dir.display()
            );
        }
        let cert_pem = fs::read_to_string(&cert_path)?;
        let key_pem = fs::read_to_string(&key_path)?;
        let key = KeyPair::from_pem(&key_pem).context("loading CA key")?;
        // Rebuild a usable issuer certificate from the stored PEM + key.
        let params =
            CertificateParams::from_ca_cert_pem(&cert_pem).context("parsing CA certificate")?;
        let cert = params
            .self_signed(&key)
            .context("reconstructing CA issuer")?;
        Ok(LocalCa {
            cert_pem,
            ca_cert: cert,
            ca_key: key,
            dir,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Load the CA from `dir`, generating it if absent.
    #[allow(dead_code)]
    pub fn load_or_generate(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        if dir.join(CA_CERT_FILE).exists() {
            Self::load(dir)
        } else {
            Self::generate(dir)
        }
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CA_CERT_FILE)
    }

    #[allow(dead_code)]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Build (or fetch from cache) a rustls server config presenting a leaf
    /// certificate valid for `host`.
    pub fn server_config_for(&self, host: &str) -> Result<Arc<ServerConfig>> {
        if let Some(cfg) = self.cache.lock().unwrap().get(host) {
            return Ok(cfg.clone());
        }
        let cfg = Arc::new(self.mint_server_config(host)?);
        self.cache
            .lock()
            .unwrap()
            .insert(host.to_string(), cfg.clone());
        Ok(cfg)
    }

    fn mint_server_config(&self, host: &str) -> Result<ServerConfig> {
        let leaf_key = KeyPair::generate().context("generating leaf key")?;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .or_else(|_| CertificateParams::new(Vec::new()))
            .context("leaf params")?;
        // If the host wasn't accepted as a DNS SAN (e.g. an IP), add it explicitly.
        if params.subject_alt_names.is_empty() {
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                params.subject_alt_names.push(SanType::IpAddress(ip));
            } else {
                params.subject_alt_names.push(SanType::DnsName(
                    host.try_into().context("host as DNS name")?,
                ));
            }
        }
        params.distinguished_name.push(DnType::CommonName, host);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .context("signing leaf cert")?;

        let cert_der = CertificateDer::from(leaf.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let mut cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .context("building leaf server config")?;
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(cfg)
    }

    /// Install the CA into the OS trust store. Best-effort and platform aware;
    /// returns instructions when it cannot do it automatically.
    pub fn install_trust(&self) -> Result<TrustOutcome> {
        install_ca_trust(&self.cert_path())
    }
}

/// Result of attempting to trust the CA.
#[allow(dead_code)] // `Installed` is only constructed on platforms with automatic trust (Windows)
pub enum TrustOutcome {
    /// Trust installed automatically.
    Installed,
    /// Could not install automatically; `instructions` tell the user how.
    Manual { instructions: String },
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    use std::io::Write;
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_ca_trust(cert_path: &Path) -> Result<TrustOutcome> {
    use std::process::Command;
    let out = Command::new("certutil")
        .args(["-user", "-addstore", "Root"])
        .arg(cert_path)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(TrustOutcome::Installed),
        _ => Ok(TrustOutcome::Manual {
            instructions: format!(
                "Run this in an elevated terminal to trust the CA:\n  certutil -user -addstore Root \"{}\"",
                cert_path.display()
            ),
        }),
    }
}

#[cfg(target_os = "macos")]
fn install_ca_trust(cert_path: &Path) -> Result<TrustOutcome> {
    Ok(TrustOutcome::Manual {
        instructions: format!(
            "Trust the CA in the system keychain:\n  sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain \"{}\"",
            cert_path.display()
        ),
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_ca_trust(cert_path: &Path) -> Result<TrustOutcome> {
    Ok(TrustOutcome::Manual {
        instructions: format!(
            "Trust the CA system-wide (Debian/Ubuntu shown):\n  sudo cp \"{}\" /usr/local/share/ca-certificates/replaykit.crt\n  sudo update-ca-certificates\nMany tools also honour:  export REQUESTS_CA_BUNDLE=\"{}\"  /  export SSL_CERT_FILE=\"{}\"",
            cert_path.display(),
            cert_path.display(),
            cert_path.display()
        ),
    })
}

/// Build a rustls client config that trusts the real public web PKI, used by
/// the proxy to connect to upstreams in record mode.
///
/// If the `REPLAYKIT_EXTRA_ROOTS` environment variable points at a PEM file,
/// every CA in that file is appended to the trust store. This is the
/// recommended hook for integration tests that record against a localhost
/// TLS mock — it stays narrow (no "disable verification" flag exists) so the
/// real `record` path stays safe by default.
pub fn upstream_client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Ok(path) = std::env::var("REPLAYKIT_EXTRA_ROOTS") {
        if let Ok(bytes) = std::fs::read(&path) {
            let mut cursor = std::io::Cursor::new(bytes);
            for entry in rustls_pemfile::certs(&mut cursor).flatten() {
                let _ = roots.add(entry);
            }
        }
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_mint() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::generate(dir.path()).unwrap();
        assert!(ca.cert_pem().contains("BEGIN CERTIFICATE"));
        // Mint a leaf for a host and ensure it caches.
        let c1 = ca.server_config_for("api.openai.com").unwrap();
        let c2 = ca.server_config_for("api.openai.com").unwrap();
        assert!(Arc::ptr_eq(&c1, &c2));
        // A different host mints a different config.
        let c3 = ca.server_config_for("api.anthropic.com").unwrap();
        assert!(!Arc::ptr_eq(&c1, &c3));
    }

    #[test]
    fn reload_existing_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::generate(dir.path()).unwrap();
        let pem = ca.cert_pem().to_string();
        drop(ca);
        let ca2 = LocalCa::load(dir.path()).unwrap();
        assert_eq!(ca2.cert_pem(), pem);
        // Issuer still works for minting.
        ca2.server_config_for("example.com").unwrap();
    }
}
