use crate::model::{DisplayError, DisplayResult};
use amaterasu_contracts::display_color_profile::ColorProfileState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRecovery {
    pub previous: ColorProfileState,
    pub requested: ColorProfileState,
}

#[derive(Debug)]
pub struct ProfileTransactionFailure {
    pub error: Box<DisplayError>,
    /// Persist this exact state and retain the operation journal when rollback
    /// cannot be read back. The host must not report retirement in that case.
    pub recovery: Option<Box<ProfileRecovery>>,
}

pub trait ProfileAdapter {
    fn read(&mut self) -> DisplayResult<ColorProfileState>;
    fn write(&mut self, profile: &ColorProfileState) -> DisplayResult<()>;
}

pub fn apply<A: ProfileAdapter>(
    adapter: &mut A,
    requested: &ColorProfileState,
) -> Result<ProfileRecovery, ProfileTransactionFailure> {
    requested
        .validate()
        .map_err(|code| ProfileTransactionFailure {
            error: Box::new(DisplayError::new(code, "invalid color profile state")),
            recovery: None,
        })?;
    if matches!(
        requested,
        ColorProfileState::Unspecified | ColorProfileState::Unknown
    ) {
        return Err(ProfileTransactionFailure {
            error: Box::new(DisplayError::new(
                "invalid_color_profile_state",
                "a profile transaction requires a concrete state",
            )),
            recovery: None,
        });
    }
    let previous = adapter.read().map_err(|error| ProfileTransactionFailure {
        error: Box::new(error),
        recovery: None,
    })?;
    if matches!(
        previous,
        ColorProfileState::Unknown | ColorProfileState::Unspecified
    ) {
        return Err(ProfileTransactionFailure {
            error: Box::new(DisplayError::new(
                "profile_state_unknown",
                "exact prior profile state is required before mutation",
            )),
            recovery: None,
        });
    }
    let recovery = ProfileRecovery {
        previous,
        requested: requested.clone(),
    };
    if recovery.previous == recovery.requested {
        return Ok(recovery);
    }
    if let Err(error) = adapter.write(&recovery.requested) {
        return Err(rollback(adapter, recovery, error));
    }
    match adapter.read() {
        Ok(current) if current == recovery.requested => Ok(recovery),
        Ok(_) => Err(rollback(
            adapter,
            recovery,
            DisplayError::after_mutation(DisplayError::new(
                "profile_readback_mismatch",
                "requested color profile did not match per-display readback",
            )),
        )),
        Err(error) => Err(rollback(
            adapter,
            recovery,
            DisplayError::after_mutation(error),
        )),
    }
}

#[cfg(test)]
pub fn restore<A: ProfileAdapter>(
    adapter: &mut A,
    recovery: &ProfileRecovery,
) -> Result<(), ProfileTransactionFailure> {
    let transaction = apply(adapter, &recovery.previous)?;
    match adapter.read() {
        Ok(current) if current == recovery.previous => Ok(()),
        Ok(_) => Err(ProfileTransactionFailure {
            error: Box::new(DisplayError::after_mutation(DisplayError::new(
                "profile_restore_mismatch",
                "restored color profile differs from the saved profile state",
            ))),
            recovery: Some(Box::new(ProfileRecovery {
                previous: transaction.previous,
                requested: recovery.previous.clone(),
            })),
        }),
        Err(error) => Err(ProfileTransactionFailure {
            error: Box::new(DisplayError::after_mutation(error)),
            recovery: Some(Box::new(ProfileRecovery {
                previous: transaction.previous,
                requested: recovery.previous.clone(),
            })),
        }),
    }
}

