//! KScreen policy composition keeps native recovery state independent of CCD.
use super::*;
use crate::model::{DisplaySetting, KScreenSnapshot, ModePolicy, Rotation};
use crate::refresh_rate_option::RefreshRateOption;
use std::collections::BTreeMap;

pub(super) fn resolve<P: Platform>(
    platform: &mut P,
    mut configuration: Configuration,
    baseline: &KScreenSnapshot,
    mode_policy: Option<ModePolicy>,
    mode_policies: Option<Vec<ModePolicy>>,
    expected_revision: Option<&str>,
) -> DisplayResult<Configuration> {
    baseline.validate()?;
    configuration.validate_request_fields()?;
    for display in &configuration.displays {
        if !baseline
            .outputs
            .iter()
            .any(|output| output.connected && output.id.eq_ignore_ascii_case(&display.id))
        {
            return Err(DisplayError::new(
                "unsupported_topology",
                "selected KScreen output is not connected",
            ));
        }
    }
    let revision = baseline.revision()?;
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(DisplayError::new(
            "topology_changed",
            "KScreen topology changed before policy selection",
        ));
    }
    if mode_policy.is_some() && mode_policies.is_some() {
        return Err(DisplayError::new(
            "invalid_request",
            "apply accepts either mode_policy or mode_policies",
        ));
    }
    let topology_policy = mode_policy.clone();
    let policies = mode_policies.unwrap_or_else(|| mode_policy.into_iter().collect());
    let mut selected_ids = BTreeSet::new();
    for policy in &policies {
        if !selected_ids.insert(policy.display_id.to_ascii_lowercase())
            || !configuration
                .displays
                .iter()
                .any(|display| display.id.eq_ignore_ascii_case(&policy.display_id))
        {
            return Err(DisplayError::new(
                "invalid_request",
                "mode policies require distinct configured displays",
            ));
        }
        let modes = if policy.refresh_rate_option == Some(RefreshRateOption::PreferHighest)
            && !policy.context.client_display_mode_override
        {
            Some(platform.available_modes(&policy.display_id)?)
        } else {
            None
        };
        configuration.resolve_mode_policy_in_batch(policy, modes.as_deref())?;
    }
    if let Some(policy) = topology_policy
        && let Some(option) = policy.configuration_option
    {
        if matches!(
            option,
            DisplayConfigurationOption::EnsureActive
                | DisplayConfigurationOption::EnsurePrimary
                | DisplayConfigurationOption::EnsureOnlyDisplay
        ) && expected_revision.is_none()
        {
            return Err(DisplayError::new(
                "invalid_request",
                "topology-changing configuration option requires expected_topology_revision",
            ));
        }
        configuration = compose(&configuration, baseline, &policy.display_id, option)?;
    }
    validate_configuration(&configuration, baseline)?;
    Ok(configuration)
}

fn validate_configuration(
    configuration: &Configuration,
    baseline: &KScreenSnapshot,
) -> DisplayResult<()> {
    let scales = configuration
        .displays
        .iter()
        .map(|display| {
            baseline
                .outputs
                .iter()
                .find(|output| output.connected && output.id.eq_ignore_ascii_case(&display.id))
                .map(|output| (display.id.to_ascii_lowercase(), output.scale))
                .ok_or_else(|| {
                    DisplayError::new(
                        "unsupported_topology",
                        "selected KScreen output is not connected",
                    )
                })
        })
        .collect::<DisplayResult<BTreeMap<_, _>>>()?;
    configuration.validate_with_scales(Some(&scales))
}

fn active_configuration(baseline: &KScreenSnapshot) -> DisplayResult<Configuration> {
    let displays = baseline
        .outputs
        .iter()
        .filter(|output| output.connected && output.enabled)
        .map(|output| {
            let mode = output.mode.as_ref().ok_or_else(|| {
                DisplayError::new("invalid_snapshot", "active output has no mode")
            })?;
            Ok(DisplaySetting {
                id: output.id.clone(),
                width: mode.width,
                height: mode.height,
                refresh: Rational {
                    numerator: mode.refresh_millihz,
                    denominator: 1000,
                },
                x: output.position.x,
                y: output.position.y,
                rotation: match output.rotation {
                    1 => Rotation::Identity,
                    2 => Rotation::Rotate90,
                    4 => Rotation::Rotate180,
                    8 => Rotation::Rotate270,
                    _ => {
                        return Err(DisplayError::new(
                            "invalid_snapshot",
                            "invalid KScreen rotation",
                        ));
                    }
                },
                hdr_enabled: output.hdr_enabled,
            })
        })
        .collect::<DisplayResult<Vec<_>>>()?;
    let configuration = Configuration { displays };
    if !configuration.displays.is_empty() {
        validate_configuration(&configuration, baseline)?;
    }
    Ok(configuration)
}

