//! 会場 ⇄ 中継サーバの mTLS(QUIC)用の私設 CA と証明書
//!
//! `dcr-transcribe --gen-uplink-pki <出力先> <中継サーバの IP か名前>[,...]` で次を作る:
//!
//! | ファイル | 置き場所 |
//! |---|---|
//! | `ca.pem` | 会場と中継サーバの両方(相手の証明書を確かめる) |
//! | `ca.key` | **どちらにも置かない**(作り直すときだけ使う。オフラインで保管) |
//! | `relay.pem` / `relay.key` | 中継サーバ(サーバ証明書。SAN に中継サーバの IP / 名前) |
//! | `venue.pem` / `venue.key` | 会場(クライアント証明書) |
//!
//! - 鍵は ECDSA P-256、TLS は 1.3 だけ、鍵交換は X25519MLKEM768(耐量子のハイブリッド)だけ。証明書は公開の CT ログに載らないので、IP を SAN にすれば DNS に名前を出さずに済む
//! - CA はこの用途専用(パス長 0)。CA の証明書で署名されたものは「会場」とみなすので、漏れたら CA ごと作り直す

use anyhow::{Context, Result};
use chrono::Datelike;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;

/// QUIC の ALPN(別の用途の QUIC と取り違えないため)
pub const ALPN: &[u8] = b"dcr-uplink/1";

/// 私設 CA と、中継サーバ・会場の証明書を作って `out` に書く
pub fn generate(out: &Path, server_names: &[String], days: i64) -> Result<()> {
    anyhow::ensure!(!server_names.is_empty(), "中継サーバの IP か名前を 1 つ以上指定してください");
    std::fs::create_dir_all(out).with_context(|| format!("ディレクトリを作れません: {:?}", out))?;
    for name in ["ca.key", "relay.key", "venue.key"] {
        anyhow::ensure!(!out.join(name).exists(), "{:?} がすでにあります(上書きしません)", out.join(name));
    }
    let set = issue(server_names, days)?;
    write(out, "ca.pem", &set.ca_pem, false)?;
    write(out, "ca.key", &set.ca_key, true)?;
    write(out, "relay.pem", &set.relay_pem, false)?;
    write(out, "relay.key", &set.relay_key, true)?;
    write(out, "venue.pem", &set.venue_pem, false)?;
    write(out, "venue.key", &set.venue_key, true)?;
    Ok(())
}

/// 作った証明書と鍵(PEM)
pub(crate) struct Issued {
    pub(crate) ca_pem: String,
    pub(crate) ca_key: String,
    pub(crate) relay_pem: String,
    pub(crate) relay_key: String,
    pub(crate) venue_pem: String,
    pub(crate) venue_key: String,
}

pub(crate) fn issue(server_names: &[String], days: i64) -> Result<Issued> {
    let validity = |p: &mut CertificateParams, days: i64| {
        let now = chrono::Utc::now() - chrono::Duration::days(1);
        let end = now + chrono::Duration::days(days);
        p.not_before = rcgen::date_time_ymd(now.year(), now.month() as u8, now.day() as u8);
        p.not_after = rcgen::date_time_ymd(end.year(), end.month() as u8, end.day() as u8);
    };
    let name = |cn: &str| {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "dcr-transcribe");
        dn.push(DnType::CommonName, cn);
        dn
    };

    let ca_key = KeyPair::generate().context("CA の鍵を作れません")?;
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name = name("dcr-uplink CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    validity(&mut ca, days + 30);
    let ca_cert = ca.self_signed(&ca_key).context("CA の証明書を作れません")?;
    let issuer = Issuer::from_params(&ca, &ca_key);

    let relay_key = KeyPair::generate()?;
    let mut relay = CertificateParams::new(server_names.to_vec()).context("中継サーバの IP / 名前が不正です")?;
    relay.distinguished_name = name("dcr-relay");
    relay.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    relay.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    validity(&mut relay, days);
    let relay_cert = relay.signed_by(&relay_key, &issuer)?;

    let venue_key = KeyPair::generate()?;
    let mut venue = CertificateParams::new(Vec::<String>::new())?;
    venue.distinguished_name = name("dcr-venue");
    venue.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    venue.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    validity(&mut venue, days);
    let venue_cert = venue.signed_by(&venue_key, &issuer)?;

    Ok(Issued {
        ca_pem: ca_cert.pem(),
        ca_key: ca_key.serialize_pem(),
        relay_pem: relay_cert.pem(),
        relay_key: relay_key.serialize_pem(),
        venue_pem: venue_cert.pem(),
        venue_key: venue_key.serialize_pem(),
    })
}

fn write(dir: &Path, name: &str, content: &str, secret: bool) -> Result<()> {
    use std::io::Write;
    let path = dir.join(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, if secret { 0o600 } else { 0o644 });
    opts.open(&path)
        .and_then(|mut f| f.write_all(content.as_bytes()))
        .with_context(|| format!("書けません: {:?}", path))
}

/// 鍵交換は耐量子のハイブリッド(X25519 + ML-KEM-768)だけ。両端とも自分たちなので、ほかの方式へは落とさない
/// (通信を保存しておいて将来の量子計算機で解く攻撃に備える)
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    let mut p = rustls::crypto::aws_lc_rs::default_provider();
    p.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519MLKEM768];
    Arc::new(p)
}

