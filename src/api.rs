//! Public operation schemas, independently selected and approved by composition.
use amaterasu_module_sdk::{ApiMethod, operation_api};
use serde_json::Value;
pub(crate) const SERVICE: &str = "nonary.display/1";
pub(crate) const PROFILE_SERVICE: &str = "nonary.display-profile/1";
pub(crate) const PROFILE_SERVICE_V2: &str = "nonary.display-profile/2";
pub(crate) const LEGACY_PROFILE_SERVICE: &str = "amaterasu.display-profile/1";
pub(crate) const LEGACY_PROFILE_SERVICE_V2: &str = "amaterasu.display-profile/2";
pub(crate) fn methods() -> Vec<ApiMethod> {
    let mut methods: Vec<ApiMethod> = serde_json::from_str(include_str!("../metadata/api.json"))
        .expect("valid module-owned API metadata");
    let profile_methods: Vec<ApiMethod> =
        serde_json::from_str(include_str!("../metadata/profile_api.json"))
            .expect("valid module-owned profile API metadata");
    let profile_methods_v2: Vec<ApiMethod> =
        serde_json::from_str(include_str!("../metadata/profile_api_v2.json"))
            .expect("valid module-owned profile API v2 metadata");
    methods.extend(profile_methods);
    methods.extend(profile_methods_v2);
    assert!(methods.iter().all(|method| {
        method.service == SERVICE
            || method.service == PROFILE_SERVICE
            || method.service == PROFILE_SERVICE_V2
    }));
    methods
}
pub(crate) fn decode(payload: Value, service: &str) -> Result<Value, String> {
    let is_v1_profile_service = matches!(service, LEGACY_PROFILE_SERVICE | PROFILE_SERVICE);
    if is_v1_profile_service
        && payload["method"] == "resolve_hdr_profile"
        && !payload["arguments"]["request"]["lifecycle_target"].is_null()
    {
        return Err("lifecycle-bound HDR profiles require display-profile/2".into());
    }
    let legacy_service = match service {
        crate::SERVICE | SERVICE => crate::SERVICE,
        LEGACY_PROFILE_SERVICE | PROFILE_SERVICE => LEGACY_PROFILE_SERVICE,
        LEGACY_PROFILE_SERVICE_V2 | PROFILE_SERVICE_V2 => LEGACY_PROFILE_SERVICE_V2,
        _ => return Err("undeclared display provider service".into()),
    };
    operation_api::decode_delivery(payload, service, legacy_service, &methods())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn public_aliases_require_their_exact_envelope_and_service() {
        let name = methods()[0].method.clone();
        let arguments = if name == "get" {
            json!({"id":"app-1"})
        } else {
            json!({})
        };
        let request =
            json!({"method":name,"interface_major":1,"interface_minor":0,"arguments":arguments});
        assert!(decode(request.clone(), SERVICE).is_ok());
        assert!(decode(request.clone(), crate::SERVICE).is_err());
        assert!(decode(json!({"action":name}), SERVICE).is_err());
        assert!(decode(request.clone(), "foreign.service/1").is_err());
        let mut invalid = request;
        invalid["arguments"]["action"] = json!("apply");
        assert!(decode(invalid, SERVICE).is_err());
    }

    #[test]
    fn profile_resolver_alias_is_separately_versioned_and_rejects_display_service() {
        let request = json!({
            "method":"resolve_hdr_profile",
            "interface_major":1,
            "interface_minor":0,
            "arguments":{"request":{"selection":{"id":"profile.icc"},"target_id":"target","target_revision":"topology","policy_revision":"policy"}}
        });
        assert_eq!(
            decode(request.clone(), PROFILE_SERVICE).unwrap(),
            json!({"action":"resolve_hdr_profile","request":{"selection":{"id":"profile.icc"},"target_id":"target","target_revision":"topology","policy_revision":"policy"}})
        );
        assert!(decode(request.clone(), SERVICE).is_err());
    }

    #[test]
    fn profile_v2_alias_is_separate_from_legacy_profile_service() {
        let request = json!({
            "method":"resolve_hdr_profile",
            "interface_major":1,
            "interface_minor":0,
            "arguments":{"request":{"selection":{"id":"profile.icc"},"lifecycle_target":{"kind":"session","host_epoch":"epoch","session_id":"session","generation":1},"target_id":"target","target_revision":"topology","policy_revision":"policy"}}
        });
        assert_eq!(
            decode(request.clone(), PROFILE_SERVICE_V2).unwrap()["action"],
            "resolve_hdr_profile"
        );
        assert!(decode(request, PROFILE_SERVICE).is_err());
        assert!(decode(json!({"action":"recover_hdr_profile"}), PROFILE_SERVICE_V2).is_err());
    }
}
