# Python → Rust test parity

Every Python test in `gymclaw/tests` (173) mapped to one category. Regenerate the table by hand when tests change.

- **ported**: an existing Rust test covers the same scenario (often under another name).
- **ported now**: added on `rust/parity`, same scenario and assertions.
- **obsolete**: the behavior itself is gone (OpenClaw, warm worker, argparse CLI, alembic chain, demo mode) or the type system rules it out.
- **covered elsewhere**: no single literal port; the named Rust tests cover the live behavior.

| Category | Count |
|---|---|
| ported | 83 |
| ported now | 58 |
| obsolete | 23 |
| covered elsewhere | 9 |
| total | 173 |

## gymclaw/tests/test_adaptation_audit.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_reorder_retry_deferred_and_audit` | ported now | src/domain/adaptation.rs::reorder_retry_deferred_and_audit |
| `test_substitute_only_remaining_volume_and_hold_partial_target` | ported now | src/domain/adaptation.rs::substitute_only_remaining_volume_and_hold_partial_target |
| `test_dependency_blocks_unsafe_reordering_and_explicit_skips` | ported now | src/domain/adaptation.rs::dependency_blocks_unsafe_reordering_and_explicit_skips |
| `test_eta_includes_deferred_volume_and_elapsed_time` | ported now | src/domain/adaptation.rs::eta_includes_deferred_volume_and_elapsed_time |
| `test_replacement_prerequisites_are_preserved` | ported now | src/domain/adaptation.rs::replacement_prerequisites_are_preserved |
| `test_invalid_substitution_preserves_state` | ported now | src/domain/adaptation.rs::invalid_substitution_preserves_state |

## gymclaw/tests/test_availability.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_inclusive_local_date_handles_dst_and_restart` | ported now | src/tools/plan.rs::inclusive_local_date_handles_dst_and_retry |
| `test_pause_repairs_future_slots_no_makeup_volume_and_expires` | ported now | src/domain/availability.rs::pause_repairs_future_slots_no_makeup_volume_and_expires |
| `test_locked_slot_preserved_but_reminders_suppressed_until_explicit_removal` | ported now | src/domain/availability.rs::locked_slot_preserved_but_reminders_suppressed_until_explicit_removal |
| `test_explicit_cancel_locked_records_later_user_intent` | ported now | src/domain/availability.rs::explicit_cancel_locked_records_later_user_intent |

## gymclaw/tests/test_body.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_backfill_uses_photo_dates_and_reimport_is_a_noop` | ported | src/domain/body.rs::backfill_uses_photo_dates_and_reimport_is_a_noop |
| `test_compressed_photo_without_date_is_refused` | ported now | src/domain/body.rs::compressed_photo_without_date_is_refused |
| `test_briefing_line_shows_trend_then_nudges_when_stale` | ported | src/domain/body.rs::briefing_line_shows_trend_then_nudges_when_stale |

## gymclaw/tests/test_calendar_busy.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_recurring_blocker_honors_dst_and_moved_exception` | ported now | src/domain/calendar_busy.rs::recurring_blocker_honors_dst_and_moved_exception |
| `test_all_day_recurring_cancellation_and_transparent_event` | ported now | src/domain/calendar_busy.rs::all_day_recurring_cancellation_and_transparent_event |

## gymclaw/tests/test_calendar_cli.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_calendar_json_cli_plan_publish_manual_edit_and_restart` | covered elsewhere | fixture-file CLI is gone; plan/publish/manual-move/lock behavior in src/domain/calendar.rs::move_kept_reminders_replaced_other_sessions_stay and src/domain/weekly.rs::weekly_plan_retry_one_briefing_and_no_duplicate_calendar_intent |
| `test_calendar_cli_safe_defaults_and_scope_errors` | covered elsewhere | write gate: src/domain/calendar.rs::live_write_gate_runs_before_any_provider_call; scope binding: src/providers/google.rs::refresh_writes_google_auth_format_back. FIXTURE_DB_REQUIRED / CALENDAR_ID_REQUIRED belonged to the removed fixture and auth CLI |
| `test_move_session_picks_valid_slot_or_explains` | ported now | src/tools/plan.rs::move_session_picks_valid_slot_or_explains |
| `test_owner_adds_sessions_and_picks_workouts` | ported now | src/tools/plan.rs::owner_adds_sessions_and_picks_workouts |
| `test_profile_window_change_moves_sessions_that_no_longer_fit` | ported now | src/tools/plan.rs::profile_window_change_moves_sessions_that_no_longer_fit |

