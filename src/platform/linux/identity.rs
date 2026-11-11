//! Read-only physical connector identity for durable KScreen recovery leases.
use crate::model::{DisplayError, DisplayResult};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

const MAX_EDID_BYTES: usize = 64 * 1024;
const MAX_UUID_BYTES: usize = 512;

/// KScreen commonly omits UUID/EDID. In that case bind the unique connected
/// DRM connector to its canonical device node and observed EDID. Unknown or
/// ambiguous hardware identity stays explicit and cannot authorize mutation.
pub(super) fn monitor_identity(output: &Value) -> DisplayResult<Option<String>> {
    identity_with_drm(output, Path::new("/sys/class/drm"))
}

fn identity_with_drm(output: &Value, drm_root: &Path) -> DisplayResult<Option<String>> {
    let name = output.get("name").and_then(Value::as_str).ok_or_else(|| {
        DisplayError::new("invalid_snapshot", "KScreen connector name is unavailable")
    })?;
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(DisplayError::new(
            "invalid_snapshot",
            "invalid KScreen connector name",
        ));
    }
    if let Some(uuid) = output
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
    {
        if uuid.len() > MAX_UUID_BYTES || uuid.chars().any(char::is_control) {
            return Err(DisplayError::new(
                "invalid_snapshot",
                "invalid KScreen hardware identity",
            ));
        }
        return Ok(Some(digest_identity(
            b"kscreen-uuid-v1",
            name.as_bytes(),
            uuid.as_bytes(),
        )));
    }
    Ok(drm_identity(drm_root, name))
}

fn drm_identity(drm_root: &Path, connector: &str) -> Option<String> {
    let suffix = format!("-{connector}");
    let mut matched = None;
    for entry in fs::read_dir(drm_root).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        let Some(card) = name
            .strip_suffix(&suffix)
            .and_then(|name| name.strip_prefix("card"))
        else {
            continue;
        };
        if card.is_empty() || !card.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let node = entry.path();
        let mut status = String::new();
        fs::File::open(node.join("status"))
            .ok()?
            .take(32)
            .read_to_string(&mut status)
            .ok()?;
        if status.trim() != "connected" {
            continue;
        }
        // Never select a connector based only on an unqualified name shared
        // by multiple GPUs or drivers.
        if matched.is_some() {
            return None;
        }
        matched = Some(node);
    }
    let node = matched?;
    let canonical = fs::canonicalize(&node).ok()?;
    let canonical = canonical.to_str()?;
    let mut edid = Vec::new();
    fs::File::open(node.join("edid"))
        .ok()?
        .take(MAX_EDID_BYTES as u64 + 1)
        .read_to_end(&mut edid)
        .ok()?;
    if edid.is_empty() || edid.len() > MAX_EDID_BYTES {
        return None;
    }
    let mut status = String::new();
    fs::File::open(node.join("status"))
        .ok()?
        .take(32)
        .read_to_string(&mut status)
        .ok()?;
    if status.trim() != "connected" || fs::canonicalize(&node).ok()?.to_str()? != canonical {
        return None;
    }
    Some(digest_identity(b"drm-edid-v1", canonical.as_bytes(), &edid))
}

fn digest_identity(domain: &[u8], source: &[u8], identity: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update((source.len() as u64).to_le_bytes());
    hash.update(source);
    hash.update(identity);
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct DrmFixture(std::path::PathBuf);
    impl DrmFixture {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "amaterasu-display-identity-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn connector(&self, name: &str, status: &str, edid: &[u8]) -> std::path::PathBuf {
            let node = self.0.join(name);
            fs::create_dir(&node).unwrap();
            fs::write(node.join("status"), status).unwrap();
            fs::write(node.join("edid"), edid).unwrap();
            node
        }
        fn identity(&self) -> Option<String> {
            identity_with_drm(&json!({"name":"HDMI-A-1"}), &self.0).unwrap()
        }
    }
    impl Drop for DrmFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn drm_fallback_binds_device_node_and_monitor_edid_when_kscreen_omits_uuid() {
        let fixture = DrmFixture::new();
        let node = fixture.connector("card1-HDMI-A-1", "connected\n", b"monitor-one-edid");
        let original = fixture.identity().unwrap();
        assert_eq!(fixture.identity().unwrap(), original);
        fs::write(node.join("edid"), b"monitor-two-edid").unwrap();
        assert_ne!(
            fixture.identity().unwrap(),
            original,
            "a replacement monitor must not inherit recovery identity"
        );
        let replacement = fixture.identity().unwrap();
        fs::rename(&node, fixture.0.join("card2-HDMI-A-1")).unwrap();
        assert_ne!(fixture.identity().unwrap(), replacement);
    }
    #[test]
    fn ambiguous_disconnected_missing_or_oversized_edid_never_establishes_identity() {
        let fixture = DrmFixture::new();
        assert!(fixture.identity().is_none());
        let first = fixture.connector("card0-HDMI-A-1", "disconnected\n", b"first");
        let second = fixture.connector("card1-HDMI-A-1", "connected\n", b"second");
        assert!(fixture.identity().is_some());
        fs::write(first.join("status"), "connected").unwrap();
        assert!(fixture.identity().is_none());
        fs::write(first.join("status"), "disconnected").unwrap();
        fs::write(second.join("edid"), []).unwrap();
        assert!(fixture.identity().is_none());
        fs::write(second.join("edid"), vec![0; MAX_EDID_BYTES + 1]).unwrap();
        assert!(fixture.identity().is_none());
    }
    #[test]
    fn bounded_kscreen_uuid_is_available_without_drm_and_changes_on_replacement() {
        let fixture = DrmFixture::new();
        let first = identity_with_drm(&json!({"name":"DP-1","uuid":"monitor-one"}), &fixture.0)
            .unwrap()
            .unwrap();
        let second = identity_with_drm(&json!({"name":"DP-1","uuid":"monitor-two"}), &fixture.0)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        assert!(monitor_identity(&json!({"name":"../DP-1"})).is_err());
        assert!(identity_with_drm(&json!({"name":"../DP-1"}), &fixture.0).is_err());
        assert!(
            identity_with_drm(
                &json!({"name":"DP-1","uuid":"x".repeat(MAX_UUID_BYTES+1)}),
                &fixture.0
            )
            .is_err()
        );
    }
}
