//! Windows software RSA generation, matching the platform used by C# RSA.Create.
//! Keys are unnamed/ephemeral; only the existing isolated identity writer persists them.
use windows::Win32::Security::Cryptography::*;

struct Handle(NCRYPT_HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = NCryptFreeObject(self.0);
        }
    }
}

pub(crate) fn rsa_3072_key_pair() -> Result<rcgen::KeyPair, String> {
    let der = export_ephemeral_key().map_err(|error| error.to_string())?;
    let secret = rsa::pkcs8::SecretDocument::try_from(der).map_err(|error| error.to_string())?;
    rcgen::KeyPair::try_from(secret.as_bytes()).map_err(|error| error.to_string())
}

fn export_ephemeral_key() -> windows::core::Result<Vec<u8>> {
    unsafe {
        let mut provider = NCRYPT_PROV_HANDLE::default();
        NCryptOpenStorageProvider(&mut provider, MS_KEY_STORAGE_PROVIDER, 0)?;
        let _provider = Handle(NCRYPT_HANDLE(provider.0));
        let mut key = NCRYPT_KEY_HANDLE::default();
        // NULL key name is essential: no machine/user key container is created.
        // https://learn.microsoft.com/windows/win32/api/ncrypt/nf-ncrypt-ncryptcreatepersistedkey
        NCryptCreatePersistedKey(
            provider,
            &mut key,
            BCRYPT_RSA_ALGORITHM,
            windows::core::PCWSTR::null(),
            CERT_KEY_SPEC(0),
            NCRYPT_FLAGS(0),
        )?;
        let key_handle = Handle(NCRYPT_HANDLE(key.0));
        NCryptSetProperty(
            key_handle.0,
            NCRYPT_LENGTH_PROPERTY,
            &3072u32.to_le_bytes(),
            NCRYPT_FLAGS(0),
        )?;
        let export_policy = NCRYPT_ALLOW_EXPORT_FLAG | NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG;
        NCryptSetProperty(
            key_handle.0,
            NCRYPT_EXPORT_POLICY_PROPERTY,
            &export_policy.to_le_bytes(),
            NCRYPT_FLAGS(0),
        )?;
        NCryptFinalizeKey(key, NCRYPT_SILENT_FLAG)?;
        let mut length = 0;
        NCryptExportKey(
            key,
            NCRYPT_KEY_HANDLE::default(),
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            None,
            None,
            &mut length,
            NCRYPT_SILENT_FLAG,
        )?;
        // A 3072-bit PKCS#8 key is small. Reject a bad provider result before allocation.
        if length == 0 || length > 16 * 1024 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "Invalid RSA export size",
            ));
        }
        let mut der = vec![0; length as usize];
        NCryptExportKey(
            key,
            NCRYPT_KEY_HANDLE::default(),
            NCRYPT_PKCS8_PRIVATE_KEY_BLOB,
            None,
            Some(&mut der),
            &mut length,
            NCRYPT_SILENT_FLAG,
        )?;
        der.truncate(length as usize);
        Ok(der)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{pkcs8::DecodePrivateKey, traits::PublicKeyParts, RsaPrivateKey};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use std::io::{Cursor, Read, Write};
    use std::sync::Arc;

    #[test]
    fn windows_rsa_key_supports_real_tls_without_network_or_key_container() {
        let started = std::time::Instant::now();
        // Call the native implementation directly: fallback must not hide a broken CNG path.
        let key = rsa_3072_key_pair().expect("Windows software RSA key");
        eprintln!(
            "Windows RSA-3072 generation: {} ms",
            started.elapsed().as_millis()
        );
        let parsed = RsaPrivateKey::from_pkcs8_der(&key.serialize_der()).unwrap();
        assert_eq!(parsed.n().bits(), 3072);
        parsed.validate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let cert = CertificateDer::from(certificate.der().to_vec());
        let saved = crate::lan::LanIdentity {
            device_id: "native-rsa-fixture".into(),
            fingerprint: crate::lan::sha256_hex(cert.as_ref()),
            cert_der: cert.to_vec(),
            key_der: key.serialize_der(),
        };
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.json");
        std::fs::write(&identity_path, serde_json::to_vec(&saved).unwrap()).unwrap();
        std::fs::write(dir.path().join("cert.der"), &saved.cert_der).unwrap();
        std::fs::write(dir.path().join("key.der"), &saved.key_der).unwrap();
        let loaded = crate::lan::LanIdentity::load_or_create(&identity_path);
        assert_eq!(loaded.device_id, saved.device_id);
        assert_eq!(loaded.fingerprint, saved.fingerprint);
        assert_eq!(loaded.cert_der, saved.cert_der);
        assert_eq!(loaded.key_der, saved.key_der);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = rustls::ClientConnection::new(
            Arc::new(client),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server)).unwrap();
        for _ in 0..8 {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            server.read_tls(&mut Cursor::new(bytes)).unwrap();
            server.process_new_packets().unwrap();
            let mut bytes = Vec::new();
            server.write_tls(&mut bytes).unwrap();
            client.read_tls(&mut Cursor::new(bytes)).unwrap();
            client.process_new_packets().unwrap();
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }
        assert!(!client.is_handshaking() && !server.is_handshaking());
        client.writer().write_all(b"LAN fixture").unwrap();
        let mut bytes = Vec::new();
        client.write_tls(&mut bytes).unwrap();
        server.read_tls(&mut Cursor::new(bytes)).unwrap();
        server.process_new_packets().unwrap();
        let mut received = [0; 11];
        server.reader().read_exact(&mut received).unwrap();
        assert_eq!(&received, b"LAN fixture");
    }
}
