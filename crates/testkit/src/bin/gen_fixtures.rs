//! Generates TLS certificates (`testdata/certs`), SSH keys (`testdata/ssh`) and the MongoDB
//! replica-set keyfile (`testdata/mongo/keyfile`) for the Docker test matrix. Idempotent:
//! existing files are kept. All of them are gitignored.

use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use ssh_key::rand_core::{OsRng, RngCore};
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

const KEY_PASSPHRASE: &str = "dnt_pass";

fn main() -> Result<(), Box<dyn Error>> {
    let certs = dnt_testkit::testdata("certs");
    gen_certs(&certs)?;
    gen_client_combined_pem(&certs)?;
    gen_ssh_keys(&dnt_testkit::testdata("ssh"))?;
    gen_mongo_keyfile(&dnt_testkit::testdata("mongo/keyfile"))?;
    Ok(())
}

/// `client-combined.pem`: the client certificate followed by its key, the one-file form
/// MongoDB clients take (`tlsCertificateKeyFile`). Rewritten every run, it is derived.
fn gen_client_combined_pem(dir: &Path) -> Result<(), Box<dyn Error>> {
    let cert = std::fs::read_to_string(dir.join("client.pem"))?;
    let key = std::fs::read_to_string(dir.join("client-key.pem"))?;
    let mut combined = cert.trim_end().to_string();
    combined.push('\n');
    combined.push_str(key.trim_end());
    combined.push('\n');
    std::fs::write(dir.join("client-combined.pem"), combined)?;
    println!("certs: client-combined.pem written");
    Ok(())
}

/// Shared secret of the `mongo80` replica set's internal authentication: 128 hex
/// characters (MongoDB takes 6–1024 characters of the base64 alphabet).
fn gen_mongo_keyfile(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        println!("mongo: keyfile up to date");
        return Ok(());
    }
    let mut bytes = [0u8; 64];
    OsRng.fill_bytes(&mut bytes);
    let mut text = String::with_capacity(129);
    for b in bytes {
        write!(text, "{b:02x}")?;
    }
    text.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    println!("mongo: generated keyfile");
    Ok(())
}

/// rcgen defaults to a 1975 start date, which some TLS stacks misread as 2075 ("not yet valid").
/// Use explicit dates that stay within UTCTime (< 2050).
fn set_validity(params: &mut CertificateParams) {
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2049, 12, 31);
}

fn dn(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "DntDatabase Test");
    dn.push(DnType::CommonName, cn);
    dn
}

fn gen_certs(dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;
    let names = [
        "ca.pem",
        "server.pem",
        "server-key.pem",
        "client.pem",
        "client-key.pem",
        "server-nosan.pem",
        "server-nosan-key.pem",
    ];
    if names.iter().all(|n| dir.join(n).exists()) {
        println!("certs: up to date");
        return Ok(());
    }

    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    set_validity(&mut ca_params);
    ca_params.distinguished_name = dn("DntDatabase Test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca_params.self_signed(&ca_key)?;

    let server_key = KeyPair::generate()?;
    let mut server_params =
        CertificateParams::new(["localhost", "127.0.0.1", "mysql8", "pg17"].map(String::from))?;
    set_validity(&mut server_params);
    server_params.distinguished_name = dn("localhost");
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server = server_params.signed_by(&server_key, &ca, &ca_key)?;

    let client_key = KeyPair::generate()?;
    let mut client_params = CertificateParams::new(Vec::<String>::new())?;
    set_validity(&mut client_params);
    client_params.distinguished_name = dn("dnt-client");
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &ca, &ca_key)?;

    // Like MySQL's auto-generated server certificates: CN only, no subjectAltName.
    let nosan_key = KeyPair::generate()?;
    let mut nosan_params = CertificateParams::new(Vec::<String>::new())?;
    set_validity(&mut nosan_params);
    nosan_params.distinguished_name = dn("DntDatabase_Auto_Generated_Server_Certificate");
    nosan_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let nosan = nosan_params.signed_by(&nosan_key, &ca, &ca_key)?;

    std::fs::write(dir.join("ca.pem"), ca.pem())?;
    std::fs::write(dir.join("server.pem"), server.pem())?;
    std::fs::write(dir.join("server-key.pem"), server_key.serialize_pem())?;
    std::fs::write(dir.join("client.pem"), client.pem())?;
    std::fs::write(dir.join("client-key.pem"), client_key.serialize_pem())?;
    std::fs::write(dir.join("server-nosan.pem"), nosan.pem())?;
    std::fs::write(dir.join("server-nosan-key.pem"), nosan_key.serialize_pem())?;
    println!("certs: generated in {}", dir.display());
    Ok(())
}

fn gen_ssh_keys(dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;
    write_ssh_key(dir, "id_ed25519", None)?;
    write_ssh_key(dir, "id_ed25519_enc", Some(KEY_PASSPHRASE))?;
    Ok(())
}

fn write_ssh_key(dir: &Path, name: &str, passphrase: Option<&str>) -> Result<(), Box<dyn Error>> {
    let private_path = dir.join(name);
    let public_path = dir.join(format!("{name}.pub"));
    if private_path.exists() && public_path.exists() {
        println!("ssh: {name} up to date");
        return Ok(());
    }
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
    let public = key.public_key().to_openssh()?;
    let stored = match passphrase {
        Some(p) => key.encrypt(&mut OsRng, p)?,
        None => key,
    };
    std::fs::write(&private_path, stored.to_openssh(LineEnding::LF)?.as_bytes())?;
    std::fs::write(&public_path, format!("{public}\n"))?;
    println!("ssh: generated {name}");
    Ok(())
}