## gymclaw/tests/test_calendar_provider.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_pagination_keeps_sync_params` | ported | src/providers/google.rs::pagination_keeps_sync_params_and_quotes_calendar |
| `test_expired_token_and_safe_errors` | ported | src/providers/google.rs::expired_token_and_safe_errors |
| `test_all_day_exclusive_end_and_minimal_deletion` | ported | src/providers/calendar.rs::all_day_exclusive_end_and_minimal_raw |
| `test_create_retry_verifies_ownership_and_does_not_duplicate` | ported | src/providers/google.rs::create_retry_verifies_ownership |
| `test_update_guard_and_if_match` | ported | src/providers/google.rs::update_uses_if_match_and_detects_conflicts; its delete-ownership assert ported now as src/providers/google.rs::delete_guard_refuses_events_gymclaw_does_not_own |
| `test_oauth_private_sqlite_and_calendar_binding` | covered elsewhere | credential storage and calendar binding: src/providers/google.rs::refresh_writes_google_auth_format_back. The InstalledAppFlow local-server login is Python-only; the 0600 chmod (src/providers/google.rs:60) has no test |

## gymclaw/tests/test_calendar_reconcile.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_move_kept_reminders_replaced_other_sessions_stay` | ported | src/domain/calendar.rs::move_kept_reminders_replaced_other_sessions_stay |
| `test_delete_tombstone_not_recreated_valid_replacement_found` | ported | src/domain/calendar.rs::delete_tombstone_not_recreated_valid_replacement_found |
| `test_resize_compresses_to_45_minutes_and_start_uses_snapshot` | ported | src/domain/calendar.rs::resize_compresses_to_45_minutes |
| `test_compression_uses_learned_set_duration` | ported | src/domain/calendar.rs::compression_uses_learned_set_duration |
| `test_too_short_edit_preserved_and_explained` | ported | src/domain/calendar.rs::too_short_edit_preserved_and_explained |
| `test_all_day_travel_cancels_without_compressing_missed_volume` | ported | src/domain/calendar.rs::all_day_travel_cancels_without_compressing_missed_volume |
| `test_invalidated_token_full_sync_keeps_deletion_provenance` | ported | src/domain/calendar.rs::invalidated_token_full_sync_keeps_deletion_provenance |
| `test_failed_sync_preserves_plan_and_token` | ported | src/domain/calendar.rs::failed_sync_preserves_plan_and_token |
| `test_locked_hard_conflicts_are_kept_and_explained` | ported | src/domain/calendar.rs::locked_hard_conflicts_are_kept_and_explained |
| `test_write_retry_after_remote_success_no_duplicate_events` | ported | src/domain/calendar.rs::write_retry_after_remote_success_no_duplicate_events |
| `test_patch_retry_after_remote_success_is_acknowledged` | ported | src/domain/calendar.rs::patch_retry_after_remote_success_is_acknowledged |
| `test_live_write_gate_runs_before_any_provider_call` | ported | src/domain/calendar.rs::live_write_gate_runs_before_any_provider_call |
| `test_remote_write_race_never_overwrites_manual_move` | ported | src/domain/calendar.rs::remote_write_race_never_overwrites_manual_move |

## gymclaw/tests/test_cli.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_cli_restart_plan_and_idempotency` | ported now | src/tools/plan.rs::plan_week_restart_and_idempotency |
| `test_cli_errors_json_and_rollback` | ported now | src/tools/plan.rs::errors_roll_back_and_naive_times_are_local |
| `test_cli_forgives_common_guesses` | ported now | src/tools/plan.rs::errors_roll_back_and_naive_times_are_local (naive time = owner-local; the argparse operation-guessing is gone) |
| `test_calendar_snapshot_and_locked_session_preserved` | ported now | src/domain/scheduling.rs::calendar_snapshot_and_locked_session_preserved |
| `test_request_id_input_conflict` | ported now | src/tools/plan.rs::plan_week_restart_and_idempotency |
| `test_fixture_explicit_and_persisted` | obsolete | `--fixture` busy/unavailable input to plan-week; no fixture path in the tools |