fn rollback<A: ProfileAdapter>(
    adapter: &mut A,
    recovery: ProfileRecovery,
    cause: DisplayError,
) -> ProfileTransactionFailure {
    let restored = adapter.write(&recovery.previous).is_ok()
        && adapter.read().is_ok_and(|state| state == recovery.previous);
    if restored {
        ProfileTransactionFailure {
            error: Box::new(cause),
            recovery: None,
        }
    } else {
        ProfileTransactionFailure {
            error: Box::new(DisplayError::after_mutation(DisplayError::new(
                "profile_rollback_failed",
                "color profile mutation failed and the previous per-display profile could not be restored",
            ))),
            recovery: Some(Box::new(recovery)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_contracts::display_color_profile::{HdrProfileDescriptor, HdrProfileSelection};

    struct FakeAdapter {
        state: ColorProfileState,
        writes: usize,
        fail_write: Option<usize>,
        lie_on_read: Option<usize>,
        reads: usize,
    }

    impl ProfileAdapter for FakeAdapter {
        fn read(&mut self) -> DisplayResult<ColorProfileState> {
            self.reads += 1;
            if self.lie_on_read == Some(self.reads) {
                return Ok(ColorProfileState::SystemDefault);
            }
            Ok(self.state.clone())
        }
        fn write(&mut self, profile: &ColorProfileState) -> DisplayResult<()> {
            self.writes += 1;
            if self.fail_write == Some(self.writes) {
                return Err(DisplayError::after_mutation(DisplayError::new(
                    "fake_write_failed",
                    "injected failure",
                )));
            }
            self.state = profile.clone();
            Ok(())
        }
    }

    fn selected(id: &str) -> ColorProfileState {
        ColorProfileState::Selected(HdrProfileDescriptor {
            selection: HdrProfileSelection { id: id.into() },
            revision: "sha256:abc".into(),
            peak_luminance_nits: Some(800),
        })
    }

    fn fake() -> FakeAdapter {
        FakeAdapter {
            state: ColorProfileState::SystemDefault,
            writes: 0,
            fail_write: None,
            lie_on_read: None,
            reads: 0,
        }
    }

    #[test]
    fn unknown_prior_profile_fails_before_any_write() {
        let mut adapter = fake();
        adapter.state = ColorProfileState::Unknown;
        let error = apply(&mut adapter, &selected("hdr.icc")).unwrap_err();
        assert_eq!(error.error.code, "profile_state_unknown");
        assert_eq!(adapter.writes, 0);
    }

    #[test]
    fn write_failure_restores_exact_previous_profile() {
        let mut adapter = fake();
        adapter.fail_write = Some(1);
        let error = apply(&mut adapter, &selected("hdr.icc")).unwrap_err();
        assert_eq!(error.error.code, "fake_write_failed");
        assert!(error.recovery.is_none());
        assert_eq!(adapter.state, ColorProfileState::SystemDefault);
    }

    #[test]
    fn readback_mismatch_restores_exact_previous_profile() {
        let mut adapter = fake();
        adapter.lie_on_read = Some(2);
        let error = apply(&mut adapter, &selected("hdr.icc")).unwrap_err();
        assert_eq!(error.error.code, "profile_readback_mismatch");
        assert!(error.recovery.is_none());
        assert_eq!(adapter.state, ColorProfileState::SystemDefault);
    }

    #[test]
    fn restore_returns_to_the_exact_previous_profile() {
        let mut adapter = fake();
        let recovery = apply(&mut adapter, &selected("hdr.icc")).unwrap();
        restore(&mut adapter, &recovery).unwrap();
        assert_eq!(adapter.state, ColorProfileState::SystemDefault);
    }

    #[test]
    fn failed_rollback_returns_recovery_evidence() {
        let mut adapter = fake();
        adapter.lie_on_read = Some(2);
        adapter.fail_write = Some(2);
        let error = apply(&mut adapter, &selected("hdr.icc")).unwrap_err();
        assert_eq!(error.error.code, "profile_rollback_failed");
        assert_eq!(
            error.recovery.unwrap().previous,
            ColorProfileState::SystemDefault
        );
    }
}