fn certs(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("{} を読めません", what))?;
    anyhow::ensure!(!certs.is_empty(), "{} に証明書がありません", what);
    Ok(certs)
}

fn key(pem: &[u8], what: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem).with_context(|| format!("{} を読めません", what))
}

fn read(path: &str, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("{} を読めません: {}", what, path))
}

/// QUIC の接続の設定(両側で同じ)。4G の NAT は UDP を早く忘れるので keep-alive を短くする
pub(crate) fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    t.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(20_000))));
    t.datagram_receive_buffer_size(Some(256 * 1024));
    t.datagram_send_buffer_size(256 * 1024);
    Arc::new(t)
}

/// 中継サーバの QUIC 設定: サーバ証明書を出し、CA が署名したクライアント証明書だけを受け入れる
pub(crate) fn server_config(cert_pem: &[u8], key_pem: &[u8], client_ca_pem: &[u8]) -> Result<quinn::ServerConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in certs(client_ca_pem, "会場の CA")? {
        roots.add(c).context("会場の CA を登録できません")?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .context("クライアント証明書の検証器を作れません")?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs(cert_pem, "中継サーバの証明書")?, key(key_pem, "中継サーバの鍵")?)
        .context("中継サーバの証明書と鍵が合いません")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT は使わない(再送攻撃を考えなくて済むように)
    tls.max_early_data_size = 0;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    // 会場の IP が変わっても接続を続ける
    config.migration(true);
    Ok(config)
}

/// 会場の QUIC 設定: CA が署名したサーバ証明書だけを信用し、クライアント証明書を出す
pub(crate) fn client_config(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<quinn::ClientConfig> {
    client_config_with(provider(), ca_pem, cert_pem, key_pem)
}

fn client_config_with(
    provider: Arc<rustls::crypto::CryptoProvider>,
    ca_pem: &[u8],
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<quinn::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in certs(ca_pem, "中継サーバの CA")? {
        roots.add(c).context("中継サーバの CA を登録できません")?;
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs(cert_pem, "会場の証明書")?, key(key_pem, "会場の鍵")?)
        .context("会場の証明書と鍵が合いません")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    Ok(config)
}

/// 設定ファイルのパスから中継サーバの QUIC 設定を作る
pub fn server_config_from_files(cert: &str, key: &str, client_ca: &str) -> Result<quinn::ServerConfig> {
    server_config(
        &read(cert, "中継サーバの証明書")?,
        &read(key, "中継サーバの鍵")?,
        &read(client_ca, "会場の CA")?,
    )
}

/// 設定ファイルのパスから会場の QUIC 設定を作る
pub fn client_config_from_files(ca: &str, cert: &str, key: &str) -> Result<quinn::ClientConfig> {
    client_config(&read(ca, "中継サーバの CA")?, &read(cert, "会場の証明書")?, &read(key, "会場の鍵")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 耐量子の鍵交換に対応しない相手(X25519 だけ)とはつながらない
    #[tokio::test]
    async fn classic_key_exchange_is_refused() {
        let set = issue(&["127.0.0.1".into()], 30).unwrap();
        let server = server_config(set.relay_pem.as_bytes(), set.relay_key.as_bytes(), set.ca_pem.as_bytes()).unwrap();
        let endpoint = quinn::Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        let _ = conn.accept_bi().await;
                    }
                });
            }
        });
        let connect = |provider: Arc<rustls::crypto::CryptoProvider>| {
            let client = client_config_with(provider, set.ca_pem.as_bytes(), set.venue_pem.as_bytes(), set.venue_key.as_bytes())
                .unwrap();
            async move {
                let ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                let r = tokio::time::timeout(std::time::Duration::from_secs(3), ep.connect_with(client, addr, "127.0.0.1").unwrap()).await;
                matches!(r, Ok(Ok(_)))
            }
        };
        assert!(connect(provider()).await, "耐量子の鍵交換ならつながる");
        let mut classic = rustls::crypto::aws_lc_rs::default_provider();
        classic.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519];
        assert!(!connect(Arc::new(classic)).await, "X25519 だけの相手は断る");
    }

    #[test]
    fn issued_set_builds_configs_and_refuses_overwrite() {
        let set = issue(&["127.0.0.1".into(), "relay.example.com".into()], 30).unwrap();
        server_config(set.relay_pem.as_bytes(), set.relay_key.as_bytes(), set.ca_pem.as_bytes()).unwrap();
        client_config(set.ca_pem.as_bytes(), set.venue_pem.as_bytes(), set.venue_key.as_bytes()).unwrap();
        // 鍵と証明書の取り違えは起動時に分かる
        assert!(client_config(set.ca_pem.as_bytes(), set.venue_pem.as_bytes(), set.relay_key.as_bytes()).is_err());

        let dir = tempfile::tempdir().unwrap();
        generate(dir.path(), &["127.0.0.1".into()], 30).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("venue.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(generate(dir.path(), &["127.0.0.1".into()], 30).is_err(), "上書きしない");
    }
}
