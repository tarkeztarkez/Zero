use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

#[derive(Clone)]
pub struct Crypto {
    cipher: Aes256Gcm,
}

impl Crypto {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: Aes256Gcm::new_from_slice(key).expect("32-byte key") }
    }

    /// Encrypts to base64(nonce || ciphertext).
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let nonce_bytes: [u8; 12] = random_bytes();
        let ciphertext = self
            .cipher
            .encrypt(&Nonce::from(nonce_bytes), plaintext.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = nonce_bytes.to_vec();
        out.extend(ciphertext);
        Ok(STANDARD.encode(out))
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let data = STANDARD.decode(encoded)?;
        if data.len() < 12 {
            return Err(anyhow!("ciphertext too short"));
        }
        let (nonce, ciphertext) = data.split_at(12);
        let plaintext = self
            .cipher
            .decrypt(&Nonce::try_from(nonce).map_err(|_| anyhow!("bad nonce"))?, ciphertext)
            .map_err(|_| anyhow!("decryption failed"))?;
        Ok(String::from_utf8(plaintext)?)
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom_fill(&mut buf);
    buf
}

fn getrandom_fill(buf: &mut [u8]) {
    use rand::RngExt;
    rand::rng().fill(buf);
}

pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
