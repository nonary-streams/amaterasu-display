//! Full Linux desktop topology transactions, distinct from scoped composition.
use super::*;

fn unchanged_connectors(saved: &KScreenSnapshot, current: &KScreenSnapshot) -> DisplayResult<()> {
    let connected = |snapshot: &KScreenSnapshot| {
        snapshot
            .outputs
            .iter()
            .filter(|output| output.connected)
            .map(|output| output.id.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
    };
    if connected(saved) != connected(current) {
        return Err(DisplayError::new(
            "topology_changed",
            "connected KScreen inventory changed during the display lease",
        ));
    }
    for output in &saved.outputs {
        if !output.connected {
            continue;
        }
        if output.hardware_identity.is_none()
            || !current.outputs.iter().any(|present| {
                present.id.eq_ignore_ascii_case(&output.id)
                    && present.hardware_identity == output.hardware_identity
            })
        {
            return Err(DisplayError::new(
                "recovery_blocked",
                "physical monitor identity is missing or changed",
            ));
        }
    }
    Ok(())
}

/// Resolve every requested native mode before any output changes. Enable
/// commands are executed and independently verified before any deactivation,
/// preserving a usable scanout when replacement admission or readback fails.
fn plan(
    expected: &KScreenSnapshot,
    current: &Value,
    explicit_hdr: &BTreeSet<String>,
) -> DisplayResult<Vec<KScreenAction>> {
    let current_snapshot = typed_snapshot(&validated_outputs(current)?)?;
    unchanged_connectors(expected, &current_snapshot)?;
    if !expected
        .outputs
        .iter()
        .any(|output| output.connected && output.enabled)
    {
        return Err(DisplayError::new(
            "unsupported_topology",
            "display topology must retain at least one enabled output",
        ));
    }
    let outputs = validated_outputs(current)?;
    let mut actions = Vec::new();
    let mut disable = Vec::new();
    for saved in &expected.outputs {
        if !saved.connected {
            continue;
        }
        let present = outputs
            .iter()
            .copied()
            .find(|output| {
                output_name(output).is_ok_and(|name| name.eq_ignore_ascii_case(&saved.id))
            })
            .ok_or_else(|| {
                DisplayError::new("topology_changed", "saved connector is unavailable")
            })?;
        let active = bool_field(present, "enabled")?;
        if !saved.enabled {
            if active {
                if present.get("priority").and_then(Value::as_u64) == Some(1) {
                    actions.push(KScreenAction::Priority {
                        output: saved.id.clone(),
                        priority: 2,
                    });
                }
                disable.push(KScreenAction::Enabled {
                    output: saved.id.clone(),
                    enabled: false,
                });
            }
            continue;
        }
        let saved_mode = saved.mode.as_ref().ok_or_else(|| {
            DisplayError::new("invalid_snapshot", "enabled output has no saved mode")
        })?;
        let selected = resolve_mode(present, saved_mode, Some(&saved_mode.id))?;
        let mode_changed = current_mode(present).map_or(true, |mode| {
            !mode_timing_matches(mode, saved_mode).unwrap_or(false)
        });
        if !active {
            actions.push(KScreenAction::Enabled {
                output: saved.id.clone(),
                enabled: true,
            });
        }
        if mode_changed || !active {
            actions.push(KScreenAction::Mode {
                output: saved.id.clone(),
                mode: string_field(selected, "id")?.into(),
            });
        }
        let position = present
            .get("pos")
            .ok_or_else(|| DisplayError::new("platform_error", "output position missing"))?;
        if bounded_integer(position, "x", -65_536, 65_536)? != saved.position.x
            || bounded_integer(position, "y", -65_536, 65_536)? != saved.position.y
        {
            actions.push(KScreenAction::Position {
                output: saved.id.clone(),
                x: saved.position.x,
                y: saved.position.y,
            });
        }
        if present.get("scale").and_then(Value::as_f64).unwrap_or(1.0) != saved.scale || !active {
            actions.push(KScreenAction::Scale {
                output: saved.id.clone(),
                scale: saved.scale,
            });
        }
        if present.get("rotation").and_then(Value::as_u64).unwrap_or(1) != u64::from(saved.rotation)
            || !active
        {
            actions.push(KScreenAction::Rotation {
                output: saved.id.clone(),
                rotation: saved.rotation,
            });
        }
        if present.get("priority").and_then(Value::as_u64).unwrap_or(0) != u64::from(saved.priority)
            || !active
        {
            actions.push(KScreenAction::Priority {
                output: saved.id.clone(),
                priority: saved.priority,
            });
        }
        let actual_hdr = present.get("hdr").and_then(Value::as_bool);
        match (saved.hdr_enabled, actual_hdr) {
            (Some(expected), Some(actual))
                if expected != actual
                    || (expected
                        && (mode_changed
                            || !active
                            || explicit_hdr.contains(&saved.id.to_ascii_lowercase()))) =>
            {
                actions.push(KScreenAction::Hdr {
                    output: saved.id.clone(),
                    enabled: expected,
                });
            }
            (Some(_), Some(_)) | (None, None) => {}
            _ => {
                return Err(DisplayError::new(
                    "unsupported_topology",
                    "HDR baseline is not readable enough for display recovery",
                ));
            }
        }
    }
    actions.extend(disable);
    Ok(actions)
}

pub(super) fn prepare(
    configuration: &Configuration,
    baseline: &KScreenSnapshot,
    current: &Value,
) -> DisplayResult<()> {
    let observed = typed_snapshot(&validated_outputs(current)?)?;
    if observed.revision()? != baseline.revision()? {
        return Err(DisplayError::new(
            "topology_changed",
            "KScreen changed before configuration preparation",
        ));
    }
    let expected = baseline.configured_topology(configuration)?;
    // Never admit an HDR change without a restorable observed source value.
    for display in &configuration.displays {
        if display.hdr_enabled.is_some()
            && baseline
                .outputs
                .iter()
                .find(|output| output.id.eq_ignore_ascii_case(&display.id))
                .and_then(|output| output.hdr_enabled)
                .is_none()
        {
            return Err(DisplayError::new(
                "unsupported_topology",
                "HDR has no recoverable native baseline",
            ));
        }
    }
    plan(&expected, current, &BTreeSet::new())?;
    Ok(())
}

pub(super) fn apply(
    runner: &mut impl KScreenCommandRunner,
    configuration: &Configuration,
    baseline: &KScreenSnapshot,
    guard: &mut dyn FnMut() -> DisplayResult<()>,
) -> DisplayResult<()> {
    guard()?;
    let before = query_configuration_with(runner)?;
    prepare(configuration, baseline, &before)?;
    guard()?;
    let current = query_configuration_with(runner)?;
    prepare(configuration, baseline, &current)?;
    let expected = baseline.configured_topology(configuration)?;
    let explicit_hdr = configuration
        .displays
        .iter()
        .filter(|display| display.hdr_enabled == Some(true))
        .map(|display| display.id.to_ascii_lowercase())
        .collect();
    let actions = plan(&expected, &current, &explicit_hdr)?;
    execute(runner, &expected, &current, actions, guard)?;
    Ok(())
}

/// Intermediate observations may still include retiring outputs. Every desired
/// survivor must independently expose its exact mode, geometry and color before
/// the transaction is allowed to retire any previous scanout.
fn survivors_match(expected: &KScreenSnapshot, observed: &KScreenSnapshot) -> bool {
    if unchanged_connectors(expected, observed).is_err() {
        return false;
    }
    expected
        .outputs
        .iter()
        .filter(|output| output.enabled)
        .all(|saved| {
            observed
                .outputs
                .iter()
                .find(|present| present.id.eq_ignore_ascii_case(&saved.id))
                .is_some_and(|present| {
                    present.enabled
                        && saved
                            .mode
                            .as_ref()
                            .map(|mode| (mode.width, mode.height, mode.refresh_millihz))
                            == present
                                .mode
                                .as_ref()
                                .map(|mode| (mode.width, mode.height, mode.refresh_millihz))
                        && saved.position == present.position
                        && saved.scale == present.scale
                        && saved.rotation == present.rotation
                        && saved.primary == present.primary
                        && saved.hdr_enabled == present.hdr_enabled
                })
        })
}

fn execute(
    runner: &mut impl KScreenCommandRunner,
    expected: &KScreenSnapshot,
    current: &Value,
    actions: Vec<KScreenAction>,
    guard: &mut dyn FnMut() -> DisplayResult<()>,
) -> DisplayResult<()> {
    let observed = typed_snapshot(&validated_outputs(current)?)?;
    let mut before_hdr = expected.clone();
    for output in &mut before_hdr.outputs {
        if actions.iter().any(|action| matches!(action,KScreenAction::Hdr{output:name,enabled:true} if name.eq_ignore_ascii_case(&output.id))) {
            output.hdr_enabled=observed.outputs.iter().find(|present|present.id.eq_ignore_ascii_case(&output.id)).and_then(|present|present.hdr_enabled);
        }
    }
    let (mut retirement, activation): (Vec<_>, Vec<_>) = actions
        .into_iter()
        .partition(|action| matches!(action, KScreenAction::Enabled { enabled: false, .. }));
    configure_before_hdr_activation(runner, activation, guard, |observed| {
        survivors_match(&before_hdr, observed)
    })?;
    if !retirement.is_empty() {
        let current = query_configuration_with(runner).map_err(DisplayError::after_mutation)?;
        let survivors =
            typed_snapshot(&validated_outputs(&current).map_err(DisplayError::after_mutation)?)
                .map_err(DisplayError::after_mutation)?;
        if !survivors_match(expected, &survivors) {
            return Err(DisplayError::new("verification_failed","replacement display was not independently ready before retiring the previous scanout").after_mutation());
        }
        guard().map_err(DisplayError::after_mutation)?;
        // KWin can renumber priorities while a retiring output still occupies
        // a slot. Reassert the saved desired ordering in the final transaction.
        for output in expected.outputs.iter().filter(|output| output.enabled) {
            retirement.push(KScreenAction::Priority {
                output: output.id.clone(),
                priority: output.priority,
            });
        }
        run_configuration_with(runner, retirement)?;
    }
    let after = query_configuration_with(runner).map_err(DisplayError::after_mutation)?;
    let observed =
        typed_snapshot(&validated_outputs(&after).map_err(DisplayError::after_mutation)?)
            .map_err(DisplayError::after_mutation)?;
    if !expected.matches_topology_restoration(&observed) {
        return Err(DisplayError::new(
            "verification_failed",
            "KScreen topology did not read back the complete expected desktop",
        )
        .after_mutation());
    }
    Ok(())
}

pub(super) fn restore(
    runner: &mut impl KScreenCommandRunner,
    baseline: &KScreenSnapshot,
    guard: &mut dyn FnMut() -> DisplayResult<()>,
) -> DisplayResult<()> {
    baseline.validate()?;
    guard()?;
    let before = query_configuration_with(runner)?;
    let observed = typed_snapshot(&validated_outputs(&before)?)?;
    if baseline.matches_topology_restoration(&observed) {
        return Ok(());
    }
    plan(baseline, &before, &BTreeSet::new())?;
    guard()?;
    let current = query_configuration_with(runner)?;
    let latest = typed_snapshot(&validated_outputs(&current)?)?;
    if latest.revision()? != observed.revision()? {
        return Err(DisplayError::new(
            "topology_changed",
            "KScreen changed immediately before topology restoration",
        ));
    }
    let actions = plan(baseline, &current, &BTreeSet::new())?;
    execute(runner, baseline, &current, actions, guard)
}
