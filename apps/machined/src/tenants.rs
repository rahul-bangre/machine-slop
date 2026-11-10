use std::collections::HashMap;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use slop_machines::{Quotas, TenantConfig};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantsFile {
    tenants: Vec<TenantEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantEntry {
    id: String,
    api_key_sha256: Vec<String>,
    quotas: Quotas,
}

pub struct ApiKeys(HashMap<[u8; 32], String>);

impl ApiKeys {
    pub fn tenant_of(&self, key: &str) -> Option<&str> {
        self.0
            .get(&<[u8; 32]>::from(Sha256::digest(key.as_bytes())))
            .map(String::as_str)
    }
}

pub fn parse(json: &str) -> Result<(Vec<TenantConfig>, ApiKeys), String> {
    let file: TenantsFile =
        serde_json::from_str(json).map_err(|error| format!("tenants file: {error}"))?;
    let mut keys = HashMap::new();
    let mut tenants = Vec::new();
    for entry in file.tenants {
        for hash in &entry.api_key_sha256 {
            let digest = decode_sha256(hash).ok_or_else(|| {
                format!(
                    "tenant {}: {hash:?} is not a hex SHA-256 digest of an API key",
                    entry.id
                )
            })?;
            if let Some(holder) = keys.insert(digest, entry.id.clone()) {
                return Err(format!(
                    "tenants {holder} and {} share an API key",
                    entry.id
                ));
            }
        }
        tenants.push(TenantConfig {
            id: entry.id,
            quotas: entry.quotas,
        });
    }
    Ok((tenants, ApiKeys(keys)))
}

fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    let mut digest = [0; 32];
    if hex.len() != 64 {
        return None;
    }
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_SHA256: &str = "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

    fn file(tenants: &str) -> String {
        format!(r#"{{ "tenants": [{tenants}] }}"#)
    }

    fn tenant(id: &str, hash: &str) -> String {
        format!(
            r#"{{ "id": "{id}", "api_key_sha256": ["{hash}"], "quotas": {{ "running_machines": 1, "memory_mib": 512, "disk_gib": 10, "creates_per_second": 1 }} }}"#
        )
    }

    #[test]
    fn a_key_names_the_tenant_whose_digest_it_matches() {
        let (tenants, keys) = parse(&file(&tenant("acme", KEY_SHA256))).expect("parse");
        assert_eq!(tenants.len(), 1);
        assert_eq!(tenants[0].quotas.memory_mib, 512);
        assert_eq!(keys.tenant_of("secret"), Some("acme"));
        assert_eq!(keys.tenant_of("Secret"), None);
        assert_eq!(keys.tenant_of(""), None);
    }

    #[test]
    fn malformed_or_shared_keys_are_rejected() {
        assert!(parse(&file(&tenant("acme", "not hex"))).is_err());
        assert!(parse(&file(&tenant("acme", &KEY_SHA256[..62]))).is_err());
        let shared = format!("{},{}", tenant("a", KEY_SHA256), tenant("b", KEY_SHA256));
        assert!(parse(&file(&shared)).is_err());
        assert!(parse(r#"{ "tenants": [], "extra": 1 }"#).is_err());
    }
}
