//! API-key authentication (D12). Keys are configured as SHA-256 hashes, so
//! plaintext keys are never stored; presented keys are hashed and looked up.

use std::collections::HashMap;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::AuthConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Queries, counts and explain; reading the schema.
    Read,
    /// Everything `read` can do, plus writes.
    ReadWrite,
    /// Everything, including schema changes and maintenance.
    Admin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No or unknown credentials.
    Unauthenticated,
    /// Valid credentials without the required role.
    Forbidden { need: Role },
}

#[derive(Debug, Default)]
pub struct Auth {
    disabled: bool,
    keys: HashMap<[u8; 32], Principal>,
}

pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

impl Auth {
    pub fn new(cfg: &AuthConfig) -> Self {
        let keys = cfg
            .keys
            .iter()
            .filter_map(|k| {
                let bytes: [u8; 32] = hex::decode(&k.sha256).ok()?.try_into().ok()?;
                Some((
                    bytes,
                    Principal {
                        name: k.name.clone(),
                        role: k.role,
                    },
                ))
            })
            .collect();
        Self {
            disabled: cfg.disabled,
            keys,
        }
    }

    /// Authenticate an `Authorization` header value and check the role.
    pub fn check(&self, authorization: Option<&str>, need: Role) -> Result<Principal, AuthError> {
        if self.disabled {
            return Ok(Principal {
                name: "anonymous".into(),
                role: Role::Admin,
            });
        }
        let token = authorization
            .and_then(|h| {
                h.strip_prefix("Bearer ")
                    .or_else(|| h.strip_prefix("bearer "))
            })
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::Unauthenticated)?;
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let p = self.keys.get(&digest).ok_or(AuthError::Unauthenticated)?;
        if p.role < need {
            return Err(AuthError::Forbidden { need });
        }
        Ok(p.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKey;

    #[test]
    fn roles_are_enforced() {
        let auth = Auth::new(&AuthConfig {
            disabled: false,
            keys: vec![ApiKey {
                name: "app".into(),
                sha256: hash_key("secret"),
                role: Role::ReadWrite,
            }],
        });
        assert_eq!(
            auth.check(Some("Bearer secret"), Role::Read).unwrap().name,
            "app"
        );
        assert!(auth.check(Some("Bearer secret"), Role::ReadWrite).is_ok());
        assert_eq!(
            auth.check(Some("Bearer secret"), Role::Admin),
            Err(AuthError::Forbidden { need: Role::Admin })
        );
        assert_eq!(
            auth.check(Some("Bearer nope"), Role::Read),
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(
            auth.check(None, Role::Read),
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(
            auth.check(Some("secret"), Role::Read),
            Err(AuthError::Unauthenticated)
        );
    }
}