## gymclaw/tests/test_coach.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_typed_set_reacts_then_rest_card_then_fresh_set_card` | ported | src/domain/coach.rs::typed_set_then_rest_card_then_fresh_set_card |
| `test_rest_over_closes_job_once_and_returns_fresh_card` | ported | src/domain/coach.rs::rest_over_closes_job_once_and_finishing_an_exercise_rests_before_the_next |
| `test_finishing_an_exercise_rests_before_the_next` | ported | src/domain/coach.rs::rest_over_closes_job_once_and_finishing_an_exercise_rests_before_the_next |
| `test_swap_menu_on_card_wait_later_and_free` | ported | src/domain/coach.rs::swap_menu_later_free_and_remembered_swap |
| `test_swap_is_remembered_in_the_template` | ported | src/domain/coach.rs::swap_menu_later_free_and_remembered_swap; the old-button assert ported now as skip_before_any_set_on_a_swapped_card_is_an_old_button |
| `test_next_end_crowd_and_baseline_weight` | ported | src/domain/coach.rs::next_end_and_baseline_weight_then_relabel; the rating assert ported now as a_few_rating_is_stored_as_fine |
| `test_idle_workout_is_closed_and_keeps_logged_sets` | ported now | src/domain/coach.rs::idle_workout_is_closed_and_keeps_logged_sets |
| `test_session_preview_uses_current_plan_local_time_and_one_image` | ported now | src/domain/coach.rs::session_preview_uses_current_plan_local_time_and_one_image |
| `test_unknown_weight_offers_a_guess_from_the_same_movement` | ported | src/domain/coach.rs::next_end_and_baseline_weight_then_relabel |
| `test_rest_can_be_cut_by_15_or_30_seconds` | ported | src/domain/coach.rs::cut_rest_and_rest_change |
| `test_rest_change_applies_now_and_can_become_the_default` | ported | src/domain/coach.rs::cut_rest_and_rest_change |
| `test_owner_names_the_exercise_and_the_workout_follows` | ported | src/domain/coach.rs::owner_names_the_exercise_and_the_workout_follows |
| `test_relabel_fixes_what_was_logged_even_after_the_workout` | ported | src/domain/coach.rs::next_end_and_baseline_weight_then_relabel |
| `test_start_button_bodyweight_sets_and_no_blind_warmup` | ported | src/domain/coach.rs::bodyweight_reps_alone_and_no_rest_after_the_last_set; the start-button half ported now as start_button_starts_once_without_a_blind_warmup |
| `test_swapped_away_exercise_comes_back_first` | ported | src/domain/coach.rs::swap_menu_later_free_and_remembered_swap |
| `test_only_button_starts_count_as_gym_arrivals_for_habits` | ported now | src/domain/coach.rs::only_button_starts_count_as_gym_arrivals_for_habits |

