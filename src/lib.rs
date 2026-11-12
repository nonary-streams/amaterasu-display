//! Physical display changes guarded by a durable recovery journal.
mod api;
pub mod configuration_option;
pub mod hdr_option;
pub mod mode_remapping;
mod model;
mod pipeline;
mod platform;
#[cfg(any(windows, test))]
mod profile_reader;
#[cfg(any(windows, test))]
mod profile_transaction;
pub mod refresh_rate_option;
pub mod resolution_option;
mod scoped;
mod targets;

pub use configuration_option::DisplayConfigurationOption;
pub use hdr_option::DisplayHdrOption;
pub use mode_remapping::{
    DisplayMode, ModeRemapping, ModeRemappingEntry, ModeRemappingError, Resolution, ResolverContext,
};
pub use model::Rational;
pub use refresh_rate_option::RefreshRateOption;
pub use resolution_option::ResolutionOption;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
use pipeline::Pipeline;
use platform::NativePlatform;
use serde::Deserialize;
use std::collections::BTreeSet;

const SERVICE: &str = "amaterasu.display/1";
const PROFILE_SERVICE: &str = "amaterasu.display-profile/1";
const PROFILE_SERVICE_V2: &str = "amaterasu.display-profile/2";
const STATE_SERVICE: &str = "amaterasu.state/1";
const JOURNAL_KEY: &str = "physical-display/recovery";
const PROFILE_JOURNAL_KEY: &str = "physical-display/profile-recovery";

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    allow_mutation: bool,
    /// Optional HDR profile APIs are only registered when explicitly selected
    /// by trusted startup composition. Empty keeps the baseline display grants
    /// and registrations unchanged.
    #[serde(default)]
    hdr_profile_api_versions: Vec<ProfileApiVersion>,
}

impl Settings {
    fn validate(&self) -> ModuleResult<()> {
        let selected: BTreeSet<_> = self.hdr_profile_api_versions.iter().copied().collect();
        if selected.len() != self.hdr_profile_api_versions.len() {
            return Err("duplicate HDR profile API version".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum ProfileApiVersion {
    V1,
    V2,
}

impl ProfileApiVersion {
    const fn provider_service(self) -> &'static str {
        match self {
            Self::V1 => PROFILE_SERVICE,
            Self::V2 => PROFILE_SERVICE_V2,
        }
    }

    const fn public_service(self) -> &'static str {
        match self {
            Self::V1 => api::PROFILE_SERVICE,
            Self::V2 => api::PROFILE_SERVICE_V2,
        }
    }
}
struct DisplayModule {
    runtime: RuntimeClient,
    pipeline: Pipeline<NativePlatform>,
}
impl ManagedModule for DisplayModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("display settings unavailable".into());
        };
        let settings: Settings = serde_json::from_value(payload)
            .map_err(|error| format!("invalid display settings: {error}"))?;
        settings.validate()?;
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        for version in &settings.hdr_profile_api_versions {
            runtime.require_success(Command::Register {
                service: version.provider_service().into(),
            })?;
        }
        let selected_methods: Vec<_> = api::methods()
            .into_iter()
            .filter(|method| {
                method.service == api::SERVICE
                    || settings
                        .hdr_profile_api_versions
                        .iter()
                        .any(|version| method.service == version.public_service())
            })
            .collect();
        amaterasu_module_sdk::operation_api::register_selected(&runtime, &selected_methods)?;
        Ok(Self {
            runtime,
            pipeline: Pipeline::new(
                #[cfg(windows)]
                NativePlatform::default(),
                #[cfg(not(windows))]
                NativePlatform,
                settings.allow_mutation,
            ),
        })
    }
    fn tick(&mut self) -> ModuleResult<()> {
        self.pipeline
            .tick(|command| self.runtime.send_ephemeral(command))
    }
}
// Native display APIs and KScreen commands may wait for mode publication.
// Keep the complete durable transaction on one serialized worker; host ticks
// only wake it, and worker retirement precedes unloading its native library.
amaterasu_module_sdk::export_module!(
    amaterasu_module_sdk::service_worker::ServiceWorker<DisplayModule>
);
