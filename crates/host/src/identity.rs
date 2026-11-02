use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hmac::{Hmac, Mac};
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey, VerifyingKey,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub fn public_key(key: &SigningKey) -> String {
    B64.encode(key.verifying_key().to_encoded_point(false).as_bytes())
}
pub fn device_id(key: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(B64.decode(key)?)))
}
pub fn random_nonce() -> String {
    let mut b = [0; 32];
    OsRng.fill_bytes(&mut b);
    B64.encode(b)
}
pub fn transcript(parts: &[&str]) -> Vec<u8> {
    serde_json::to_vec(parts).expect("string array serializes")
}
pub fn sign(key: &SigningKey, parts: &[&str]) -> String {
    let signature: Signature = key.sign(&transcript(parts));
    B64.encode(signature.to_bytes())
}
pub fn verify(key: &str, signature: &str, parts: &[&str]) -> Result<()> {
    let key = VerifyingKey::from_sec1_bytes(&B64.decode(key)?)?;
    let signature = Signature::from_slice(&B64.decode(signature)?)?;
    key.verify(&transcript(parts), &signature)
        .context("Invalid peer signature")
}
pub fn mac(secret: &[u8], parts: &[&str]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&transcript(parts));
    B64.encode(mac.finalize().into_bytes())
}
pub fn verify_mac(secret: &[u8], proof: &str, parts: &[&str]) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
    mac.update(&transcript(parts));
    mac.verify_slice(&B64.decode(proof)?)
        .context("Invalid pairing proof")
}

