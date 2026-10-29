use std::fmt;

use ring::{
    aead, hmac,
    rand::{SecureRandom, SystemRandom},
};

use crate::{SyncError, MAX_ENCRYPTED_OBJECT_BYTES};

/// Random high-entropy recovery material, independent of the DAV password.
#[derive(Clone)]
pub struct RecoveryKey(pub(crate) [u8; 32]);

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey([REDACTED])")
    }
}

impl RecoveryKey {
    pub fn generate() -> Result<Self, SyncError> {
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| SyncError::InvalidData("random_source"))?;
        Ok(Self(bytes))
    }

    pub fn from_hex(text: &str) -> Result<Self, SyncError> {
        if text.len() != 64 {
            return Err(SyncError::InvalidData("recovery_key"));
        }
        let mut key = [0u8; 32];
        for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
            let nibble = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            key[index] = (nibble(pair[0]).ok_or(SyncError::InvalidData("recovery_key"))? << 4)
                | nibble(pair[1]).ok_or(SyncError::InvalidData("recovery_key"))?;
        }
        Ok(Self(key))
    }

    /// Only explicitly export for a protected recovery file. Never log this.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    pub(crate) fn keyed_id(&self, domain: &str, bytes: &[u8]) -> String {
        let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, &self.0));
        context.update(b"cliphost-webdav-v1\0");
        context.update(domain.as_bytes());
        context.update(b"\0");
        context.update(bytes);
        hex(context.sign().as_ref())
    }

    fn cipher(&self, collection: &str) -> Result<aead::LessSafeKey, SyncError> {
        let derived = self.keyed_id("encryption-key", collection.as_bytes());
        let raw = Self::from_hex(&derived)?;
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &raw.0)
            .map_err(|_| SyncError::InvalidData("encryption_key"))?;
        Ok(aead::LessSafeKey::new(key))
    }

    pub(crate) fn seal(
        &self,
        collection: &str,
        kind: &str,
        id: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        if plaintext.len() > MAX_ENCRYPTED_OBJECT_BYTES - 29 {
            return Err(SyncError::LimitExceeded("object_bytes"));
        }
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| SyncError::InvalidData("random_source"))?;
        let aad = format!("cliphost-webdav-v1\0{collection}\0{kind}\0{id}");
        let mut ciphertext = plaintext.to_vec();
        self.cipher(collection)?
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_bytes()),
                &mut ciphertext,
            )
            .map_err(|_| SyncError::InvalidData("encryption_failed"))?;
        let mut output = Vec::with_capacity(13 + ciphertext.len());
        output.push(1);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    pub(crate) fn open(
        &self,
        collection: &str,
        kind: &str,
        id: &str,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        if ciphertext.len() < 29
            || ciphertext.len() > MAX_ENCRYPTED_OBJECT_BYTES
            || ciphertext[0] != 1
        {
            return Err(SyncError::InvalidData("encrypted_object"));
        }
        let nonce: [u8; 12] = ciphertext[1..13]
            .try_into()
            .map_err(|_| SyncError::InvalidData("nonce"))?;
        let aad = format!("cliphost-webdav-v1\0{collection}\0{kind}\0{id}");
        let mut buffer = ciphertext[13..].to_vec();
        let plaintext = self
            .cipher(collection)?
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_bytes()),
                &mut buffer,
            )
            .map_err(|_| SyncError::KeyMismatch)?;
        Ok(plaintext.to_vec())
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
