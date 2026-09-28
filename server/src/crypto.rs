use std::sync::{Arc, LazyLock};
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, PasswordHasher as _, PasswordVerifier as _, Version};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

/// Random token as URL-safe base64 without padding.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

#[derive(Debug)]
pub struct HasherBusy;

/// Argon2id hashing on the blocking pool, with a limit on parallel hashes.
#[derive(Clone)]
pub struct PasswordHasher {
    slots: Arc<Semaphore>,
}

const PARALLEL_HASHES: usize = 4;
const SLOT_WAIT: Duration = Duration::from_secs(2);

fn argon2() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Verified against when the user does not exist, so both cases take the same time.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    argon2()
        .hash_password(random_token(16).as_bytes())
        .expect("hashing a random password works")
        .to_string()
});

impl PasswordHasher {
    pub fn new() -> Self {
        LazyLock::force(&DUMMY_HASH);
        Self {
            slots: Arc::new(Semaphore::new(PARALLEL_HASHES)),
        }
    }

    pub async fn hash(&self, password: String) -> Result<String, HasherBusy> {
        self.run(move || {
            argon2()
                .hash_password(password.as_bytes())
                .expect("argon2 hashing does not fail")
                .to_string()
        })
        .await
    }

    /// With `hash` = `None`, it checks against a dummy hash and returns `false`.
    pub async fn verify(&self, hash: Option<String>, password: String) -> Result<bool, HasherBusy> {
        self.run(move || {
            let known = hash.is_some();
            let hash = hash.unwrap_or_else(|| DUMMY_HASH.clone());
            let ok = argon2().verify_password(password.as_bytes(), hash.as_str()).is_ok();
            ok && known
        })
        .await
    }

    async fn run<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> Result<T, HasherBusy> {
        let permit = tokio::time::timeout(SLOT_WAIT, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| HasherBusy)?
            .expect("semaphore is never closed");
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job()
        })
        .await
        .expect("hash task does not panic");
        Ok(result)
    }
}
