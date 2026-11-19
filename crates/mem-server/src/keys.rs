use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};

use crate::control::validate_slug;
use crate::{ApiKey, ControlPlane, Role, ServerError};

const KEY_COLUMNS: &str = "id, name, role, projects, expires_at, revoked_at";

impl ControlPlane {
    /// Creates a key and returns its record plus the plaintext, which is shown only once.
    /// `projects = None` grants all projects; `expires_at_ms` is a Unix epoch in milliseconds.
    pub fn create_key(
        &self,
        name: &str,
        role: Role,
        projects: Option<Vec<String>>,
        expires_at_ms: Option<i64>,
    ) -> Result<(ApiKey, String), ServerError> {
        if let Some(list) = &projects {
            list.iter().try_for_each(|p| validate_slug(p))?;
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| ServerError::Internal(format!("rng: {e}")))?;
        let plain = format!("egm_{}", base62(&raw));
        let scope = projects.as_ref().map_or_else(|| "*".to_string(), |p| p.join(","));
        let db = self.db()?;
        db.execute(
            "INSERT INTO api_keys (name, role, projects, key_hash, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![name, role.as_str(), scope, hash(&plain), now(), expires_at_ms],
        )?;
        let key = ApiKey {
            id: db.last_insert_rowid(),
            name: name.to_string(),
            role,
            projects,
            expires_at_ms,
            revoked: false,
        };
        Ok((key, plain))
    }

    /// Resolves a plaintext key; unknown, revoked, and expired keys are `Unauthorized`.
    pub fn verify(&self, plain: &str) -> Result<ApiKey, ServerError> {
        let db = self.db()?;
        let sql = format!("SELECT {KEY_COLUMNS} FROM api_keys WHERE key_hash = ?1");
        let key = db
            .query_row(&sql, [hash(plain)], row_to_key)
            .optional()?
            .ok_or(ServerError::Unauthorized)?;
        if key.revoked || key.expires_at_ms.is_some_and(|t| t <= now()) {
            return Err(ServerError::Unauthorized);
        }
        db.execute("UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2", params![now(), key.id])?;
        Ok(key)
    }

    pub fn list_keys(&self) -> Result<Vec<ApiKey>, ServerError> {
        let db = self.db()?;
        let mut stmt = db.prepare(&format!("SELECT {KEY_COLUMNS} FROM api_keys ORDER BY id"))?;
        let rows = stmt.query_map([], row_to_key)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns `false` when the key does not exist or is already revoked.
    pub fn revoke_key(&self, id: i64) -> Result<bool, ServerError> {
        let changed = self.db()?.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now(), id],
        )?;
        Ok(changed > 0)
    }
}

fn row_to_key(row: &Row<'_>) -> rusqlite::Result<ApiKey> {
    let role: String = row.get(2)?;
    let scope: String = row.get(3)?;
    Ok(ApiKey {
        id: row.get(0)?,
        name: row.get(1)?,
        role: Role::parse(&role).unwrap_or(Role::Bot),
        projects: (scope != "*").then(|| scope.split(',').map(String::from).collect()),
        expires_at_ms: row.get(4)?,
        revoked: row.get::<_, Option<i64>>(5)?.is_some(),
    })
}

fn hash(plain: &str) -> String {
    blake3::hash(plain.as_bytes()).to_hex().to_string()
}

fn now() -> i64 {
    Utc::now().timestamp_millis()
}

/// Big-endian base62 encoding of `bytes`.
fn base62(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut num = bytes.to_vec();
    let mut out = Vec::with_capacity(44);
    while num.iter().any(|&b| b != 0) {
        let mut rem = 0u32;
        for b in num.iter_mut() {
            let acc = (rem << 8) | u32::from(*b);
            *b = (acc / 62) as u8;
            rem = acc % 62;
        }
        out.push(ALPHABET[rem as usize]);
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}
