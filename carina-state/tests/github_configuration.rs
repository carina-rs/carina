use carina_core::{
    EnvironmentSnapshot,
    parser::{self, ProviderContext},
};
use carina_state::{BackendConfig, BackendLock, StateUrl};

#[test]
fn github_configuration_round_trips_locks_and_saved_plan_candidates() {
    let raw = parser::parse_file(
        "backend github { repository = 'owner/repo' branch = 'states/prod' path = 'states/prod.json' }",
        std::path::Path::new("backend.crn"), &ProviderContext::new(EnvironmentSnapshot::empty()),
    ).unwrap().backend.unwrap();
    let config = BackendConfig::try_from(&raw).unwrap();
    let lock = BackendLock::from_config(&config);
    assert_eq!(lock.to_state_config().unwrap(), config);
    assert_eq!(
        BackendConfig::try_from(&config.to_parser_config()).unwrap(),
        config
    );
    let encoded = serde_json::to_value(&lock).unwrap();
    assert_eq!(encoded["backend_type"], "github");
    assert_eq!(encoded["attributes"].as_object().unwrap().len(), 3);
    assert!(config.as_s3().is_none());
}

#[test]
fn github_url_requires_explicit_validated_destination() {
    let url = StateUrl::parse("github://owner/repo/states/prod.json?branch=states%2Fprod").unwrap();
    assert_eq!(
        url.display_target(),
        "github://owner/repo/states/prod.json?branch=states%2Fprod"
    );
    for raw in [
        "github:///repo/s.json?branch=b",
        "github://owner//s.json?branch=b",
        "github://owner/repo?branch=b",
        "github://owner/repo/?branch=b",
        "github://owner/repo/s.json",
        "github://owner/repo/s.json?branch=",
        "github://owner/repo/s.json?branch=b&branch=c",
        "github://owner/repo/s.json?branch=b&typo=x",
        "github://owner/repo/../s.json?branch=b",
        "github://owner/repo/s.json.lock?branch=b",
    ] {
        assert!(StateUrl::parse(raw).is_err(), "{raw}");
    }
}
