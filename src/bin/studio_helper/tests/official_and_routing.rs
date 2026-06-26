    #[test]
    fn select_instance_for_route_uses_full_identity() {
        let mut instances = HashMap::new();
        instances.insert(
            "older".to_owned(),
            test_instance("93795519121520", Some("t1"), 50),
        );
        instances.insert(
            "newer".to_owned(),
            test_instance("93795519121520", Some("t1"), 5),
        );

        let selected = select_instance_for_route("93795519121520", Some("t1"), &instances);
        assert_eq!(selected.as_deref(), Some("newer"));

        let missing = select_instance_for_route("93795519121520", Some("missing"), &instances);
        assert!(
            missing.is_none(),
            "unknown task_id should not fall back by place"
        );
    }

    #[test]
    fn official_adapter_allows_only_curated_actions() {
        assert!(ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"generate_mesh"));
        assert!(ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"search_creator_store"));
        assert!(!ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"execute_luau"));
        assert!(!ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"script_read"));
        assert_eq!(
            official_tool_for_action("generate_mesh").unwrap(),
            "generate_mesh"
        );
        assert_eq!(
            official_tool_for_action("search_creator_store").unwrap(),
            "search_creator_store"
        );
        assert!(official_tool_for_action("execute_luau").is_err());
        assert!(official_tool_for_action("script_read").is_err());
    }

    #[test]
    fn official_adapter_keeps_process_while_studio_ws_host_is_starting() {
        let transient = eyre!(
            "official MCP tool list_roblox_studios returned error: Not connected to the WS host"
        );
        assert!(!should_restart_official_process_after_error(&transient));
        assert!(!official_adapter_error_keeps_ready(&transient));

        let no_connected_studio = eyre!("official MCP found no connected Roblox Studio instances");
        assert!(!should_restart_official_process_after_error(
            &no_connected_studio
        ));
        assert!(!official_adapter_error_keeps_ready(&no_connected_studio));

        let opening_place = eyre!(
            "official MCP tool search_creator_store returned error: Execution is prevented because previously active Studio has disconnected or doesn't have a place opened."
        );
        assert!(!should_restart_official_process_after_error(&opening_place));
        assert!(!official_adapter_error_keeps_ready(&opening_place));

        let fatal = eyre!("official MCP process closed stdout");
        assert!(should_restart_official_process_after_error(&fatal));
        assert!(!official_adapter_error_keeps_ready(&fatal));
    }

    #[test]
    fn official_adapter_keeps_ready_for_tool_and_request_failures() {
        let workflow_failed =
            eyre!("official MCP tool wait_job_finished returned error: Workflow failed");
        assert!(!should_restart_official_process_after_error(
            &workflow_failed
        ));
        assert!(official_adapter_error_keeps_ready(&workflow_failed));

        let missing_generation = eyre!(
            "official MCP tool wait_job_finished returned error: No job found with generation ID: generation-id-does-not-exist"
        );
        assert!(!should_restart_official_process_after_error(
            &missing_generation
        ));
        assert!(official_adapter_error_keeps_ready(&missing_generation));

        let required_generation_id =
            eyre!("official MCP tool wait_job_finished returned error: generationId required");
        assert!(!should_restart_official_process_after_error(
            &required_generation_id
        ));
        assert!(official_adapter_error_keeps_ready(&required_generation_id));

        let missing_generation_id =
            eyre!("official MCP tool wait_job_finished returned error: generation_id missing");
        assert!(!should_restart_official_process_after_error(
            &missing_generation_id
        ));
        assert!(official_adapter_error_keeps_ready(&missing_generation_id));

        let required_job_id =
            eyre!("official MCP tool wait_job_finished returned error: job id required");
        assert!(!should_restart_official_process_after_error(
            &required_job_id
        ));
        assert!(official_adapter_error_keeps_ready(&required_job_id));

        let poll_generation_failed = eyre!(
            "official MCP tool wait_job_finished returned error: failed to poll generation ID result"
        );
        assert!(should_restart_official_process_after_error(
            &poll_generation_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&poll_generation_failed));

        let generate_failed =
            eyre!("official MCP tool generate_mesh returned error: policy rejected input");
        assert!(!should_restart_official_process_after_error(
            &generate_failed
        ));
        assert!(official_adapter_error_keeps_ready(&generate_failed));

        let generate_channel_failed =
            eyre!("official MCP tool generate_mesh returned error: Studio connection lost");
        assert!(should_restart_official_process_after_error(
            &generate_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &generate_channel_failed
        ));

        let search_channel_failed =
            eyre!("official MCP tool search_creator_store returned error: Studio connection lost");
        assert!(should_restart_official_process_after_error(
            &search_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&search_channel_failed));

        let insert_channel_failed = eyre!(
            "official MCP tool insert_from_creator_store returned error: Roblox Studio unavailable"
        );
        assert!(should_restart_official_process_after_error(
            &insert_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&insert_channel_failed));

        let store_image_channel_failed =
            eyre!("official MCP tool store_image returned error: internal adapter failure");
        assert!(should_restart_official_process_after_error(
            &store_image_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &store_image_channel_failed
        ));

        let store_image_file_failed =
            eyre!("official MCP tool store_image returned error: file not found");
        assert!(!should_restart_official_process_after_error(
            &store_image_file_failed
        ));
        assert!(official_adapter_error_keeps_ready(&store_image_file_failed));

        let internal_studio_tool_failed =
            eyre!("official MCP tool list_roblox_studios returned error: unexpected failure");
        assert!(should_restart_official_process_after_error(
            &internal_studio_tool_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &internal_studio_tool_failed
        ));

        let bad_image = eyre!("store_image imageBase64 content does not match mimeType");
        assert!(!should_restart_official_process_after_error(&bad_image));
        assert!(official_adapter_error_keeps_ready(&bad_image));

        let unsupported_action = eyre!("unsupported official MCP adapter action: unknown_action");
        assert!(!should_restart_official_process_after_error(
            &unsupported_action
        ));
        assert!(!official_adapter_error_keeps_ready(&unsupported_action));
        assert!(official_adapter_error_preserves_state(&unsupported_action));
    }

    #[test]
    fn derive_remote_helper_ws_url_supports_http_and_https() {
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("https://example.com"),
            "wss://example.com/ws/helper"
        );
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("http://127.0.0.1:44756"),
            "ws://127.0.0.1:44756/ws/helper"
        );
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("ws://127.0.0.1:44756/"),
            "ws://127.0.0.1:44756/ws/helper"
        );
    }

    #[test]
    fn helper_id_from_machine_guid_is_stable() {
        let machine_guid = "12345678-90AB-CDEF-1234-567890ABCDEF";
        let derived_a = helper_id_from_machine_guid(machine_guid).expect("helper id should derive");
        let derived_b = helper_id_from_machine_guid(machine_guid).expect("helper id should derive");
        assert_eq!(derived_a, derived_b);
        assert!(derived_a.starts_with("h_"));
        assert!(derived_a
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'));
    }

    #[test]
    fn parse_machine_guid_reg_query_output_extracts_guid() {
        let guid = parse_machine_guid_reg_query_output(
            "\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\n    MachineGuid    REG_SZ    abcdef12-3456-7890-abcd-ef1234567890\n",
        )
        .expect("MachineGuid should parse");
        assert_eq!(guid, "abcdef12-3456-7890-abcd-ef1234567890");
    }

    #[test]
    fn parse_helper_id_conflict_extracts_message() {
        let message = parse_helper_id_conflict(
            StatusCode::CONFLICT,
            None,
            r#"{"code":"helper_id_conflict","message":"helper_id already active"}"#,
        )
        .expect("helper_id_conflict should parse");
        assert_eq!(message, "helper_id already active");
        let header_message = parse_helper_id_conflict(
            StatusCode::CONFLICT,
            Some("helper_id_conflict"),
            "helper_id already active",
        )
        .expect("helper_id_conflict header should parse");
        assert_eq!(header_message, "helper_id already active");
        assert!(parse_helper_id_conflict(StatusCode::BAD_REQUEST, None, "{}").is_none());
    }

    #[test]
    fn claim_task_response_ignores_internal_active_launch_id_field() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "universe_id": "9838206573",
                "active_launch_id": "l_internal_only",
                "routes": {
                    "rojo_base_url": "https://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com",
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com",
                    "runtime_log_base_url": "https://93795519121520-t_example-runtime-log-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("claim payload should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.task_id, "t_example");
        assert_eq!(task.place_id, "93795519121520");
        assert_eq!(task.universe_id.as_deref(), Some("9838206573"));
    }

    #[test]
    fn claim_task_response_accepts_legacy_game_id_field() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "game_id": "9838206573",
                "routes": {
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("legacy claim payload should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.game_id.as_deref(), Some("9838206573"));
    }

    #[test]
    fn claim_task_response_accepts_both_universe_id_and_game_id_fields() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "universe_id": "9838206573",
                "game_id": "9838206573",
                "routes": {
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("claim payload with both id fields should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.universe_id.as_deref(), Some("9838206573"));
        assert_eq!(task.game_id.as_deref(), Some("9838206573"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_plugin_routing_decision_allows_explicit_task_hint() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let decision =
            resolve_plugin_routing_decision(&state, "93795519121520", Some("task_a"), Some(999))
                .expect("explicit task hint should resolve to managed task");
        assert_eq!(decision.task_id, "task_a");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_plugin_routing_decision_accepts_helper_launched_pid() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 222,
                launched_by_helper: true,
                kill_on_close_job: None,
                child: None,
            },
        );

        let decision = resolve_plugin_routing_decision(&state, "93795519121520", None, Some(222))
            .expect("helper-launched Studio should stay managed");
        assert_eq!(decision.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_request_prefers_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        state.launch_processes.insert(
            "task_b".to_owned(),
            LaunchProcessRecord {
                task_id: "task_b".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 222,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );

        let selected = resolve_claimed_task_for_request(&state, "93795519121520", None, Some(222))
            .expect("pid-bound task should resolve");
        assert_eq!(selected.task_id, "task_b");
    }

    #[test]
    fn resolve_claimed_task_for_request_requires_identity_without_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let error =
            match resolve_claimed_task_for_request(&state, "93795519121520", None, Some(999)) {
                Ok(_) => panic!("expected unbound Studio pid without explicit identity to fail"),
                Err(error) => error,
            };

        assert!(error
            .to_string()
            .contains("task_id is required unless the Studio pid was launched by helper"));
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_accepts_unique_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let selected =
            resolve_claimed_task_for_plugin_request(&state, "93795519121520", None, Some(999))
                .expect("unique claimed task for place should resolve without plugin task hint");

        assert_eq!(selected.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_accepts_task_hint_without_extra_fields() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let selected = resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            Some("task_a"),
            Some(999),
        )
        .expect("task hint should resolve with task_id only");

        assert_eq!(selected.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_rejects_ambiguous_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );

        let error = match resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            None,
            Some(999),
        ) {
            Ok(_) => panic!(
                "expected ambiguous place binding without helper pid or explicit task to fail"
            ),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("multiple claimed tasks found for place_id 93795519121520"));
    }

    #[test]
    fn task_scoped_rojo_forward_resolves_ambiguous_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );

        let selected = select_claimed_task(&state, "93795519121520", Some("task_b"))
            .expect("task-scoped Rojo forward should not depend on place uniqueness");

        assert_eq!(selected.task_id, "task_b");
    }

    #[test]
    fn bind_launch_process_preserves_helper_launch_ownership() {
        let mut state = empty_helper_state();
        let claimed_task = test_claimed_task("task_a", "93795519121520");
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );

        bind_launch_process_to_pid(&mut state, &claimed_task, 222)
            .expect("plugin registration should update pid binding");

        let launch = state
            .launch_processes
            .get("task_a")
            .expect("launch process should stay tracked");
        assert_eq!(launch.studio_pid, 222);
        assert!(launch.launched_by_helper);
    }

    #[test]
    fn release_claimed_task_cleans_exact_route_state() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.instances.insert(
            "instance_a".to_owned(),
            test_instance("93795519121520", Some("task_a"), 5),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: false,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        state
            .last_remote_errors
            .insert(task_connection_key("task_a"), "boom".to_owned());

        let pending_termination = release_claimed_task(&mut state, "task_a", false, "test release");

        assert!(state.claimed_tasks.is_empty());
        assert!(state.instances.is_empty());
        assert!(state.launch_processes.is_empty());
        assert!(state.last_remote_errors.is_empty());
        assert!(pending_termination.is_none());
    }

    #[test]
    fn validate_remote_ready_ack_checks_full_route_identity() {
        validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "93795519121520",
            Some("t_example"),
        )
        .expect("exact ready ack should pass");

        let missing_task =
            validate_remote_ready_ack("93795519121520", Some("t_example"), "93795519121520", None)
                .expect_err("missing task identity should fail");
        assert!(missing_task
            .to_string()
            .contains("remote MCP ready ack task_id mismatch"));

        let wrong_task = validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "93795519121520",
            Some("t_other"),
        )
        .expect_err("task mismatch should fail");
        assert!(wrong_task
            .to_string()
            .contains("remote MCP ready ack task_id mismatch"));

        let wrong_place = validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "111",
            Some("t_example"),
        )
        .expect_err("place mismatch should fail");
        assert!(wrong_place
            .to_string()
            .contains("remote MCP ready ack place_id mismatch"));
    }

    #[test]
    fn store_image_legacy_file_path_passes_through() {
        let (arguments, cleanup) = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({ "filePath": "C:\\tmp\\probe.png" }),
        )
        .expect("legacy Windows filePath should pass through");

        assert!(cleanup.is_none());
        assert_eq!(arguments["filePath"], "C:\\tmp\\probe.png");
    }

    #[test]
    fn store_image_base64_writes_task_scoped_temp_file() {
        let png_base64 =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=";
        let (arguments, cleanup) = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({
                "imageBase64": png_base64,
                "mimeType": "image/png",
                "fileName": "..\\bad name.png",
            }),
        )
        .expect("base64 image should be written to helper temp storage");
        let cleanup = cleanup.expect("base64 mode should create a temp cleanup guard");
        let path = cleanup.path.clone();

        assert_eq!(
            arguments["filePath"].as_str(),
            Some(path.to_string_lossy().as_ref())
        );
        assert!(path.exists());
        assert!(path
            .components()
            .any(|component| component.as_os_str() == "official-images"));
        assert!(path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .ends_with(".png"));

        drop(cleanup);
        assert!(!path.exists());
    }

    #[test]
    fn store_image_base64_rejects_mime_mismatch() {
        let jpeg_header_as_base64 = "/9j/AA==";
        let error = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({
                "imageBase64": jpeg_header_as_base64,
                "mimeType": "image/png",
            }),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match mimeType"));
    }
