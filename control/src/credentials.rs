use crate::protocol::validate_namespace;
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, path::Path, sync::Mutex};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Capabilities,
    Create,
    Read,
    Renew,
    Release,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Quotas {
    pub inputs: u32,
    pub outputs: u32,
    pub viewers: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeySpec {
    pub label: String,
    pub namespaces: Vec<String>,
    pub scopes: Vec<Scope>,
    pub quotas: Quotas,
    pub expires_at: Option<i64>,
}
impl KeySpec {
    pub fn validate(&self, now: i64) -> Result<(), &'static str> {
        if self.label.trim().is_empty()
            || self.label.len() > 128
            || self.label.chars().any(char::is_control)
            || self.namespaces.is_empty()
            || self.namespaces.len() > 32
            || self
                .namespaces
                .iter()
                .any(|n| validate_namespace(n).is_err())
            || self.scopes.is_empty()
            || self.scopes.len() > 5
            || !(1..=64).contains(&self.quotas.inputs)
            || !(1..=128).contains(&self.quotas.outputs)
            || !(1..=1024).contains(&self.quotas.viewers)
            || self.expires_at.is_some_and(|time| time <= now)
        {
            return Err("invalid_key_configuration");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyRecord {
    pub id: String,
    pub spec: KeySpec,
    pub created_at: i64,
    pub revoked: bool,
}

#[derive(Serialize)]
pub struct IssuedKey {
    pub key: KeyRecord,
    pub secret: String,
}
impl fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedKey")
            .field("id", &self.key.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub key: KeyRecord,
}
impl Principal {
    pub fn require(&self, namespace: &str, scope: Scope) -> Result<(), &'static str> {
        if !self.key.spec.scopes.contains(&scope)
            || !self.key.spec.namespaces.iter().any(|n| n == namespace)
        {
            Err("forbidden")
        } else {
            Ok(())
        }
    }
}

pub struct Credentials {
    db: Mutex<Connection>,
    bootstrap_hash: [u8; 32],
}
impl Credentials {
    pub fn open(path: impl AsRef<Path>, bootstrap: &str) -> Result<Self, &'static str> {
        let db = Connection::open(path).map_err(|_| "credential_store_unavailable")?;
        Self::from_connection(db, bootstrap)
    }
    pub fn from_connection(db: Connection, bootstrap: &str) -> Result<Self, &'static str> {
        if bootstrap.len() < 32
            || bootstrap.len() > 256
            || bootstrap.chars().any(char::is_whitespace)
        {
            return Err("invalid_bootstrap_key");
        }
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS integration_keys(id TEXT PRIMARY KEY,secret_hash BLOB NOT NULL UNIQUE,spec TEXT NOT NULL,created_at INTEGER NOT NULL,revoked INTEGER NOT NULL DEFAULT 0);")
            .map_err(|_|"credential_store_unavailable")?;
        Ok(Self {
            db: Mutex::new(db),
            bootstrap_hash: hash(bootstrap),
        })
    }
    pub fn is_bootstrap(&self, token: &str) -> bool {
        if token.len() > 256 {
            return false;
        }
        let supplied = hash(token);
        self.bootstrap_hash
            .iter()
            .zip(supplied)
            .fold(0u8, |different, (a, b)| different | (a ^ b))
            == 0
    }
    pub fn issue(
        &self,
        bootstrap: &str,
        spec: KeySpec,
        now: i64,
    ) -> Result<IssuedKey, &'static str> {
        if !self.is_bootstrap(bootstrap) {
            return Err("unauthorized");
        }
        spec.validate(now)?;
        let secret = format!("pgk_{}", random_hex(32));
        let id = format!("key_{}", random_hex(16));
        let data = serde_json::to_string(&spec).map_err(|_| "invalid_key_configuration")?;
        self.db
            .lock()
            .map_err(|_| "credential_store_unavailable")?
            .execute(
                "INSERT INTO integration_keys(id,secret_hash,spec,created_at) VALUES(?1,?2,?3,?4)",
                params![id, hash(&secret).as_slice(), data, now],
            )
            .map_err(|_| "credential_store_unavailable")?;
        Ok(IssuedKey {
            key: KeyRecord {
                id,
                spec,
                created_at: now,
                revoked: false,
            },
            secret,
        })
    }
    pub fn authenticate(&self, token: &str, now: i64) -> Result<Principal, &'static str> {
        if token.len() > 256 {
            return Err("unauthorized");
        }
        let row=self.db.lock().map_err(|_|"credential_store_unavailable")?
            .query_row("SELECT id,spec,created_at FROM integration_keys WHERE secret_hash=?1 AND revoked=0",[hash(token).as_slice()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?)))
            .optional().map_err(|_|"credential_store_unavailable")?.ok_or("unauthorized")?;
        let spec: KeySpec =
            serde_json::from_str(&row.1).map_err(|_| "credential_store_unavailable")?;
        if spec.expires_at.is_some_and(|expiry| expiry <= now) {
            return Err("unauthorized");
        }
        Ok(Principal {
            key: KeyRecord {
                id: row.0,
                spec,
                created_at: row.2,
                revoked: false,
            },
        })
    }
    pub fn active(&self, id: &str, now: i64) -> Result<bool, &'static str> {
        let db = self.db.lock().map_err(|_| "credential_store_unavailable")?;
        let row = db
            .query_row(
                "SELECT spec,revoked FROM integration_keys WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
            .map_err(|_| "credential_store_unavailable")?;
        let Some((spec, revoked)) = row else {
            return Ok(false);
        };
        let spec: KeySpec =
            serde_json::from_str(&spec).map_err(|_| "credential_store_unavailable")?;
        Ok(!revoked && !spec.expires_at.is_some_and(|expiry| expiry <= now))
    }
    pub fn list(&self, bootstrap: &str) -> Result<Vec<KeyRecord>, &'static str> {
        if !self.is_bootstrap(bootstrap) {
            return Err("unauthorized");
        }
        let db = self.db.lock().map_err(|_| "credential_store_unavailable")?;
        let mut query=db.prepare("SELECT id,spec,created_at,revoked FROM integration_keys ORDER BY created_at,id LIMIT 1000").map_err(|_|"credential_store_unavailable")?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .map_err(|_| "credential_store_unavailable")?;
        rows.map(|row| {
            let (id, spec, created_at, revoked) =
                row.map_err(|_| "credential_store_unavailable")?;
            Ok(KeyRecord {
                id,
                spec: serde_json::from_str(&spec).map_err(|_| "credential_store_unavailable")?,
                created_at,
                revoked,
            })
        })
        .collect()
    }
    pub fn revoke(&self, bootstrap: &str, id: &str) -> Result<(), &'static str> {
        if !self.is_bootstrap(bootstrap) {
            return Err("unauthorized");
        }
        self.db
            .lock()
            .map_err(|_| "credential_store_unavailable")?
            .execute("UPDATE integration_keys SET revoked=1 WHERE id=?1", [id])
            .map_err(|_| "credential_store_unavailable")?;
        Ok(())
    }
}