## gymclaw/tests/test_crowd.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_private_gym_config_without_hardcoded_fallback` | covered elsewhere | src/providers/crowd.rs::config_shapes (both shapes resolve; no env fallback exists in from_environment). The file lookup itself isn't tested: it needs a process-wide env var |
| `test_invalid_private_gym_config_withholds_values` | covered elsewhere | src/providers/crowd.rs::config_shapes (the error is a fixed string, so values can't leak) |
| `test_http_source_reads_count` | ported | src/providers/crowd.rs::http_source_extracts_and_identity_matches_python |
| `test_http_identity_covers_headers` | ported now | src/providers/crowd.rs::http_identity_covers_headers_not_key_order |
| `test_existing_mysports_history_keeps_its_binding` | covered elsewhere | legacy identity: src/providers/crowd.rs::config_shapes; binding: src/domain/crowd.rs::fixture_source_cannot_replace_live_studio |
| `test_command_source_reads_count` | ported | src/providers/crowd.rs::command_source_reads_count |
| `test_invalid_source_config_is_not_configured` | ported | src/providers/crowd.rs::config_shapes |
| `test_mysports_observed_public_contract_no_session_cookie` | ported | src/providers/crowd.rs::mysports_accepts_only_whole_nonnegative_counts |
| `test_invalid_count_never_stored` | ported | src/providers/crowd.rs::mysports_accepts_only_whole_nonnegative_counts |
| `test_unknown_freshness_change_window_and_restart` | ported | src/domain/crowd.rs::unknown_freshness_change_window_and_restart |
| `test_unavailable_source_keeps_history_and_health` | ported | src/domain/crowd.rs::unavailable_source_keeps_history_and_health |
| `test_feedback_uses_only_prearrival_signals_and_is_idempotent` | ported | src/domain/crowd.rs::feedback_uses_only_prearrival_signals_and_is_idempotent |
| `test_old_signal_not_paired_and_future_labels_do_not_leak` | ported | src/domain/crowd.rs::old_signal_not_paired_and_future_labels_do_not_leak |
| `test_discomfort_evidence_from_labels` | ported | src/domain/crowd.rs::discomfort_evidence_from_labels |
| `test_calibration_monotonic_after_ten_labels_not_capacity_conversion` | ported | src/domain/crowd.rs::calibration_monotonic_after_ten_labels_not_capacity_conversion |
| `test_many_polls_do_not_inflate_independent_evidence_days` | ported | src/domain/crowd.rs::many_polls_do_not_inflate_independent_evidence_days |
| `test_calendar_planning_uses_arrival_labels` | ported | src/domain/crowd.rs::calendar_planning_uses_arrival_labels |
| `test_fixture_source_cannot_replace_live_studio` | ported | src/domain/crowd.rs::fixture_source_cannot_replace_live_studio |
| `test_crowd_cli_fixture_feedback_and_health` | obsolete | `crowd poll --fixture` CLI; health is covered by src/domain/crowd.rs::unavailable_source_keeps_history_and_health |
| `test_units_never_mix_counts_with_percentages` | obsolete | CrowdReading only holds a whole count by type; no percentage field exists |
| `test_polling_alert_once_when_stale_and_once_when_back` | ported | src/domain/crowd.rs::polling_alert_once_when_stale_and_once_when_back |

## gymclaw/tests/test_dashboard.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_demo_uses_real_domain_with_explicit_isolation` | obsolete | the demo snapshot mode is not part of the Rust dashboard (live read-only DB only) |
| `test_demo_http_serves_only_public_assets_and_rejects_writes` | ported now | src/web/mod.rs::serves_state_only_to_allowed_hosts_and_only_public_assets (asset, CSP, 404 and 405 checks on the live handler) |
| `test_db_live_mode_serves_state_only_to_allowed_hosts` | ported now | src/web/mod.rs::serves_state_only_to_allowed_hosts_and_only_public_assets |
| `test_one_rep_max_makes_different_rep_counts_comparable` | ported now | src/web/state.rs::one_rep_max_makes_different_rep_counts_comparable (40×12 was already in python_number_formats) |

## gymclaw/tests/test_db.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_migrations_and_restart` | ported now | src/db/mod.rs::migrations_and_restart (SQLAlchemy compare_metadata has no Rust equivalent; baseline.sql is the schema) |
| `test_foreign_keys_and_naive_timestamps` | ported now | src/db/mod.rs::foreign_keys_are_enforced; the naive-timestamp half is impossible in Rust (Ts is always an instant) |

## gymclaw/tests/test_illustrations.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_catalog_assets_and_no_fuzzy_matching` | ported now | src/domain/illustrations.rs::catalog_assets_and_no_fuzzy_matching |
| `test_pose_follows_active_exercise_not_previous_set` | ported now | src/domain/workout.rs::pose_follows_active_exercise_not_previous_set |
| `test_swap_alternatives_differ_in_equipment` | ported now | src/domain/illustrations.rs::swap_alternatives_differ_in_equipment |
| `test_swap_never_offers_the_same_movement_under_another_name` | ported now | src/domain/illustrations.rs::swap_never_offers_the_same_movement_under_another_name |
| `test_swap_offers_the_same_movement_on_other_equipment_first` | ported now | src/domain/illustrations.rs::swap_offers_the_same_movement_on_other_equipment_first |

## gymclaw/tests/test_managed_runtime.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_cli_startup_logs_do_not_hide_final_json` | obsolete | OpenClaw CLI output parsing / managed config |
| `test_cli_json_rejects_partial_or_unstructured_output` | obsolete | OpenClaw CLI output parsing / managed config |
| `test_cli_pairing_failure_is_actionable_without_child_output` | obsolete | OpenClaw CLI output parsing / managed config |
| `test_cli_failure_keeps_error_class_without_child_output` | obsolete | OpenClaw CLI output parsing / managed config |
| `test_legacy_cron_command_and_managed_config_path` | obsolete | OpenClaw CLI output parsing / managed config |
| `test_managed_callback_retains_config_without_recreation` | obsolete | OpenClaw CLI output parsing / managed config |

## gymclaw/tests/test_onboarding.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_interview_is_mandatory_and_defaults_never_count` | ported | src/domain/onboarding.rs::interview_plan_review_finish; the missing-count and finish-too-soon asserts ported now in interview_is_mandatory_idempotent_and_finish_grants_no_authority |
| `test_plan_review_finish_grants_no_authority_and_stays_ready` | ported now | src/domain/onboarding.rs::interview_is_mandatory_idempotent_and_finish_grants_no_authority |
| `test_idempotency_and_demo_rejection` | ported now | src/domain/onboarding.rs::interview_is_mandatory_idempotent_and_finish_grants_no_authority (demo rejection already in interview_plan_review_finish) |
| `test_onboarding_cli_envelope` | covered elsewhere | argparse envelope gone; the first-question instruction is asserted in src/domain/onboarding.rs::interview_is_mandatory_idempotent_and_finish_grants_no_authority |

## gymclaw/tests/test_personal_calendar.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_feed_expands_recurrence_and_skips_free_cancelled` | ported | src/providers/ics.rs::matches_python_expansion (basic.ics, first window) |
| `test_floating_and_all_day_events_at_utc_window_boundary` | ported | src/providers/ics.rs::matches_python_expansion (basic.ics, 22:00–23:00 UTC window) |
| `test_webcal_link_normalized_and_plain_http_rejected` | ported | src/providers/ics.rs::webcal_link_normalized_and_plain_http_rejected |
| `test_personal_blocker_moves_session_and_outage_keeps_blockers` | ported | src/domain/personal_calendar.rs::personal_blocker_moves_session_and_outage_keeps_blockers |

## gymclaw/tests/test_planning.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_target_recovery_and_determinism` | ported | src/domain/planning.rs::target_with_rest_days; determinism and buffers ported now as determinism_and_travel_buffers |
| `test_travel_entire_week` | ported now | src/domain/planning.rs::travel_entire_week |
| `test_full_block_rejects_prep_and_home_overlap` | ported | src/domain/planning.rs::prep_and_travel_must_clear_blockers_but_may_touch |
| `test_touching_boundaries_allowed` | ported | src/domain/planning.rs::prep_and_travel_must_clear_blockers_but_may_touch |
| `test_cross_week_recovery_and_weekly_max` | ported | src/domain/planning.rs::recovery_crosses_weeks; the weekly-max half ported now as weekly_max_counts_existing_sessions |
| `test_global_search_beats_greedy_high_scoring_tuesday` | ported | src/domain/planning.rs::session_count_beats_one_great_slot |
| `test_score_selects_quiet_slot` | ported | src/domain/planning.rs::quiet_slot_wins |
| `test_shortened_workout_when_only_minimum_fits` | ported | src/domain/planning.rs::shortened_workout_when_only_minimum_fits |
| `test_no_past_preparation` | ported now | src/domain/planning.rs::no_past_preparation |
| `test_invalid_profile` | ported now | src/domain/planning.rs::invalid_profile_and_week (all 8 cases) |
| `test_invalid_inputs` | ported now | src/domain/planning.rs::invalid_profile_and_week (Monday check); naive datetimes and zero-length Interval validation are impossible/absent by type |
| `test_dst_elapsed_duration` | ported | src/domain/planning.rs::dst_durations_are_elapsed_time |
| `test_habit_breaks_ties_but_a_quiet_slot_beats_a_busy_habitual_one` | ported | src/domain/planning.rs::habit_breaks_ties_but_quiet_beats_habitual |

## gymclaw/tests/test_progression.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_double_progression` | ported now | src/domain/progression.rs::double_progression_needs_every_set_at_target_and_rep_max (all 5 parametrized cases) |
| `test_decimal_increment` | ported now | src/domain/progression.rs::decimal_increment |

## gymclaw/tests/test_runtime.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_scheduler_adopts_creation_after_lost_response_and_restart` | obsolete | OpenClaw remote timer adoption; rest timers now fire in-process |
| `test_early_set_cancels_remote_timer_without_touching_foreign_jobs` | obsolete | OpenClaw remote timers; the cancelled rest job itself is covered by src/domain/workout.rs::warmup_rest_and_early_set_cancel |
| `test_rest_output_survives_restart_and_delivery_retries_do_not_duplicate` | ported now | src/runtime/outbox.rs::rest_output_survives_restart_and_delivery_retries_do_not_duplicate |
| `test_lost_send_response_fails_closed_until_operator_resolution` | ported | src/runtime/outbox.rs::failed_send_is_unknown_and_never_resent |
| `test_crash_after_claim_never_auto_resends` | ported now | src/runtime/outbox.rs::crash_after_claim_never_auto_resends |
| `test_new_set_suppresses_stale_pending_delivery` | ported now | src/runtime/outbox.rs::new_set_suppresses_stale_pending_delivery |
| `test_moved_plan_suppresses_old_preparation_delivery` | ported now | src/runtime/outbox.rs::moved_plan_suppresses_old_preparation_delivery |
| `test_session_start_reminder_carries_a_start_button` | ported now | src/runtime/outbox.rs::session_start_reminder_carries_a_start_button |
| `test_approval_gates_run_before_external_calls` | covered elsewhere | --allow-messages/--allow-runtime-changes flags are gone; the remaining gate is src/runtime/outbox.rs::paused_runtime_sends_nothing |
| `test_single_owner_dedicated_runtime_required` | obsolete | validates OpenClaw profile names and route strings; Rust sends to one numeric Telegram id |
| `test_provider_exact_argv_and_verified_receipt` | obsolete | OpenClaw CLI argv |
| `test_adapter_hides_child_secrets_on_failure` | obsolete | OpenClaw subprocess adapter |
| `test_runtime_cli_offline_plan_and_fire` | obsolete | `runtime plan|fire` CLI for OpenClaw callbacks |

## gymclaw/tests/test_templates.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_template_roundtrip_and_validation` | ported now | src/domain/templates.rs::template_roundtrip_and_validation |
| `test_calendar_upgrade_preserves_existing_rest_jobs` | obsolete | upgrades from an old alembic revision; Rust starts from the alembic head baseline and refuses any other revision (SCHEMA_MISMATCH in src/db/mod.rs::migrate) |
| `test_upgrade_preserves_existing_workout_and_logs` | obsolete | same: the alembic upgrade chain is gone; prod is already at head |
| `test_edit_one_exercise_without_reimport` | ported now | src/domain/templates.rs::edit_one_exercise_without_reimport |

## gymclaw/tests/test_warm.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_warm_worker_matches_direct_calls` | obsolete | warm worker socket |

## gymclaw/tests/test_weekly.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_weekly_plan_retry_one_briefing_and_no_duplicate_calendar_intent` | ported | src/domain/weekly.rs::weekly_plan_retry_one_briefing_and_no_duplicate_calendar_intent |
| `test_briefing_hides_uncertain_crowd_and_lists_new_targets` | ported | src/domain/weekly.rs::briefing_hides_uncertain_crowd_and_lists_new_targets |
| `test_rolling_watch_marks_elapsed_unstarted_session_missed_not_completed` | ported | src/domain/weekly.rs::rolling_watch_marks_elapsed_unstarted_session_missed_not_completed |
| `test_runtime_weekly_and_crowd_jobs_persist_selected_template` | obsolete | OpenClaw cron job sync; the in-process schedule is src/runtime/scheduler.rs::weekly_waits_for_sunday_evening |
| `test_separate_calendar_write_authority_and_pause_cannot_be_undone_by_callback` | ported now | src/tools/ops.rs::separate_calendar_write_authority_and_resume_needs_the_owner (the OpenClaw callback part is gone) |
| `test_relocate_rebinds_paused_runtime_to_new_deployment` | obsolete | runtime relocate rebinds OpenClaw jobs to a project root; one binary has no project root |
| `test_generic_outbox_dedup_and_pending_calendar_ack_superseded` | ported | src/runtime/outbox.rs::newer_briefing_supersedes_pending_one_and_old_messages_expire; send-then-ack ported now as src/runtime/outbox.rs::sent_event_message_marks_its_event_handled |
| `test_weekly_cli_fixture_preview_never_activates_or_queues_send` | obsolete | fixture preview of the `runtime weekly` CLI; no such CLI or fixture mode |
| `test_calendar_resize_watch_proactively_sends_once_without_publication` | ported | src/domain/ticks.rs::calendar_resize_watch_sends_once_without_publication |
| `test_start_without_template_starts_todays_session_even_when_late` | ported | src/domain/weekly.rs::start_without_template_starts_todays_session_even_when_late |
| `test_watch_survives_lost_calendar_auth_and_alerts_once` | ported | src/domain/ticks.rs::watch_survives_lost_calendar_auth_and_alerts_once |
| `test_new_arrival_label_reconsiders_tentative_slot_but_not_user_lock` | ported | src/domain/weekly.rs::new_arrival_label_reconsiders_tentative_slot_but_not_user_lock |
| `test_split_rotates_after_last_started_workout_and_respects_locks` | ported | src/domain/weekly.rs::split_rotates_after_last_started_workout_and_respects_locks |
| `test_crowd_forecast_text_and_significant_changes` | ported | src/domain/weekly.rs::crowd_forecast_text_and_significant_changes |
| `test_refresh_crowd_fills_unknown_then_respects_window` | ported | src/domain/weekly.rs::refresh_crowd_fills_unknown_then_respects_window |
| `test_refresh_plans_replaces_stale_copy_once` | ported | src/domain/weekly.rs::refresh_plans_replaces_stale_copy_once |
| `test_event_body_has_plan_location_and_split_colour` | ported | src/domain/weekly.rs::event_body_has_plan_location_and_split_colour |
| `test_no_show_nudges_once_then_skip_counts_toward_the_week` | ported | src/domain/weekly.rs::no_show_nudges_once_then_skip_counts_toward_the_week |
| `test_no_show_is_quiet_when_a_workout_started_and_can_still_be_moved` | ported | src/domain/weekly.rs::no_show_is_quiet_when_a_workout_started_and_can_still_be_moved |

## gymclaw/tests/test_workout_cli.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_cli_full_fake_workout_and_restart` | covered elsewhere | the old workout CLI flow; its behaviors are in src/domain/adaptation.rs::reorder_retry_deferred_and_audit, src/runtime/outbox.rs::rest_output_survives_restart_and_delivery_retries_do_not_duplicate and src/tools/plan.rs::planner_events_stay_out_of_the_inbox_until_asked |
| `test_cli_workout_errors_are_json` | obsolete | argparse argument combinations (--text with --weight, missing --request-id); the typed Workout tool rejects these at deserialization |
| `test_log_past_workout_counts_for_plan_and_progression` | ported | src/domain/backfill.rs::past_workout_counts_for_progression_without_rewriting_the_plan |

## gymclaw/tests/test_workout_state.py

| Python test | Category | Rust / reason |
|---|---|---|
| `test_persistent_state_warmup_and_early_set_cancel` | ported | src/domain/workout.rs::warmup_rest_and_early_set_cancel |
| `test_rest_due_exactly_and_retry` | ported | src/domain/workout.rs::rest_is_due_exactly; the retry half ported now as src/domain/workout.rs::rest_complete_retry_returns_the_first_result |
| `test_guardrails_rollback_and_id_conflict` | ported now | src/domain/workout.rs::guardrails_rollback_and_id_conflict |
| `test_set_parser` | ported now | src/domain/set_parser.rs::either_order_with_units_and_separators |
| `test_set_parser_uses_expected_weight_when_order_is_unclear` | ported now | src/domain/set_parser.rs::uses_expected_weight_when_order_is_unclear |
| `test_bodyweight_set_is_reps_alone` | ported now | src/domain/set_parser.rs::bodyweight_set_is_reps_alone |
| `test_working_set_skips_pending_warmup` | ported | src/domain/workout.rs::working_set_skips_pending_warmup_and_time_cannot_reverse |
| `test_ambiguous_set_rejected` | ported now | src/domain/set_parser.rs::ambiguous_set_rejected |
| `test_template_edits_do_not_mutate_active_workout` | ported now | src/domain/workout.rs::template_edits_do_not_mutate_active_workout |
| `test_eta_learns_from_completed_set_timing` | ported | src/domain/workout.rs::eta_learns_from_completed_set_timing |
| `test_only_first_primary_gets_warmup` | ported now | src/domain/workout.rs::only_first_primary_gets_warmup |

## Not ported on purpose

Nothing in this list is trivial filler: every live behavior has a Rust test. Python-only checks left out: SQLAlchemy `compare_metadata`, pydantic model validation of naive datetimes and empty intervals, and argparse error shapes.

## Behavior differences found while porting

None remaining. `profile_update` briefly stopped replanning in Rust; it replans again, like Python.