fn logical_width(display: &DisplaySetting, baseline: &KScreenSnapshot) -> DisplayResult<i32> {
    let output = baseline
        .outputs
        .iter()
        .find(|output| output.id.eq_ignore_ascii_case(&display.id))
        .ok_or_else(|| {
            DisplayError::new("unsupported_display", "selected output is not connected")
        })?;
    let width = if matches!(display.rotation, Rotation::Rotate90 | Rotation::Rotate270) {
        display.height
    } else {
        display.width
    };
    Ok((f64::from(width) / output.scale).ceil() as i32)
}

fn compose(
    requested: &Configuration,
    baseline: &KScreenSnapshot,
    target: &str,
    option: DisplayConfigurationOption,
) -> DisplayResult<Configuration> {
    let mut selected = requested
        .displays
        .iter()
        .find(|display| display.id.eq_ignore_ascii_case(target))
        .cloned()
        .ok_or_else(|| {
            DisplayError::new(
                "invalid_request",
                "selected display is absent from configuration",
            )
        })?;
    let mut active = active_configuration(baseline)?;
    let existing = active
        .displays
        .iter()
        .position(|display| display.id.eq_ignore_ascii_case(target));
    if option == DisplayConfigurationOption::EnsurePrimary
        && existing
            .is_some_and(|index| active.displays[index].x == 0 && active.displays[index].y == 0)
        && baseline
            .outputs
            .iter()
            .any(|output| output.primary && !output.id.eq_ignore_ascii_case(target))
    {
        return Err(DisplayError::new(
            "unsupported_topology",
            "changing primary without a desktop origin translation requires explicit primary intent",
        ));
    }
    if option == DisplayConfigurationOption::VerifyOnly && existing.is_none() {
        return Err(DisplayError::new(
            "unsupported_display",
            "verify_only requires an active display",
        ));
    }
    if option == DisplayConfigurationOption::Disabled {
        return Err(DisplayError::new(
            "invalid_request",
            "disabled configuration must not enter mutation preparation",
        ));
    }
    if option == DisplayConfigurationOption::EnsureOnlyDisplay {
        selected.x = 0;
        selected.y = 0;
        return Ok(Configuration {
            displays: vec![selected],
        });
    }
    let (origin_x, origin_y) = if let Some(index) = existing {
        selected.x = active.displays[index].x;
        selected.y = active.displays[index].y;
        let origin = (selected.x, selected.y);
        active.displays[index] = selected;
        origin
    } else if option == DisplayConfigurationOption::EnsurePrimary {
        let Some(anchor) = active.displays.iter().min_by_key(|display| display.x) else {
            selected.x = 0;
            selected.y = 0;
            active.displays.push(selected);
            return Ok(active);
        };
        let shift_x = logical_width(&selected, baseline)?
            .checked_sub(anchor.x)
            .ok_or_else(coordinate_overflow)?;
        let shift_y = anchor.y.checked_neg().ok_or_else(coordinate_overflow)?;
        crate::scoped::translate_configuration(&mut active, (shift_x, shift_y))?;
        selected.x = 0;
        selected.y = 0;
        active.displays.push(selected);
        (0, 0)
    } else {
        let right = active.displays.iter().try_fold(0i32, |right, display| {
            display
                .x
                .checked_add(logical_width(display, baseline)?)
                .map(|edge| right.max(edge))
                .ok_or_else(coordinate_overflow)
        })?;
        selected.x = right;
        selected.y = 0;
        active.displays.push(selected);
        (right, 0)
    };
    if option == DisplayConfigurationOption::EnsurePrimary {
        crate::scoped::translate_configuration(
            &mut active,
            (
                origin_x.checked_neg().ok_or_else(coordinate_overflow)?,
                origin_y.checked_neg().ok_or_else(coordinate_overflow)?,
            ),
        )?;
    }
    Ok(active)
}

fn coordinate_overflow() -> DisplayError {
    DisplayError::new("invalid_request", "display coordinate overflow")
}