pub(crate) fn hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut secret = vec![0; bytes];
    OsRng.fill_bytes(&mut secret);
    secret.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROOT: &str = "synthetic-bootstrap-key-for-tests-only-0000";
    fn spec() -> KeySpec {
        KeySpec {
            label: "Fixture".into(),
            namespaces: vec!["family".into()],
            scopes: vec![Scope::Create, Scope::Read],
            quotas: Quotas {
                inputs: 2,
                outputs: 3,
                viewers: 10,
            },
            expires_at: Some(200),
        }
    }
    #[test]
    fn keys_are_scoped_expiring_revocable_and_never_listed_as_secrets() {
        let registry =
            Credentials::from_connection(Connection::open_in_memory().unwrap(), ROOT).unwrap();
        assert!(registry.issue("wrong", spec(), 100).is_err());
        assert!(registry.authenticate(ROOT, 100).is_err());
        let issued = registry.issue(ROOT, spec(), 100).unwrap();
        let principal = registry.authenticate(&issued.secret, 101).unwrap();
        assert!(principal.require("family", Scope::Create).is_ok());
        assert!(principal.require("stranger", Scope::Create).is_err());
        assert!(principal.require("family", Scope::Release).is_err());
        assert!(!format!("{issued:?}").contains(&issued.secret));
        assert!(!serde_json::to_string(&registry.list(ROOT).unwrap())
            .unwrap()
            .contains(&issued.secret));
        assert!(registry.authenticate(&issued.secret, 200).is_err());
        registry.revoke(ROOT, &issued.key.id).unwrap();
        assert!(registry.authenticate(&issued.secret, 102).is_err());
        assert!(!registry.active(&issued.key.id, 102).unwrap());
    }
    #[test]
    fn credentials_survive_restart_without_storing_plaintext_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keys.sqlite");
        let issued = Credentials::open(&path, ROOT)
            .unwrap()
            .issue(ROOT, spec(), 100)
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes
            .windows(issued.secret.len())
            .any(|slice| slice == issued.secret.as_bytes()));
        assert!(Credentials::open(path, ROOT)
            .unwrap()
            .authenticate(&issued.secret, 101)
            .is_ok());
    }
}
