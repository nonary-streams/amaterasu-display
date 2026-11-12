use super::*;

fn child_with_pipes(script: &str) -> Child {
    Command::new("/bin/sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap()
}

#[test]
fn command_collection_does_not_wait_for_descendants_holding_pipes() {
    let child = child_with_pipes("(sleep 2; printf 'applying config failed' >&2) & printf stdout");
    let start = Instant::now();
    let error = collect_child_output(child, Duration::from_secs(1)).unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(error.code, "platform_error");
    assert!(error.message.contains("pipe remained open"));
    assert!(command_error(&KScreenCommand::Configure(vec![]), error).may_have_changed);
}

#[test]
fn command_collection_kills_descendants_that_keep_pipes_open() {
    let marker = std::env::temp_dir().join(format!(
        "amaterasu-display-descendant-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let quoted_marker = format!("'{}'", marker.to_string_lossy().replace('\'', "'\\''"));
    let script = format!("(sleep 0.15; printf leaked > {quoted_marker}) & printf stdout");
    let error =
        collect_child_output(child_with_pipes(&script), Duration::from_secs(1)).unwrap_err();
    assert!(error.message.contains("pipe remained open"));
    thread::sleep(Duration::from_millis(300));
    let descendant_ran = marker.exists();
    let _ = std::fs::remove_file(marker);
    assert!(!descendant_ran, "a descendant outlived command collection");
}

#[test]
fn command_collection_bounds_output_and_drains_both_pipes() {
    let child = child_with_pipes("head -c 1100000 /dev/zero; head -c 1100000 /dev/zero >&2");
    let output = collect_child_output(child, Duration::from_secs(5)).unwrap();
    assert_eq!(output.status, Some(0));
    assert_eq!(output.stdout.len(), MAX_CONFIGURATION_BYTES + 1);
    assert_eq!(output.stderr.len(), MAX_CONFIGURATION_BYTES + 1);
}

#[test]
fn command_collection_enforces_deadline_for_a_live_child() {
    // The background child retains both pipes after the foreground child is
    // killed, so timeout must not wait for pipe EOF either.
    let child = child_with_pipes("sleep 2 & exec sleep 5");
    let start = Instant::now();
    let error = collect_child_output(child, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.code, "platform_timeout");
    assert!(start.elapsed() < Duration::from_secs(2));
}

fn mode(id: &str, width: u32, height: u32, refresh_rate: f64) -> Value {
    json!({
        "id": id,
        "size": { "width": width, "height": height },
        "refreshRate": refresh_rate
    })
}

fn output(
    name: &str,
    connected: bool,
    enabled: bool,
    current_mode: &str,
    pos: (i32, i32),
    modes: Vec<Value>,
) -> Value {
    json!({
        "name": name,
        "uuid":format!("monitor-{name}"),
        "description": format!("{name} monitor"),
        "connected": connected,
        "enabled": enabled,
        "currentModeId": current_mode,
        "modes": modes,
        "pos": { "x": pos.0, "y": pos.1 },
        "rotation": 1,
        "scale": 1.25,
        "priority": if connected && enabled { if pos == (0, 0) { 1 } else { 2 } } else { 0 },
        "hdr": false
    })
}

fn reference_kde_configuration() -> Value {
    // Field shape and representative values match the DP-1/DP-2 KScreen
    // restore-policy fixture in Vibeshine's Linux test suite.
    let mut first = output(
        "DP-1",
        true,
        true,
        "25",
        (0, 0),
        vec![mode("25", 2560, 1440, 60.0)],
    );
    first["size"] = json!({ "width": 2560, "height": 1440 });
    first["scale"] = json!(1.5);
    first["rotation"] = json!(1);
    first["hdr"] = json!(true);
    let mut second = output(
        "DP-2",
        true,
        true,
        "31",
        (2560, 0),
        vec![mode("31", 1920, 1080, 60.0)],
    );
    second["size"] = json!({ "width": 1920, "height": 1080 });
    second["scale"] = json!(1.0);
    second["rotation"] = json!(2);
    second["hdr"] = json!(false);
    json!({ "outputs": [first, second] })
}

#[test]
fn scoped_primary_changes_only_common_origin_and_explicit_priority_pair() {
    let raw = reference_kde_configuration();
    let before = typed_snapshot(&validated_outputs(&raw).unwrap()).unwrap();
    let mut requested: Configuration = serde_json::from_value(
        configuration_snapshot(&raw, "test").unwrap()["configuration"].clone(),
    )
    .unwrap();
    for display in &mut requested.displays {
        display.x -= 2560;
    }
    let mut runner = FakeKScreenRunner {
        current: Some(raw.clone()),
        ..Default::default()
    };
    apply_scoped_kscreen_snapshot_with(&mut runner, &requested, &before, Some("DP-2"), &mut || {
        Ok(())
    })
    .unwrap();
    let observed =
        typed_snapshot(&validated_outputs(runner.current.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!(observed.outputs[0].position.x, -2560);
    assert_eq!(observed.outputs[0].scale, before.outputs[0].scale);
    assert_eq!(observed.outputs[0].mode, before.outputs[0].mode);
    assert_eq!(observed.outputs[0].rotation, before.outputs[0].rotation);
    assert_eq!(
        observed.outputs[0].hdr_enabled,
        before.outputs[0].hdr_enabled
    );
    assert_eq!(observed.outputs[0].priority, 2);
    assert_eq!(observed.outputs[1].priority, 1);
    let actions = runner
        .commands
        .iter()
        .filter_map(|command| match command {
            KScreenCommand::Configure(actions) => Some(actions),
            _ => None,
        })
        .flatten();
    assert!(actions.into_iter().all(|action| matches!(
        action,
        KScreenAction::Position { .. } | KScreenAction::Priority { .. }
    )));
    let mut partial = FakeKScreenRunner {
        current: Some(raw),
        partial_configuration: true,
        ..Default::default()
    };
    let failed = apply_scoped_kscreen_snapshot_with(
        &mut partial,
        &requested,
        &before,
        Some("DP-2"),
        &mut || Ok(()),
    )
    .unwrap_err();
    assert!(failed.may_have_changed);
    assert_eq!(failed.code, "verification_failed");
}

fn configuration() -> Value {
    json!({
        "outputs": [
            output(
                "DP-1",
                true,
                true,
                "1920x1080@60",
                (0, 0),
                vec![mode("1920x1080@60", 1920, 1080, 60.0), mode("1280x720@60", 1280, 720, 60.0)]
            ),
            output(
                "HDMI-A-1",
                true,
                false,
                "",
                (1920, 0),
                vec![mode("1920x1080@60", 1920, 1080, 60.0)]
            ),
            output("DP-3", false, false, "", (0, 0), vec![])
        ]
    })
}

#[derive(Default)]
struct FakeKScreenRunner {
    current: Option<Value>,
    commands: Vec<KScreenCommand>,
    reject_configuration: bool,
    partial_configuration: bool,
    ignore_configuration: bool,
    timeout_configuration: bool,
}

impl KScreenCommandRunner for FakeKScreenRunner {
    fn execute(&mut self, command: &KScreenCommand) -> DisplayResult<KScreenCommandOutput> {
        self.commands.push(command.clone());
        match command {
            KScreenCommand::Query => {
                let current = self.current.as_ref().ok_or_else(|| {
                    DisplayError::new("platform_error", "fake KScreen state is missing")
                })?;
                Ok(KScreenCommandOutput {
                    status: Some(0),
                    stdout: serde_json::to_vec(current).unwrap(),
                    stderr: Vec::new(),
                })
            }
            KScreenCommand::Configure(actions) => {
                if self.timeout_configuration {
                    return Err(DisplayError::new(
                        "platform_timeout",
                        "fake KScreen operation timed out",
                    )
                    .after_mutation());
                }
                if !self.ignore_configuration {
                    let current = self.current.as_mut().unwrap();
                    let actions = if self.partial_configuration {
                        actions.iter().take(1).collect::<Vec<_>>()
                    } else {
                        actions.iter().collect::<Vec<_>>()
                    };
                    for action in actions {
                        apply_fake_action(current, action);
                    }
                }
                Ok(KScreenCommandOutput {
                    status: Some(0),
                    stdout: if self.reject_configuration {
                        b"Applying config failed".to_vec()
                    } else {
                        Vec::new()
                    },
                    stderr: Vec::new(),
                })
            }
        }
    }
}

fn apply_fake_action(configuration: &mut Value, action: &KScreenAction) {
    let output_name = action_output(action);
    let output = configuration["outputs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|output| {
            output["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(output_name))
        })
        .unwrap();
    match action {
        KScreenAction::Enabled { enabled, .. } => {
            output["enabled"] = json!(enabled);
            if !enabled {
                output["priority"] = json!(0);
            }
        }
        KScreenAction::Mode { mode, .. } => output["currentModeId"] = json!(mode),
        KScreenAction::Position { x, y, .. } => output["pos"] = json!({"x":x,"y":y}),
        KScreenAction::Scale { scale, .. } => output["scale"] = json!(scale),
        KScreenAction::Rotation { rotation, .. } => output["rotation"] = json!(rotation),
        KScreenAction::Priority { priority, .. } => output["priority"] = json!(priority),
        KScreenAction::Hdr { enabled, .. } => output["hdr"] = json!(enabled),
    }
}

fn action_output(action: &KScreenAction) -> &str {
    match action {
        KScreenAction::Enabled { output, .. }
        | KScreenAction::Mode { output, .. }
        | KScreenAction::Position { output, .. }
        | KScreenAction::Scale { output, .. }
        | KScreenAction::Rotation { output, .. }
        | KScreenAction::Priority { output, .. }
        | KScreenAction::Hdr { output, .. } => output,
    }
}

fn typed_snapshot_for(configuration: &Value) -> KScreenSnapshot {
    let outputs = validated_outputs(configuration).unwrap();
    typed_snapshot(&outputs).unwrap()
}

#[test]
fn read_only_enumeration_and_snapshot_preserve_user_session_topology() {
    let configuration = configuration();
    let rows = enumeration_rows(&configuration, None).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "DP-1");
    assert_eq!(rows[0]["active"], true);
    assert_eq!(
        rows[0]["current"]["refresh"],
        json!({"numerator":60,"denominator":1})
    );
    assert_eq!(rows[1]["connected"], true);
    assert_eq!(rows[1]["active"], false);

    let modes = enumeration_rows(&configuration, Some("DP-1")).unwrap();
    assert_eq!(modes.len(), 2);
    assert_eq!(modes[0]["width"], 1920);
    assert_eq!(modes[0]["refresh"], json!({"numerator":60,"denominator":1}));

    let snapshot = configuration_snapshot(&configuration, "linux_kscreen").unwrap();
    assert_eq!(
        snapshot["configuration"]["displays"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        snapshot["session_state"]["outputs"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(snapshot["topology_revision"].as_str().unwrap().len(), 32);
}

#[test]
fn topology_revision_is_independent_of_kscreen_output_array_order() {
    let mut first = configuration();
    let mut second = first.clone();
    second["outputs"].as_array_mut().unwrap().reverse();
    let first_snapshot = configuration_snapshot(&first, "linux_kscreen").unwrap();
    let second_snapshot = configuration_snapshot(&second, "linux_kscreen").unwrap();
    assert_eq!(
        first_snapshot["topology_revision"],
        second_snapshot["topology_revision"]
    );
    assert_eq!(
        first_snapshot["configuration"],
        second_snapshot["configuration"]
    );

    first["outputs"][0]["scale"] = json!(1.5);
    let changed_snapshot = configuration_snapshot(&first, "linux_kscreen").unwrap();
    assert_ne!(
        first_snapshot["topology_revision"],
        changed_snapshot["topology_revision"]
    );
}

#[test]
fn kscreen_snapshot_retains_reference_output_identity_mode_and_layout_fields() {
    let raw = reference_kde_configuration();
    let snapshot = configuration_snapshot(&raw, "linux_kscreen").unwrap();
    let saved: KScreenSnapshot = serde_json::from_value(snapshot["session_state"].clone()).unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.outputs.len(), 2);
    assert_eq!(saved.outputs[0].id, "DP-1");
    assert_eq!(saved.outputs[0].mode.as_ref().unwrap().id, "25");
    assert_eq!(saved.outputs[0].mode.as_ref().unwrap().width, 2560);
    assert_eq!(saved.outputs[0].position.x, 0);
    assert_eq!(saved.outputs[0].scale, 1.5);
    assert_eq!(saved.outputs[0].rotation, 1);
    assert!(saved.outputs[0].primary);
    assert_eq!(saved.outputs[0].hdr_enabled, Some(true));
    assert_eq!(saved.outputs[1].priority, 2);
    assert!(!saved.outputs[1].primary);
}

#[test]
fn restore_plan_rebinds_exact_outputs_and_reselects_unique_equivalent_mode() {
    let saved: KScreenSnapshot = serde_json::from_value(
        configuration_snapshot(&configuration(), "linux_kscreen").unwrap()["session_state"].clone(),
    )
    .unwrap();
    let mut current = configuration();
    current["outputs"][0]["currentModeId"] = json!("1280x720@60");
    current["outputs"][0]["modes"] = json!([
        mode("current-1920x1080", 1920, 1080, 60.0),
        mode("1280x720@60", 1280, 720, 60.0)
    ]);
    let actions = restoration_actions(&saved, &current).unwrap();
    let arguments = action_arguments(&actions).unwrap();
    assert!(arguments.contains(&"output.DP-1.mode.current-1920x1080".to_owned()));
    assert!(!arguments.contains(&"output.DP-1.position.0,0".to_owned()));
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains("HDMI-A-1"))
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.ends_with(".disable"))
    );
}

#[test]
fn restore_plan_holds_on_disconnect_new_output_or_ambiguous_mode() {
    let saved: KScreenSnapshot = serde_json::from_value(
        configuration_snapshot(&configuration(), "linux_kscreen").unwrap()["session_state"].clone(),
    )
    .unwrap();
    let mut disconnected = configuration();
    disconnected["outputs"][0]["connected"] = json!(false);
    disconnected["outputs"][0]["enabled"] = json!(false);
    disconnected["outputs"][0]["priority"] = json!(0);
    disconnected["outputs"][0]["currentModeId"] = json!("");
    assert_eq!(
        restoration_actions(&saved, &disconnected).unwrap_err().code,
        "topology_changed"
    );

    let mut added = configuration();
    added["outputs"].as_array_mut().unwrap().push(output(
        "DP-2",
        true,
        true,
        "1920x1080@60",
        (1920, 0),
        vec![mode("1920x1080@60", 1920, 1080, 60.0)],
    ));
    assert_eq!(
        restoration_actions(&saved, &added).unwrap_err().code,
        "topology_changed"
    );

    let mut ambiguous = configuration();
    ambiguous["outputs"][0]["modes"] = json!([
        mode("alternative-a", 1920, 1080, 60.0),
        mode("alternative-b", 1920, 1080, 60.0),
        mode("1280x720@60", 1280, 720, 60.0)
    ]);
    ambiguous["outputs"][0]["currentModeId"] = json!("alternative-a");
    assert_eq!(
        restoration_actions(&saved, &ambiguous).unwrap_err().code,
        "unsupported_topology"
    );

    let mut reconnected_baseline_output = configuration();
    reconnected_baseline_output["outputs"][2] = output(
        "DP-3",
        true,
        false,
        "",
        (0, 0),
        vec![mode("1920x1080@60", 1920, 1080, 60.0)],
    );
    assert_eq!(
        restoration_actions(&saved, &reconnected_baseline_output)
            .unwrap_err()
            .code,
        "topology_changed"
    );
}

#[test]
fn kscreen_actions_are_typed_direct_arguments_and_reject_injection() {
    let actions = vec![
        KScreenAction::Mode {
            output: "DP-1".into(),
            mode: "2560x1440@60".into(),
        },
        KScreenAction::Position {
            output: "DP-1".into(),
            x: 0,
            y: 0,
        },
    ];
    assert_eq!(
        action_arguments(&actions).unwrap(),
        vec![
            "output.DP-1.mode.2560x1440@60".to_owned(),
            "output.DP-1.position.0,0".to_owned()
        ]
    );
    assert!(
        KScreenAction::Position {
            output: "DP-1;output.eDP-1.disable".into(),
            x: 0,
            y: 0,
        }
        .argument()
        .is_err()
    );
}

#[test]
fn fake_runner_applies_requested_mode_and_verifies_readback_without_enable_changes() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(json!({
        "displays": [{
            "id":"DP-1", "width":1280, "height":720,
            "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0
        }]
    }))
    .unwrap();
    let mut runner = FakeKScreenRunner {
        current: Some(raw.clone()),
        ..Default::default()
    };
    let mut guards = 0;
    apply_kscreen_snapshot_with(&mut runner, &request, &baseline, &mut || {
        guards += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(guards, 2);
    let configure = runner.commands.iter().find_map(|command| match command {
        KScreenCommand::Configure(actions) => Some(actions),
        KScreenCommand::Query => None,
    });
    let configure = configure.unwrap();
    assert!(configure.contains(&KScreenAction::Mode {
        output: "DP-1".into(),
        mode: "1280x720@60".into(),
    }));
    assert!(
        configure
            .iter()
            .all(|action| action_output(action) == "DP-1")
    );
    let readback = typed_snapshot_for(runner.current.as_ref().unwrap());
    assert!(readback.matches_configuration(&baseline, &request));
}

#[test]
fn hdr_source_apply_and_restore_are_readback_verified() {
    let mut raw = configuration();
    raw["outputs"][0]["hdr"] = json!(true);
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(json!({
        "displays": [{
            "id":"DP-1", "width":1920, "height":1080,
            "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0,
            "hdr_enabled":false
        }]
    }))
    .unwrap();
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    apply_kscreen_snapshot_with(&mut runner, &request, &baseline, &mut || Ok(())).unwrap();
    assert_eq!(runner.current.as_ref().unwrap()["outputs"][0]["hdr"], false);
    assert!(runner.commands.iter().any(|command| matches!(command,
    KScreenCommand::Configure(actions) if actions.contains(&KScreenAction::Hdr {
        output: "DP-1".into(), enabled: false
    }))));
    restore_kscreen_snapshot_with(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert!(baseline.matches_restoration(&typed_snapshot_for(runner.current.as_ref().unwrap())));
    assert!(runner.commands.iter().any(|command| matches!(command,
    KScreenCommand::Configure(actions) if actions.contains(&KScreenAction::Hdr {
        output: "DP-1".into(), enabled: true
    }))));
}

#[test]
fn hdr_activation_follows_verified_modeset_and_reapplies_existing_true_state() {
    for initially_enabled in [false, true] {
        let mut raw = configuration();
        raw["outputs"][0]["hdr"] = json!(initially_enabled);
        let baseline = typed_snapshot_for(&raw);
        let request: Configuration = serde_json::from_value(json!({
            "displays": [{
                "id":"DP-1", "width":1280, "height":720,
                "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0,
                "hdr_enabled":true
            }]
        }))
        .unwrap();
        let mut runner = FakeKScreenRunner {
            current: Some(raw),
            ..Default::default()
        };
        apply_kscreen_snapshot_with(&mut runner, &request, &baseline, &mut || Ok(())).unwrap();
        let transactions: Vec<_> = runner
            .commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                if let KScreenCommand::Configure(actions) = command {
                    Some((index, actions))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(transactions.len(), 2);
        assert!(
            transactions[0]
                .1
                .iter()
                .any(|action| matches!(action, KScreenAction::Mode { .. }))
        );
        assert!(
            transactions[0]
                .1
                .iter()
                .all(|action| !matches!(action, KScreenAction::Hdr { enabled: true, .. }))
        );
        assert_eq!(
            transactions[1].1,
            &vec![KScreenAction::Hdr {
                output: "DP-1".into(),
                enabled: true
            }]
        );
        assert!(
            runner.commands[transactions[0].0 + 1..transactions[1].0]
                .iter()
                .any(|command| matches!(command, KScreenCommand::Query))
        );
        assert!(
            typed_snapshot_for(runner.current.as_ref().unwrap())
                .matches_configuration(&baseline, &request)
        );
    }
}

#[test]
fn hdr_is_not_activated_when_modeset_readback_or_late_authorization_fails() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(json!({
        "displays": [{
            "id":"DP-1", "width":1280, "height":720,
            "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0,
            "hdr_enabled":true
        }]
    }))
    .unwrap();
    for ignore_geometry in [false, true] {
        let mut runner = FakeKScreenRunner {
            current: Some(raw.clone()),
            ignore_configuration: ignore_geometry,
            ..Default::default()
        };
        let mut guards = 0;
        let error = apply_kscreen_snapshot_with(&mut runner, &request, &baseline, &mut || {
            guards += 1;
            if guards == 3 {
                Err(DisplayError::new("cancelled", "lease was withdrawn"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.may_have_changed);
        assert_eq!(
            error.code,
            if ignore_geometry {
                "verification_failed"
            } else {
                "cancelled"
            }
        );
        assert!(!runner.commands.iter().any(|command| matches!(command,
            KScreenCommand::Configure(actions) if actions.iter().any(|action| matches!(action, KScreenAction::Hdr { enabled:true, .. }))
        )));
    }
}

#[test]
fn hdr_restoration_uses_separate_transaction_after_restoring_mode() {
    let mut saved = configuration();
    saved["outputs"][0]["hdr"] = json!(true);
    let baseline = typed_snapshot_for(&saved);
    let mut raw = saved.clone();
    raw["outputs"][0]["currentModeId"] = json!("1280x720@60");
    raw["outputs"][0]["hdr"] = json!(false);
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    restore_kscreen_snapshot_with(&mut runner, &baseline, &mut || Ok(())).unwrap();
    let transactions: Vec<_> = runner
        .commands
        .iter()
        .filter_map(|command| {
            if let KScreenCommand::Configure(actions) = command {
                Some(actions)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(transactions.len(), 2);
    assert!(
        transactions[0]
            .iter()
            .any(|action| matches!(action, KScreenAction::Mode { .. }))
    );
    assert_eq!(
        transactions[1],
        &vec![KScreenAction::Hdr {
            output: "DP-1".into(),
            enabled: true
        }]
    );
    assert!(baseline.matches_restoration(&typed_snapshot_for(runner.current.as_ref().unwrap())));
}

#[test]
fn hdr_source_change_rejects_unreadable_baseline_before_mutation() {
    let mut raw = configuration();
    raw["outputs"][0].as_object_mut().unwrap().remove("hdr");
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(json!({
        "displays": [{
            "id":"DP-1", "width":1920, "height":1080,
            "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0,
            "hdr_enabled":false
        }]
    }))
    .unwrap();
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    let error =
        apply_kscreen_snapshot_with(&mut runner, &request, &baseline, &mut || Ok(())).unwrap_err();
    assert_eq!(error.code, "unsupported_topology");
    assert!(!error.may_have_changed);
    assert!(
        !runner
            .commands
            .iter()
            .any(|command| matches!(command, KScreenCommand::Configure(_)))
    );
}

#[test]
fn restore_is_readback_verified_idempotent_and_never_toggles_last_active_output() {
    let baseline_raw = reference_kde_configuration();
    let baseline = typed_snapshot_for(&baseline_raw);
    let mut current = baseline_raw.clone();
    current["outputs"][0]["modes"] = json!([
        mode("alternate-720", 1280, 720, 60.0),
        mode("current-1440", 2560, 1440, 60.0),
        mode("25", 2560, 1440, 60.0)
    ]);
    current["outputs"][0]["currentModeId"] = json!("alternate-720");
    current["outputs"][0]["pos"] = json!({"x":100,"y":10});
    current["outputs"][0]["scale"] = json!(1.25);
    current["outputs"][0]["rotation"] = json!(2);
    current["outputs"][0]["hdr"] = json!(false);
    let mut runner = FakeKScreenRunner {
        current: Some(current),
        ..Default::default()
    };
    restore_kscreen_snapshot_with(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert!(baseline.matches_restoration(&typed_snapshot_for(runner.current.as_ref().unwrap())));
    let configure_count = runner
        .commands
        .iter()
        .filter(|command| matches!(command, KScreenCommand::Configure(_)))
        .count();
    assert_eq!(
        configure_count, 2,
        "mode restoration precedes HDR activation"
    );
    if let Some(KScreenCommand::Configure(actions)) = runner
        .commands
        .iter()
        .find(|command| matches!(command, KScreenCommand::Configure(_)))
    {
        assert!(actions.iter().all(|action| action_output(action) != "DP-2"));
    }
    restore_kscreen_snapshot_with(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert_eq!(
        runner
            .commands
            .iter()
            .filter(|command| matches!(command, KScreenCommand::Configure(_)))
            .count(),
        configure_count,
        "already restored state does not issue a second mutation",
    );
}

#[test]
fn interrupted_restore_retry_from_a_new_runner_completes_the_durable_baseline() {
    let baseline_raw = reference_kde_configuration();
    let baseline = typed_snapshot_for(&baseline_raw);
    let mut current = baseline_raw.clone();
    current["outputs"][0]["modes"] = json!([
        mode("alternate-720", 1280, 720, 60.0),
        mode("25", 2560, 1440, 60.0)
    ]);
    current["outputs"][0]["currentModeId"] = json!("alternate-720");
    current["outputs"][0]["pos"] = json!({"x":100,"y":10});
    current["outputs"][0]["scale"] = json!(1.25);
    current["outputs"][0]["rotation"] = json!(2);
    current["outputs"][0]["hdr"] = json!(false);

    let mut first_attempt = FakeKScreenRunner {
        current: Some(current),
        reject_configuration: true,
        partial_configuration: true,
        ..Default::default()
    };
    let error =
        restore_kscreen_snapshot_with(&mut first_attempt, &baseline, &mut || Ok(())).unwrap_err();
    assert!(error.may_have_changed);

    let interrupted_state = first_attempt.current.take().unwrap();
    assert!(!baseline.matches_restoration(&typed_snapshot_for(&interrupted_state)));
    let mut restarted_runner = FakeKScreenRunner {
        current: Some(interrupted_state),
        ..Default::default()
    };
    restore_kscreen_snapshot_with(&mut restarted_runner, &baseline, &mut || Ok(())).unwrap();
    assert!(baseline.matches_restoration(&typed_snapshot_for(
        restarted_runner.current.as_ref().unwrap()
    )));
    assert_eq!(
        restarted_runner
            .commands
            .iter()
            .filter(|command| matches!(command, KScreenCommand::Configure(_)))
            .count(),
        2,
        "restart retry restores remaining geometry before activating HDR",
    );
}

#[test]
fn restore_holds_on_hotplug_or_enabled_set_change_before_any_command() {
    let baseline_raw = reference_kde_configuration();
    let baseline = typed_snapshot_for(&baseline_raw);
    let mut hotplugged = baseline_raw.clone();
    hotplugged["outputs"].as_array_mut().unwrap().push(output(
        "DP-3",
        true,
        false,
        "",
        (0, 0),
        vec![mode("mode-1", 1920, 1080, 60.0)],
    ));
    for current in [hotplugged, {
        let mut changed = baseline_raw.clone();
        changed["outputs"][1]["enabled"] = json!(false);
        changed["outputs"][1]["priority"] = json!(0);
        changed
    }] {
        let mut runner = FakeKScreenRunner {
            current: Some(current),
            ..Default::default()
        };
        let error =
            restore_kscreen_snapshot_with(&mut runner, &baseline, &mut || Ok(())).unwrap_err();
        assert_eq!(error.code, "topology_changed");
        assert!(
            !runner
                .commands
                .iter()
                .any(|command| matches!(command, KScreenCommand::Configure(_)))
        );
    }
}

#[test]
fn partial_rejection_timeout_and_readback_mismatch_mark_uncertain_mutation() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(json!({
        "displays": [{
            "id":"DP-1", "width":1280, "height":720,
            "refresh":{"numerator":60,"denominator":1}, "x":0,"y":0
        }]
    }))
    .unwrap();

    let mut rejected = FakeKScreenRunner {
        current: Some(raw.clone()),
        reject_configuration: true,
        ..Default::default()
    };
    let error = apply_kscreen_snapshot_with(&mut rejected, &request, &baseline, &mut || Ok(()))
        .unwrap_err();
    assert!(error.may_have_changed);
    assert_eq!(error.code, "platform_error");

    let mut timeout = FakeKScreenRunner {
        current: Some(raw.clone()),
        timeout_configuration: true,
        ..Default::default()
    };
    let error =
        apply_kscreen_snapshot_with(&mut timeout, &request, &baseline, &mut || Ok(())).unwrap_err();
    assert!(error.may_have_changed);
    assert_eq!(error.code, "platform_timeout");

    let mut mismatch = FakeKScreenRunner {
        current: Some(raw),
        ignore_configuration: true,
        ..Default::default()
    };
    let error = apply_kscreen_snapshot_with(&mut mismatch, &request, &baseline, &mut || Ok(()))
        .unwrap_err();
    assert!(error.may_have_changed);
    assert_eq!(error.code, "verification_failed");
}

#[test]
fn malformed_or_unsafe_kscreen_id_fails_closed() {
    let mut malformed = configuration();
    malformed["outputs"][0]["name"] = json!("DP-1;disable-eDP-1");
    assert_eq!(
        validated_outputs(&malformed).unwrap_err().code,
        "platform_error"
    );
}

#[test]
fn linux_normal_topology_support_uses_kscreen_recovery_not_ccd() {
    let mut platform = LinuxPlatform;
    assert!(platform.supported());
    assert_eq!(
        platform.snapshot().unwrap_err().code,
        "unsupported_platform"
    );
}

fn only_hdmi_request(hdr: bool) -> Configuration {
    serde_json::from_value(json!({"displays":[{
        "id":"HDMI-A-1","width":1920,"height":1080,
        "refresh":{"numerator":60,"denominator":1},"x":0,"y":0,
        "hdr_enabled":hdr
    }]}))
    .unwrap()
}

#[test]
fn normal_topology_activates_inactive_target_then_restores_original_enabled_set() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let request = only_hdmi_request(false);
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    topology::apply(&mut runner, &request, &baseline, &mut || Ok(())).unwrap();
    let configured = typed_snapshot_for(runner.current.as_ref().unwrap());
    assert!(configured.matches_topology_configuration(&baseline, &request));
    assert!(
        !configured
            .outputs
            .iter()
            .find(|output| output.id == "DP-1")
            .unwrap()
            .enabled
    );
    let transactions: Vec<_> = runner
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            if let KScreenCommand::Configure(actions) = command {
                Some((index, actions))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(transactions.len(), 2);
    assert!(transactions[0].1.iter().any(
        |action| matches!(action,KScreenAction::Enabled{output,enabled:true} if output=="HDMI-A-1")
    ));
    assert!(
        transactions[0]
            .1
            .iter()
            .all(|action| !matches!(action, KScreenAction::Enabled { enabled: false, .. }))
    );
    assert!(transactions[1].1.iter().any(
        |action| matches!(action,KScreenAction::Enabled{output,enabled:false} if output=="DP-1")
    ));
    assert!(
        runner.commands[transactions[0].0 + 1..transactions[1].0]
            .iter()
            .any(|command| matches!(command, KScreenCommand::Query))
    );
    topology::restore(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert!(
        baseline
            .matches_topology_restoration(&typed_snapshot_for(runner.current.as_ref().unwrap()))
    );
    let count = runner
        .commands
        .iter()
        .filter(|command| matches!(command, KScreenCommand::Configure(_)))
        .count();
    topology::restore(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert_eq!(
        count,
        runner
            .commands
            .iter()
            .filter(|command| matches!(command, KScreenCommand::Configure(_)))
            .count()
    );
}

#[test]
fn normal_topology_changes_primary_while_preserving_peer_scale_and_mode() {
    let raw = reference_kde_configuration();
    let baseline = typed_snapshot_for(&raw);
    let request:Configuration=serde_json::from_value(json!({"displays":[
        {"id":"DP-2","width":1920,"height":1080,"refresh":{"numerator":60,"denominator":1},"x":0,"y":0,"rotation":"rotate90"},
        {"id":"DP-1","width":2560,"height":1440,"refresh":{"numerator":60,"denominator":1},"x":-1707,"y":0}
    ]})).unwrap();
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    topology::apply(&mut runner, &request, &baseline, &mut || Ok(())).unwrap();
    let observed = typed_snapshot_for(runner.current.as_ref().unwrap());
    assert!(observed.matches_topology_configuration(&baseline, &request));
    assert!(
        observed
            .outputs
            .iter()
            .find(|output| output.id == "DP-2")
            .unwrap()
            .primary
    );
    assert_eq!(observed.outputs[0].scale, baseline.outputs[0].scale);
    topology::restore(&mut runner, &baseline, &mut || Ok(())).unwrap();
    assert!(
        baseline
            .matches_topology_restoration(&typed_snapshot_for(runner.current.as_ref().unwrap()))
    );
}

#[test]
fn normal_topology_rejects_invalid_modes_empty_target_set_and_hotplug_before_effects() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let mut invalid = only_hdmi_request(false);
    invalid.displays[0].width = 2560;
    for request in [invalid, Configuration { displays: vec![] }] {
        let mut runner = FakeKScreenRunner {
            current: Some(raw.clone()),
            ..Default::default()
        };
        assert!(topology::apply(&mut runner, &request, &baseline, &mut || Ok(())).is_err());
        assert!(
            runner
                .commands
                .iter()
                .all(|command| matches!(command, KScreenCommand::Query))
        );
    }
    let mut hotplug = raw;
    hotplug["outputs"][1]["connected"] = json!(false);
    let mut runner = FakeKScreenRunner {
        current: Some(hotplug),
        ..Default::default()
    };
    assert_eq!(
        topology::apply(
            &mut runner,
            &only_hdmi_request(false),
            &baseline,
            &mut || Ok(())
        )
        .unwrap_err()
        .code,
        "topology_changed"
    );
    assert!(
        runner
            .commands
            .iter()
            .all(|command| matches!(command, KScreenCommand::Query))
    );
}

#[test]
fn partial_normal_topology_keeps_old_scanout_and_marks_recovery_required() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        partial_configuration: true,
        ..Default::default()
    };
    let error = topology::apply(
        &mut runner,
        &only_hdmi_request(false),
        &baseline,
        &mut || Ok(()),
    )
    .unwrap_err();
    assert!(error.may_have_changed);
    assert_eq!(
        runner.current.as_ref().unwrap()["outputs"][0]["enabled"],
        true
    );
}

#[test]
fn full_topology_hdr_waits_for_enabled_target_and_exact_geometry_before_activation() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let request = only_hdmi_request(true);
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    topology::apply(&mut runner, &request, &baseline, &mut || Ok(())).unwrap();
    let transactions: Vec<_> = runner
        .commands
        .iter()
        .filter_map(|command| {
            if let KScreenCommand::Configure(actions) = command {
                Some(actions)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(transactions.len(), 3);
    assert!(transactions[0].iter().any(
        |action| matches!(action,KScreenAction::Enabled{output,enabled:true} if output=="HDMI-A-1")
    ));
    assert!(
        transactions[0]
            .iter()
            .all(|action| !matches!(action, KScreenAction::Hdr { enabled: true, .. }))
    );
    assert_eq!(
        transactions[1],
        &vec![KScreenAction::Hdr {
            output: "HDMI-A-1".into(),
            enabled: true
        }]
    );
    assert!(
        typed_snapshot_for(runner.current.as_ref().unwrap())
            .matches_topology_configuration(&baseline, &request)
    );
}

#[test]
fn old_scanout_is_never_disabled_when_replacement_readback_is_ignored() {
    for partial in [false, true] {
        let raw = configuration();
        let baseline = typed_snapshot_for(&raw);
        let mut runner = FakeKScreenRunner {
            current: Some(raw),
            ignore_configuration: !partial,
            partial_configuration: partial,
            ..Default::default()
        };
        let error = topology::apply(
            &mut runner,
            &only_hdmi_request(false),
            &baseline,
            &mut || Ok(()),
        )
        .unwrap_err();
        assert!(error.may_have_changed);
        assert!(runner.commands.iter().all(|command|!matches!(command,KScreenCommand::Configure(actions) if actions.iter().any(|action|matches!(action,KScreenAction::Enabled{enabled:false,..})))));
        assert_eq!(
            runner.current.as_ref().unwrap()["outputs"][0]["enabled"],
            true
        );
    }
}

#[test]
fn full_topology_rejects_replaced_or_unidentified_physical_monitor_before_effects() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    for uuid in [Value::Null, json!("replacement-monitor")] {
        let mut replaced = raw.clone();
        replaced["outputs"][1]["uuid"] = uuid;
        let mut runner = FakeKScreenRunner {
            current: Some(replaced),
            ..Default::default()
        };
        assert!(
            topology::apply(
                &mut runner,
                &only_hdmi_request(false),
                &baseline,
                &mut || Ok(())
            )
            .is_err()
        );
        assert!(
            runner
                .commands
                .iter()
                .all(|command| matches!(command, KScreenCommand::Query))
        );
        assert_eq!(
            topology::restore(&mut runner, &baseline, &mut || Ok(()))
                .unwrap_err()
                .code,
            "recovery_blocked"
        );
        assert!(
            runner
                .commands
                .iter()
                .all(|command| matches!(command, KScreenCommand::Query))
        );
    }
}

#[test]
fn ensure_active_preserves_existing_secondary_priorities_even_if_request_order_differs() {
    let mut raw = reference_kde_configuration();
    raw["outputs"][1]["priority"] = json!(4);
    let baseline = typed_snapshot_for(&raw);
    let mut request: Configuration = serde_json::from_value(
        configuration_snapshot(&raw, "test").unwrap()["configuration"].clone(),
    )
    .unwrap();
    request.displays.reverse();
    let expected = baseline.configured_topology(&request).unwrap();
    assert_eq!(expected.outputs[1].priority, 4);
}

#[test]
fn late_authorization_withdrawal_keeps_previous_scanout_after_replacement_activation() {
    let raw = configuration();
    let baseline = typed_snapshot_for(&raw);
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    let mut guards = 0;
    let error = topology::apply(
        &mut runner,
        &only_hdmi_request(false),
        &baseline,
        &mut || {
            guards += 1;
            if guards == 3 {
                Err(DisplayError::new("cancelled", "display lease ended"))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "cancelled");
    assert!(error.may_have_changed);
    assert_eq!(
        runner.current.as_ref().unwrap()["outputs"][0]["enabled"],
        true
    );
    assert!(runner.commands.iter().all(|command| !matches!(command,
        KScreenCommand::Configure(actions) if actions.iter().any(|action| matches!(action, KScreenAction::Enabled { enabled:false, .. }))
    )));
}

fn configuration_with_managed_peer(connected: bool) -> Value {
    let mut raw = configuration();
    let managed = output(
        "Virtual-1",
        connected,
        connected,
        if connected { "1920x1080@60" } else { "" },
        (3840, 0),
        vec![mode("1920x1080@60", 1920, 1080, 60.0)],
    );
    raw["outputs"].as_array_mut().unwrap().push(managed);
    raw
}

#[test]
fn normal_physical_transactions_reject_managed_lifecycle_rows_before_effects() {
    for connected in [false, true] {
        let raw = configuration_with_managed_peer(connected);
        let baseline = typed_snapshot_for(&raw);
        let request = only_hdmi_request(false);
        let error = baseline
            .validate_topology_configuration(&request)
            .unwrap_err();
        assert_eq!(error.code, "unsupported_topology");
        assert!(error.message.contains("managed virtual outputs"));
        let mut runner = FakeKScreenRunner {
            current: Some(raw),
            ..Default::default()
        };
        let error = topology::apply(&mut runner, &request, &baseline, &mut || Ok(())).unwrap_err();
        assert_eq!(error.code, "unsupported_topology");
        assert!(
            runner
                .commands
                .iter()
                .all(|command| matches!(command, KScreenCommand::Query))
        );
    }
}

#[test]
fn normal_restore_never_recreates_a_disappeared_managed_peer_from_an_old_baseline() {
    let baseline_raw = configuration_with_managed_peer(true);
    let baseline = typed_snapshot_for(&baseline_raw);
    let mut current = baseline_raw;
    current["outputs"]
        .as_array_mut()
        .unwrap()
        .retain(|output| output["name"] != "Virtual-1");
    let mut runner = FakeKScreenRunner {
        current: Some(current),
        ..Default::default()
    };
    let error = topology::restore(&mut runner, &baseline, &mut || Ok(())).unwrap_err();
    assert_eq!(error.code, "unsupported_topology");
    assert!(!error.may_have_changed);
    assert!(runner.commands.is_empty());
    assert_eq!(
        runner.current.as_ref().unwrap()["outputs"][0]["enabled"],
        true
    );
}

#[test]
fn conservative_normal_guard_preserves_existing_scoped_mixed_topology_support() {
    let raw = configuration_with_managed_peer(true);
    let baseline = typed_snapshot_for(&raw);
    let request: Configuration = serde_json::from_value(
        configuration_snapshot(&raw, "test").unwrap()["configuration"].clone(),
    )
    .unwrap();
    assert!(baseline.validate_requested_topology(&request).is_ok());
    let mut runner = FakeKScreenRunner {
        current: Some(raw),
        ..Default::default()
    };
    apply_scoped_kscreen_snapshot_with(&mut runner, &request, &baseline, None, &mut || Ok(()))
        .unwrap();
    assert!(
        typed_snapshot_for(runner.current.as_ref().unwrap())
            .matches_configuration(&baseline, &request)
    );
}

#[test]
#[ignore = "requires an attached Wayland/KScreen session and readable DRM identity; read-only"]
fn current_physical_kscreen_inventory_has_recoverable_monitor_identities() {
    let raw = query_configuration().unwrap();
    let snapshot = typed_snapshot_for(&raw);
    let physical: Vec<_> = snapshot
        .outputs
        .iter()
        .filter(|output| {
            output.connected && !output.id.to_ascii_lowercase().starts_with("virtual-")
        })
        .collect();
    assert!(
        !physical.is_empty(),
        "no connected physical output was observed"
    );
    assert!(
        physical
            .iter()
            .all(|output| output.hardware_identity.is_some()),
        "every connected physical output needs an unambiguous observed identity"
    );
    let fresh = query_configuration().unwrap();
    let fresh_snapshot = typed_snapshot_for(&fresh);
    for output in physical {
        let current = fresh_snapshot
            .outputs
            .iter()
            .find(|current| current.id == output.id)
            .expect("physical output disappeared during read-only qualification");
        assert_eq!(output.hardware_identity, current.hardware_identity);
    }
}

#[test]
fn hdr_profile_capability_is_typed_unsupported_without_mutation() {
    use amaterasu_contracts::display_color_profile::{
        HdrProfileResolutionRequest, HdrProfileSelection,
    };

    let mut platform = LinuxPlatform;
    let request = HdrProfileResolutionRequest {
        selection: HdrProfileSelection {
            id: "display.icc".into(),
        },
        lifecycle_target: None,
        target_id: "DP-1".into(),
        target_revision: "topology-revision".into(),
        policy_revision: "policy-revision".into(),
    };
    let error = platform.resolve_hdr_profile(&request).unwrap_err();
    assert_eq!(error.code, "unsupported_hdr_profile");
    assert!(platform.supported());
}
