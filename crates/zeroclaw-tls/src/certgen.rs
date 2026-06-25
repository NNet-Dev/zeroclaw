//! Certificate generation for ZeroClaw's mutual-TLS transport.
//!
//! Produces a per-daemon CA and the server / client leaf certificates that chain
//! to it, with correct X.509 profiles: the CA is `CA:TRUE, pathlen:0`
//! (`keyCertSign` + `cRLSign`); the server leaf carries `serverAuth` EKU; the
//! client leaf carries `clientAuth` EKU. `notBefore` is backdated a few minutes
//! for clock skew. This backs the secure-by-default auto-generation path (the
//! daemon mints its own CA + server cert on first run) and client-cert issuance.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A generated certificate + private key, PEM-encoded.
#[derive(Debug, Clone)]
pub struct Pem {
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// PEM-encoded PKCS#8 private key.
    pub key_pem: String,
}

/// On-disk paths to the daemon's mTLS server materials.
#[derive(Debug, Clone)]
pub struct ServerMaterials {
    pub ca_cert_path: PathBuf,
    pub ca_key_path: PathBuf,
    pub server_cert_path: PathBuf,
    pub server_key_path: PathBuf,
}

const CA_VALIDITY_DAYS: i64 = 3650; // 10 years (stable per-daemon root)
const SERVER_VALIDITY_DAYS: i64 = 825; // ~27 months
const CLIENT_VALIDITY_DAYS: i64 = 30; // matches the cert-TTL decision
const SKEW_BACKDATE: time::Duration = time::Duration::minutes(5);

fn distinguished_name(common_name: &str) -> rcgen::DistinguishedName {
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, common_name);
    dn
}

fn set_validity(params: &mut rcgen::CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - SKEW_BACKDATE;
    params.not_after = now + time::Duration::days(days);
}

/// CA profile: `CA:TRUE, pathlen:0`, `keyCertSign` + `cRLSign`. Cannot mint sub-CAs.
fn ca_params(common_name: &str) -> Result<rcgen::CertificateParams> {
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new())
        .context("building CA certificate params")?;
    p.distinguished_name = distinguished_name(common_name);
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    p.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    set_validity(&mut p, CA_VALIDITY_DAYS);
    Ok(p)
}

/// Server leaf profile: `CA:FALSE`, `serverAuth` EKU, the given SANs.
fn server_params(sans: &[String]) -> Result<rcgen::CertificateParams> {
    let mut p = rcgen::CertificateParams::new(sans.to_vec())
        .context("building server certificate params")?;
    p.is_ca = rcgen::IsCa::NoCa;
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    set_validity(&mut p, SERVER_VALIDITY_DAYS);
    Ok(p)
}

/// Client leaf profile: `CA:FALSE`, `clientAuth` EKU only, subject = device id.
fn client_params(subject_common_name: &str) -> Result<rcgen::CertificateParams> {
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new())
        .context("building client certificate params")?;
    p.distinguished_name = distinguished_name(subject_common_name);
    p.is_ca = rcgen::IsCa::NoCa;
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    set_validity(&mut p, CLIENT_VALIDITY_DAYS);
    Ok(p)
}

/// Default SANs for an auto-generated server certificate. Suitable for local /
/// pinned access; operators exposing a public hostname should provide their own
/// server certificate (BYO) with the correct SAN.
fn default_server_sans() -> Vec<String> {
    vec!["localhost".to_string(), "127.0.0.1".to_string()]
}

fn write_pem(path: &Path, pem: &str, private: bool) -> Result<()> {
    std::fs::write(path, pem).with_context(|| format!("writing {}", path.display()))?;
    if private {
        restrict_permissions(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Ensure mTLS server materials exist under `dir`, generating a fresh per-daemon
/// CA + server certificate on first call. Idempotent: if all four files already
/// exist they are returned unchanged (the CA is never silently rotated).
///
/// The private keys are written `0600` on Unix. `server_sans` overrides the
/// default SAN set when non-empty.
pub fn ensure_server_materials(dir: &Path, server_sans: &[String]) -> Result<ServerMaterials> {
    let materials = ServerMaterials {
        ca_cert_path: dir.join("ca.crt"),
        ca_key_path: dir.join("ca.key"),
        server_cert_path: dir.join("server.crt"),
        server_key_path: dir.join("server.key"),
    };

    if materials.ca_cert_path.exists()
        && materials.ca_key_path.exists()
        && materials.server_cert_path.exists()
        && materials.server_key_path.exists()
    {
        return Ok(materials);
    }

    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating TLS materials directory {}", dir.display()))?;

    let ca_key = rcgen::KeyPair::generate().context("generating CA key")?;
    let ca_cert = ca_params("ZeroClaw WSS CA")?
        .self_signed(&ca_key)
        .context("self-signing CA certificate")?;

    let server_key = rcgen::KeyPair::generate().context("generating server key")?;
    let sans = if server_sans.is_empty() {
        default_server_sans()
    } else {
        server_sans.to_vec()
    };
    let server_cert = server_params(&sans)?
        .signed_by(&server_key, &ca_cert, &ca_key)
        .context("signing server certificate")?;

    write_pem(&materials.ca_cert_path, &ca_cert.pem(), false)?;
    write_pem(&materials.ca_key_path, &ca_key.serialize_pem(), true)?;
    write_pem(&materials.server_cert_path, &server_cert.pem(), false)?;
    write_pem(
        &materials.server_key_path,
        &server_key.serialize_pem(),
        true,
    )?;

    Ok(materials)
}

/// Issue a client certificate signed by the CA whose PEM cert + key are given.
/// The returned key is generated fresh; the subject CN is the device identity.
pub fn issue_client_cert(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    subject_common_name: &str,
) -> Result<Pem> {
    let ca_key = rcgen::KeyPair::from_pem(ca_key_pem).context("loading CA key")?;
    let ca_cert = rcgen::CertificateParams::from_ca_cert_pem(ca_cert_pem)
        .context("loading CA certificate")?
        .self_signed(&ca_key)
        .context("reconstructing CA issuer")?;

    let leaf_key = rcgen::KeyPair::generate().context("generating client key")?;
    let leaf = client_params(subject_common_name)?
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .context("signing client certificate")?;

    Ok(Pem {
        cert_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_server_materials_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let m1 = ensure_server_materials(dir.path(), &[]).unwrap();
        assert!(m1.ca_cert_path.exists() && m1.server_key_path.exists());
        let ca1 = std::fs::read_to_string(&m1.ca_cert_path).unwrap();

        // Second call must NOT regenerate (CA is stable / never silently rotated).
        let m2 = ensure_server_materials(dir.path(), &[]).unwrap();
        let ca2 = std::fs::read_to_string(&m2.ca_cert_path).unwrap();
        assert_eq!(ca1, ca2, "CA was regenerated on the second call");
    }

    #[cfg(unix)]
    #[test]
    fn private_keys_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let m = ensure_server_materials(dir.path(), &[]).unwrap();
        let mode = std::fs::metadata(&m.ca_key_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "CA key permissions are {mode:o}, expected 600");
    }
}