pub struct Pairing {
    secret: Zeroizing<[u8; 32]>,
    created: Instant,
    consumed: bool,
}
impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}
impl Pairing {
    pub fn new() -> Self {
        let mut secret = [0; 32];
        OsRng.fill_bytes(&mut secret);
        Self {
            secret: Zeroizing::new(secret),
            created: Instant::now(),
            consumed: false,
        }
    }
    pub fn secret_base64(&self) -> String {
        B64.encode(self.secret.as_ref())
    }
    pub fn accept(
        &mut self,
        host: &str,
        id: &str,
        key: &str,
        nonce: &str,
        proof: &str,
    ) -> Result<()> {
        if self.consumed || self.created.elapsed() >= Duration::from_secs(300) {
            bail!("Pairing link expired or already used");
        }
        if device_id(key)? != id || nonce.len() > 100 {
            bail!("Invalid browser identity");
        }
        VerifyingKey::from_sec1_bytes(&B64.decode(key)?)?;
        verify_mac(
            self.secret.as_ref(),
            proof,
            &["lantern-pair-v1", host, id, key, nonce],
        )?;
        // The caller persists the pairing before acknowledging it. Failed persistence must
        // not leave a success response or an authorized-but-unrecorded peer.
        Ok(())
    }
    pub fn acknowledgement(&self, host: &str, host_key: &str, client: &str, nonce: &str) -> String {
        mac(
            self.secret.as_ref(),
            &["lantern-pair-ok-v1", host, host_key, client, nonce],
        )
    }
    pub fn consume(&mut self) {
        self.consumed = true;
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PairedBrowser {
    pub name: String,
    pub key: String,
}

pub trait IdentityStore {
    fn signing_key(&self) -> &SigningKey;
    fn reload_pairs(&mut self) -> Result<()>;
    fn persist_pairs(&self) -> Result<()>;
}
pub struct Store {
    key: SigningKey,
    pub pairs: BTreeMap<String, PairedBrowser>,
    path: PathBuf,
}
impl Store {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let account = hex::encode(Sha256::digest(
            dir.canonicalize()?.to_string_lossy().as_bytes(),
        ));
        let entry = keyring::Entry::new("dev.lantern.host", &account)?;
        let key = match entry.get_secret() {
            Ok(secret) => SigningKey::from_slice(&Zeroizing::new(secret))?,
            Err(keyring::Error::NoEntry) => {
                let key = SigningKey::random(&mut OsRng);
                entry
                    .set_secret(key.to_bytes().as_ref())
                    .context("Cannot save identity in the OS credential store")?;
                key
            }
            Err(e) => return Err(e).context("Cannot read identity from the OS credential store"),
        };
        let mut store = Self {
            key,
            pairs: BTreeMap::new(),
            path: dir.join("paired-browsers.json"),
        };
        store.reload_pairs()?;
        Ok(store)
    }
    pub fn host_key(&self) -> String {
        public_key(&self.key)
    }
    pub fn host_id(&self) -> String {
        device_id(&self.host_key()).expect("own public key")
    }
    #[cfg(test)]
    pub fn temporary(path: PathBuf) -> Self {
        Self {
            key: SigningKey::random(&mut OsRng),
            pairs: BTreeMap::new(),
            path,
        }
    }
}
impl IdentityStore for Store {
    fn signing_key(&self) -> &SigningKey {
        &self.key
    }
    fn reload_pairs(&mut self) -> Result<()> {
        self.pairs = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("Paired browser file is corrupt; refusing to reset trust")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(())
    }
    fn persist_pairs(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(
            self.path.parent().context("Missing store directory")?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&serde_json::to_vec_pretty(&self.pairs)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verifies_browser_webcrypto_wire_format_and_utf8() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/webcrypto-vector.json")).unwrap();
        let parts: Vec<&str> = vector["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        verify(
            vector["hostKey"].as_str().unwrap(),
            vector["signature"].as_str().unwrap(),
            &parts,
        )
        .unwrap();
        let secret = B64.decode(vector["secret"].as_str().unwrap()).unwrap();
        assert_eq!(mac(&secret, &parts), vector["proof"].as_str().unwrap());
    }
    #[test]
    fn signatures_bind_entire_transcript_and_peer() {
        let key = SigningKey::random(&mut OsRng);
        let pubkey = public_key(&key);
        let sig = sign(&key, &["offer", "nonce", "sdp-fingerprint"]);
        assert!(verify(&pubkey, &sig, &["offer", "nonce", "sdp-fingerprint"]).is_ok());
        assert!(verify(&pubkey, &sig, &["offer", "new-nonce", "sdp-fingerprint"]).is_err());
        assert!(verify(&pubkey, &sig, &["offer", "nonce", "substituted-sdp"]).is_err());
    }
    #[test]
    fn pairing_expires_is_single_use_and_rejects_wrong_proof() {
        let key = public_key(&SigningKey::random(&mut OsRng));
        let id = device_id(&key).unwrap();
        let mut p = Pairing::new();
        let nonce = random_nonce();
        let proof = mac(
            p.secret.as_ref(),
            &["lantern-pair-v1", "host", &id, &key, &nonce],
        );
        assert!(p.accept("host", &id, &key, &nonce, "bad").is_err());
        assert!(p.accept("host", &id, &key, &nonce, &proof).is_ok());
        p.consume();
        assert!(p.accept("host", &id, &key, &nonce, &proof).is_err());
        let mut p = Pairing::new();
        p.created -= Duration::from_secs(301);
        assert!(p.accept("host", &id, &key, &nonce, &proof).is_err());
    }
    #[test]
    fn persists_and_revokes_without_resetting_corrupt_trust() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::temporary(dir.path().join("pairs.json"));
        store.pairs.insert(
            "browser".into(),
            PairedBrowser {
                name: "iPad".into(),
                key: "key".into(),
            },
        );
        store.persist_pairs().unwrap();
        store.pairs.clear();
        store.reload_pairs().unwrap();
        assert_eq!(store.pairs.len(), 1);
        store.pairs.clear();
        store.persist_pairs().unwrap();
        store.reload_pairs().unwrap();
        assert!(store.pairs.is_empty());
        std::fs::write(&store.path, "corrupt").unwrap();
        assert!(store.reload_pairs().is_err());
    }
}
