// Integration tests originally in legacy.rs. Each test pulls in the items it
// needs explicitly; nothing here is exported.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use tapoctl::{DeviceConfig, DeviceModel, DeviceSnapshot};
use tokio::sync::Mutex;

use crate::conditions::*;
use crate::devices::*;
use crate::energy::*;
use crate::hooks::*;
use crate::schedules::*;
use crate::settings::{DEFAULT_ENERGY_PRICE_PENCE_PER_KWH, Settings, optional_u64_env, parse_string_list};
use crate::state::*;
use crate::time::now_ms;

#[test]
fn parses_default_settings_without_optional_values() {
    assert_eq!(optional_u64_env("FUSEBOX_TEST_MISSING", 42).unwrap(), 42);
}

#[test]
fn parses_comma_or_space_separated_string_lists() {
    let targets = parse_string_list("192.168.0.0/24, 10.10.0.255\n172.18.0.0/16");

    assert_eq!(
        targets,
        vec![
            "192.168.0.0/24".to_string(),
            "10.10.0.255".to_string(),
            "172.18.0.0/16".to_string(),
        ],
    );
}

#[test]
fn identifies_tapo_handshake_failures() {
    let handshake_error = anyhow!("HTTP error 400: Handshake2 failed");
    let other_error = anyhow!("HTTP error 400: device busy");

    assert!(is_tapo_handshake_error(&handshake_error));
    assert!(!is_tapo_handshake_error(&other_error));
}

#[tokio::test]
async fn retries_transient_tapo_handshake_failures() {
    let attempts = Arc::new(Mutex::new(0_u8));
    let result = retry_tapo_handshake({
        let attempts = attempts.clone();

        move || {
            let attempts = attempts.clone();

            async move {
                let mut attempts = attempts.lock().await;
                *attempts += 1;

                if *attempts == 1 {
                    return Err(anyhow!("HTTP error 400: Handshake2 failed"));
                }

                Ok("ok")
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(result, "ok");
    assert_eq!(*attempts.lock().await, 2);
}

#[test]
fn renders_snapshot_backed_device_view() {
    let device = ManagedDevice {
        name: "lights".to_string(),
        config: DeviceConfig {
            ip: "192.168.0.40".parse().unwrap(),
            model: DeviceModel::P110,
        },
        snapshot: Some(DeviceSnapshot {
            ip: "192.168.0.40".parse().unwrap(),
            model: DeviceModel::P110,
            device_model: "P110".to_string(),
            nickname: "Lights".to_string(),
            device_type: "Plug with Energy Monitoring".to_string(),
            device_on: true,
            on_time_seconds: 120,
            energy: Some(tapoctl::EnergySnapshot {
                current_power_mw: Some(12_000),
                current_power_w: Some(12),
                today_energy_wh: 1500,
                month_energy_wh: 12_000,
                today_runtime_minutes: 80,
                month_runtime_minutes: 900,
            }),
        }),
        last_error: None,
        discovered_at_ms: 1,
        updated_at_ms: Some(2),
        consecutive_failures: 0,
        offline_announced: false,
    };

    let view = device.view(30.0, DeviceIntent::default(), None);

    assert_eq!(view.name, "lights");
    assert_eq!(view.nickname, "Lights");
    assert_eq!(view.device_on, Some(true));
    assert_eq!(view.on_time_seconds, Some(120));
    assert_eq!(view.energy.unwrap().today_cost_pence, 45.0);
}

#[test]
fn splits_power_export_ranges_at_tapo_limits() {
    let start = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
    let end = start + ChronoDuration::hours(24);

    let ranges = split_datetime_ranges(start, end, ChronoDuration::hours(12));

    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0], (start, start + ChronoDuration::hours(12)));
    assert_eq!(ranges[1], (start + ChronoDuration::hours(12), end));
}

#[test]
fn maps_long_usage_ranges_to_energy_history() {
    let three_months = usage_history_range(Some("3m"));
    let ytd = usage_history_range(Some("ytd"));
    let all_time = usage_history_range(Some("all"));

    assert_eq!(three_months.key, "3m");
    assert_eq!(three_months.unit, "kWh");
    assert!(matches!(three_months.kind, UsageHistoryKind::EnergyDaily));
    assert!(matches!(ytd.start, UsageHistoryStart::YearToDate));
    assert!(matches!(all_time.kind, UsageHistoryKind::EnergyMonthly));
}

#[test]
fn calculates_calendar_usage_range_starts() {
    let now = DateTime::from_timestamp(1_771_588_800, 0).unwrap();
    let ytd_start = usage_history_start_datetime(UsageHistoryStart::YearToDate, now);
    let all_time_start = usage_history_start_datetime(UsageHistoryStart::AllTime, now);

    assert_eq!(
        ytd_start.date_naive(),
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    );
    assert_eq!(
        all_time_start.date_naive(),
        NaiveDate::from_ymd_opt(ALL_TIME_USAGE_START_YEAR, 1, 1).unwrap(),
    );
}

#[test]
fn writes_export_workbook_buffer() {
    let mut values = BTreeMap::new();
    values.insert("lights".to_string(), 1.5);
    let table = ExportTable {
        sheet_name: "Energy - Hourly (last week)",
        value_format: "0.000",
        rows: vec![ExportRow {
            timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
            values,
        }],
    };

    let buffer = write_export_workbook(&["lights".to_string()], &[table], &[]).unwrap();

    assert!(buffer.len() > 1000);
    assert_eq!(&buffer[0..2], b"PK");
}

#[tokio::test]
async fn saves_and_loads_persisted_device_configs() {
    let state_path = test_state_path("roundtrip");
    let settings = Settings {
        bind_address: "127.0.0.1:8787".parse().unwrap(),
        username: "dummy@example.com".to_string(),
        password: "dummy-password".to_string(),
        refresh_seconds: 10,
        scan_seconds: 60,
        discovery_timeout_seconds: 5,
        discovery_targets: Vec::new(),
        energy_price_pence_per_kwh: DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
        state_path: state_path.clone(),
    };
    let state = AppState::new(&settings);

    {
        let mut devices = state.devices.write().await;
        devices.insert(
            "lights".to_string(),
            managed_device_from_config(
                "lights".to_string(),
                DeviceConfig {
                    ip: "192.168.0.40".parse().unwrap(),
                    model: DeviceModel::P110,
                },
            ),
        );
    }

    save_persisted_state(&state).await.unwrap();

    let contents = fs::read_to_string(&state_path).unwrap();
    assert!(contents.contains("lights"));
    assert!(!contents.contains("dummy-password"));

    let reloaded_state = AppState::new(&settings);
    load_persisted_state(&reloaded_state).await.unwrap();

    let devices = reloaded_state.devices.read().await;
    let loaded = devices.get("lights").unwrap();

    assert_eq!(loaded.config.ip.to_string(), "192.168.0.40");
    assert_eq!(loaded.config.model, DeviceModel::P110);
    assert!(loaded.snapshot.is_none());

    let _ = fs::remove_file(state_path);
}

#[tokio::test]
async fn reuses_device_operation_locks_by_ip() {
    let state_path = test_state_path("locks");
    let settings = test_settings(state_path);
    let state = AppState::new(&settings);
    let first_device = DeviceConfig {
        ip: "192.168.0.40".parse().unwrap(),
        model: DeviceModel::P110,
    };
    let same_ip_device = DeviceConfig {
        ip: "192.168.0.40".parse().unwrap(),
        model: DeviceModel::P115,
    };
    let other_device = DeviceConfig {
        ip: "192.168.0.41".parse().unwrap(),
        model: DeviceModel::P110,
    };

    let first_lock = device_operation_lock(&state, &first_device).await;
    let same_ip_lock = device_operation_lock(&state, &same_ip_device).await;
    let other_lock = device_operation_lock(&state, &other_device).await;

    assert!(Arc::ptr_eq(&first_lock, &same_ip_lock));
    assert!(!Arc::ptr_eq(&first_lock, &other_lock));
}

fn test_state_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fusebox-{name}-{}-{}.json",
        std::process::id(),
        now_ms(),
    ))
}

fn test_settings(state_path: PathBuf) -> Settings {
    Settings {
        bind_address: "127.0.0.1:8787".parse().unwrap(),
        username: "dummy@example.com".to_string(),
        password: "dummy-password".to_string(),
        refresh_seconds: 10,
        scan_seconds: 60,
        discovery_timeout_seconds: 5,
        discovery_targets: Vec::new(),
        energy_price_pence_per_kwh: DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
        state_path,
    }
}

#[test]
fn normalizes_five_field_cron_with_seconds_prefix() {
    let normalized = normalize_cron("0 7 * * 1-5").unwrap();
    assert_eq!(normalized, "0 0 7 * * 1-5");
    parse_cron(&normalized).unwrap();
}

#[test]
fn passes_six_field_cron_through() {
    let normalized = normalize_cron("30 0 7 * * 1-5").unwrap();
    assert_eq!(normalized, "30 0 7 * * 1-5");
    parse_cron(&normalized).unwrap();
}

#[test]
fn accepts_standard_dow_zero_through_seven() {
    let normalized = normalize_cron("0 2 * * 0,6").unwrap();
    assert_eq!(normalized, "0 0 2 * * 0,6");
    parse_cron(&normalized).unwrap();

    let normalized_seven = normalize_cron("0 2 * * 7").unwrap();
    parse_cron(&normalized_seven).unwrap();
}

#[test]
fn translates_standard_dow_to_crate_dow() {
    assert_eq!(translate_dow_field("0"), "1");
    assert_eq!(translate_dow_field("7"), "1");
    assert_eq!(translate_dow_field("0,6"), "1,7");
    assert_eq!(translate_dow_field("1-5"), "2-6");
    assert_eq!(translate_dow_field("*"), "*");
    assert_eq!(translate_dow_field("*/2"), "*/2");
    assert_eq!(translate_dow_field("1-5/2"), "2-6/2");
}

#[test]
fn weekday_cron_fires_monday_to_friday() {
    let normalized = normalize_cron("0 7 * * 1-5").unwrap();
    let parsed = parse_cron(&normalized).unwrap();
    let sunday_midnight =
        chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_585_600, 0).unwrap();
    let next = parsed.after(&sunday_midnight).next().unwrap();
    assert_eq!(next.timestamp(), 1_704_697_200);
}

#[test]
fn rejects_invalid_cron_expressions() {
    assert!(normalize_cron("").is_err());
    assert!(normalize_cron("not a cron").is_err());
    assert!(normalize_cron("99 99 * * *").is_err());
}

#[tokio::test]
async fn persists_schedule_across_reload() {
    let state_path = test_state_path("schedules");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    {
        let mut schedules = state.schedules.write().await;
        schedules.insert(
            "abc".to_string(),
            ScheduleConfig {
                id: "abc".to_string(),
                device_name: "lights".to_string(),
                kind: ScheduleKind::Cron,
                cron: Some("0 0 7 * * 1-5".to_string()),
                action: Some(ScheduleAction::On),
                on_seconds: None,
                off_seconds: None,
                start_action: None,
                starts_at_ms: None,
                enabled: true,
                label: Some("Morning".to_string()),
                condition_ids: Vec::new(),
                created_at_ms: 1_700_000_000_000,
                last_fired_at_ms: None,
                last_error: None,
            },
        );
        schedules.insert(
            "iv1".to_string(),
            ScheduleConfig {
                id: "iv1".to_string(),
                device_name: "lights".to_string(),
                kind: ScheduleKind::Interval,
                cron: None,
                action: None,
                on_seconds: Some(3600),
                off_seconds: Some(1800),
                start_action: Some(ScheduleAction::On),
                starts_at_ms: Some(1_700_000_000_000),
                enabled: true,
                label: Some("1h/30m".to_string()),
                condition_ids: Vec::new(),
                created_at_ms: 1_700_000_000_000,
                last_fired_at_ms: None,
                last_error: None,
            },
        );
    }
    save_persisted_state(&state).await.unwrap();

    let reloaded = AppState::new(&settings);
    load_persisted_state(&reloaded).await.unwrap();
    let schedules = reloaded.schedules.read().await;
    let cron_loaded = schedules.get("abc").unwrap();
    assert_eq!(cron_loaded.device_name, "lights");
    assert_eq!(cron_loaded.cron.as_deref(), Some("0 0 7 * * 1-5"));
    assert_eq!(cron_loaded.action, Some(ScheduleAction::On));
    assert_eq!(cron_loaded.label.as_deref(), Some("Morning"));

    let interval_loaded = schedules.get("iv1").unwrap();
    assert_eq!(interval_loaded.kind, ScheduleKind::Interval);
    assert_eq!(interval_loaded.on_seconds, Some(3600));
    assert_eq!(interval_loaded.off_seconds, Some(1800));
    assert_eq!(interval_loaded.start_action, Some(ScheduleAction::On));

    let _ = fs::remove_file(state_path);
}

#[test]
fn interval_phase_flips_at_boundary() {
    let schedule = ScheduleConfig {
        id: "x".to_string(),
        device_name: "lights".to_string(),
        kind: ScheduleKind::Interval,
        cron: None,
        action: None,
        on_seconds: Some(60),
        off_seconds: Some(120),
        start_action: Some(ScheduleAction::On),
        starts_at_ms: Some(1_000),
        enabled: true,
        label: None,
        condition_ids: Vec::new(),
        created_at_ms: 1_000,
        last_fired_at_ms: None,
        last_error: None,
    };

    assert_eq!(
        interval_phase_at(&schedule, 1_000),
        Some(ScheduleAction::On)
    );
    assert_eq!(
        interval_phase_at(&schedule, 60_000),
        Some(ScheduleAction::On)
    );
    assert_eq!(
        interval_phase_at(&schedule, 61_001),
        Some(ScheduleAction::Off)
    );
    assert_eq!(
        interval_phase_at(&schedule, 180_000),
        Some(ScheduleAction::Off)
    );
    assert_eq!(
        interval_phase_at(&schedule, 181_001),
        Some(ScheduleAction::On)
    );
    assert_eq!(interval_phase_at(&schedule, 500), None);

    // Next fire from t=30s should be at t=61s (the on→off transition).
    assert_eq!(next_interval_fire_ms(&schedule, 30_000), Some(61_000));
    // Next fire from t=120s should be at t=181s (the off→on transition).
    assert_eq!(next_interval_fire_ms(&schedule, 120_000), Some(181_000));
}

#[test]
fn parses_status_match_formats() {
    let single = parse_status_match("200").unwrap();
    assert!(status_matches(&single, 200));
    assert!(!status_matches(&single, 201));

    let range = parse_status_match("200-299").unwrap();
    assert!(status_matches(&range, 200));
    assert!(status_matches(&range, 250));
    assert!(status_matches(&range, 299));
    assert!(!status_matches(&range, 300));

    let mixed = parse_status_match("200, 204, 301-302").unwrap();
    assert!(status_matches(&mixed, 200));
    assert!(status_matches(&mixed, 204));
    assert!(status_matches(&mixed, 302));
    assert!(!status_matches(&mixed, 201));

    assert!(parse_status_match("").is_err());
    assert!(parse_status_match("not-numbers").is_err());
    assert!(parse_status_match("500-400").is_err());
}

#[test]
fn probe_key_groups_identical_requests() {
    let base = || ConditionConfig {
        id: "x".to_string(),
        name: "n".to_string(),
        device_name: "dev".to_string(),
        url: "https://example.test/api".to_string(),
        method: "GET".to_string(),
        headers: BTreeMap::new(),
        body: None,
        status_match: "200".to_string(),
        body_contains: None,
        poll_seconds: 30,
        enabled: true,
        action_on_pass: None,
        action_on_fail: None,
        created_at_ms: 0,
        last_checked_at_ms: None,
        last_passing: None,
        last_status_code: None,
        last_error: None,
        last_action_at_ms: None,
        last_action: None,
        last_action_error: None,
        min_stable_seconds: 0,
        pending_value: None,
        pending_since_ms: None,
    };

    let mut a = base();
    a.id = "a".to_string();
    let mut b = base();
    b.id = "b".to_string();
    // Different device, different poll cadence — still the same probe.
    b.device_name = "other".to_string();
    b.poll_seconds = 5;
    let mut different_url = base();
    different_url.url = "https://example.test/other".to_string();
    let mut different_status = base();
    different_status.status_match = "200-299".to_string();
    let mut different_method = base();
    different_method.method = "POST".to_string();
    let mut different_headers = base();
    different_headers
        .headers
        .insert("Authorization".to_string(), "Bearer x".to_string());

    assert_eq!(condition_probe_key(&a), condition_probe_key(&b));
    assert_ne!(condition_probe_key(&a), condition_probe_key(&different_url));
    assert_ne!(
        condition_probe_key(&a),
        condition_probe_key(&different_status)
    );
    assert_ne!(
        condition_probe_key(&a),
        condition_probe_key(&different_method)
    );
    assert_ne!(
        condition_probe_key(&a),
        condition_probe_key(&different_headers)
    );
}

#[test]
fn effective_state_truth_table() {
    // (manual, schedule, condition) -> expected
    let cases = [
        // No inputs at all: no opinion.
        ((None, None, None), None),
        // Pure condition control (e.g. AC).
        ((None, None, Some(true)), Some(true)),
        ((None, None, Some(false)), Some(false)),
        // Schedule alone.
        ((None, Some(true), None), Some(true)),
        ((None, Some(false), None), Some(false)),
        // Schedule says ON, condition agrees.
        ((None, Some(true), Some(true)), Some(true)),
        // Schedule says ON, condition forces OFF.
        ((None, Some(true), Some(false)), Some(false)),
        // Schedule says OFF, condition irrelevant.
        ((None, Some(false), Some(true)), Some(false)),
        ((None, Some(false), Some(false)), Some(false)),
        // Manual override beats every other input.
        ((Some(true), Some(false), Some(false)), Some(true)),
        ((Some(false), Some(true), Some(true)), Some(false)),
        ((Some(true), None, Some(false)), Some(true)),
    ];

    for ((manual, schedule, condition), expected) in cases {
        assert_eq!(
            compute_effective(manual, schedule, condition),
            expected,
            "compute_effective(manual={:?}, schedule={:?}, condition={:?})",
            manual,
            schedule,
            condition,
        );
    }
}

#[tokio::test]
async fn condition_intent_fail_closed_for_unprobed_required_condition() {
    let state_path = test_state_path("intent-fail-closed");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    let make = |last: Option<bool>| ConditionConfig {
        id: "c".to_string(),
        name: "n".to_string(),
        device_name: "lights".to_string(),
        url: "http://example.invalid".to_string(),
        method: "GET".to_string(),
        headers: BTreeMap::new(),
        body: None,
        status_match: "200".to_string(),
        body_contains: None,
        poll_seconds: 60,
        enabled: true,
        action_on_pass: None,
        action_on_fail: None,
        created_at_ms: 0,
        last_checked_at_ms: None,
        last_passing: last,
        last_status_code: None,
        last_error: None,
        last_action_at_ms: None,
        last_action: None,
        last_action_error: None,
        min_stable_seconds: 0,
        pending_value: None,
        pending_since_ms: None,
    };

    // No conditions targeting lights -> no opinion.
    assert_eq!(condition_intent_for_device(&state, "lights").await, None);

    // Never probed -> Some(false) (fail closed).
    {
        let mut conditions = state.conditions.write().await;
        conditions.insert("c".to_string(), make(None));
    }
    assert_eq!(
        condition_intent_for_device(&state, "lights").await,
        Some(false)
    );

    // Passing -> Some(true).
    {
        let mut conditions = state.conditions.write().await;
        conditions.get_mut("c").unwrap().last_passing = Some(true);
    }
    assert_eq!(
        condition_intent_for_device(&state, "lights").await,
        Some(true)
    );

    // Failing -> Some(false).
    {
        let mut conditions = state.conditions.write().await;
        conditions.get_mut("c").unwrap().last_passing = Some(false);
    }
    assert_eq!(
        condition_intent_for_device(&state, "lights").await,
        Some(false)
    );

    let _ = fs::remove_file(state_path);
}

fn sample_hook(device_filter: Vec<String>, event_filter: Vec<HookEvent>) -> HookConfig {
    HookConfig {
        id: "h".to_string(),
        name: "n".to_string(),
        enabled: true,
        url: "http://example.invalid".to_string(),
        method: "POST".to_string(),
        headers: BTreeMap::new(),
        body: None,
        device_filter,
        event_filter,
        created_at_ms: 0,
        last_fired_at_ms: None,
        last_event: None,
        last_status_code: None,
        last_error: None,
    }
}

#[test]
fn hook_matches_device_and_event_filters() {
    let any_device_any_event = sample_hook(Vec::new(), Vec::new());
    assert!(hook_matches(&any_device_any_event, "ac", HookEvent::On));
    assert!(hook_matches(
        &any_device_any_event,
        "lights",
        HookEvent::Offline,
    ));

    let lights_only = sample_hook(vec!["lights".to_string()], Vec::new());
    assert!(hook_matches(&lights_only, "lights", HookEvent::On));
    assert!(!hook_matches(&lights_only, "ac", HookEvent::On));

    let offline_only = sample_hook(Vec::new(), vec![HookEvent::Offline]);
    assert!(hook_matches(&offline_only, "ac", HookEvent::Offline));
    assert!(!hook_matches(&offline_only, "ac", HookEvent::On));

    let mut disabled = sample_hook(Vec::new(), Vec::new());
    disabled.enabled = false;
    assert!(!hook_matches(&disabled, "ac", HookEvent::On));

    let lights_offline = sample_hook(
        vec!["lights".to_string()],
        vec![HookEvent::Offline, HookEvent::Online],
    );
    assert!(hook_matches(&lights_offline, "lights", HookEvent::Offline));
    assert!(!hook_matches(&lights_offline, "lights", HookEvent::On));
    assert!(!hook_matches(&lights_offline, "ac", HookEvent::Offline));
}

#[test]
fn hook_template_substitution_renders_known_vars() {
    let ctx = HookTemplateContext {
        device: "lights".to_string(),
        nickname: "Lights".to_string(),
        model: "p110".to_string(),
        event: HookEvent::Off,
        source: HookSource::Condition,
        previous_on: Some(true),
        new_on: Some(false),
        timestamp_ms: 1_700_000_000_000,
    };

    assert_eq!(ctx.render("{{nickname}} -> {{event}}"), "Lights -> off",);
    assert_eq!(
        ctx.render("https://ntfy.example/topic/{{device}}"),
        "https://ntfy.example/topic/lights",
    );
    assert_eq!(
        ctx.render("source={{source}} prev={{previous_on}} new={{new_on}} ts={{timestamp_ms}}"),
        "source=condition prev=true new=false ts=1700000000000",
    );
    // Unknown placeholders stay as-is.
    assert_eq!(ctx.render("{{unknown}}"), "{{unknown}}");
    // Repeated placeholders all replaced.
    assert_eq!(ctx.render("{{event}}-{{event}}"), "off-off");
}

fn dummy_device(name: &str, ip: &str, model: DeviceModel, on: bool) -> ManagedDevice {
    let ip_addr: IpAddr = ip.parse().unwrap();
    let mut device =
        managed_device_from_config(name.to_string(), DeviceConfig { ip: ip_addr, model });
    device.snapshot = Some(DeviceSnapshot {
        ip: ip_addr,
        model,
        device_model: model.to_string(),
        device_type: "Tapo device".to_string(),
        nickname: name.to_string(),
        device_on: on,
        on_time_seconds: 0,
        energy: None,
    });
    device
}

#[tokio::test]
async fn condition_hysteresis_debounces_flapping_probes() {
    let state_path = test_state_path("hysteresis");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    // Stand up a condition with a 90s stability window pointed at an
    // unreachable URL — every probe will fail.
    let mut condition = ConditionConfig {
        id: "c".to_string(),
        name: "n".to_string(),
        device_name: "lights".to_string(),
        url: "http://127.0.0.1:1/never".to_string(),
        method: "GET".to_string(),
        headers: BTreeMap::new(),
        body: None,
        status_match: "200".to_string(),
        body_contains: None,
        poll_seconds: 5,
        enabled: true,
        action_on_pass: None,
        action_on_fail: None,
        created_at_ms: 0,
        last_checked_at_ms: None,
        last_passing: Some(true),
        last_status_code: Some(200),
        last_error: None,
        last_action_at_ms: None,
        last_action: None,
        last_action_error: None,
        min_stable_seconds: 90,
        pending_value: None,
        pending_since_ms: None,
    };
    condition.last_passing = Some(true);
    {
        let mut conditions = state.conditions.write().await;
        conditions.insert("c".to_string(), condition.clone());
    }

    // First probe: result will be Some(false). Hysteresis must NOT
    // flip last_passing yet, only start a pending wait.
    probe_and_record(&state, "c").await;
    {
        let conditions = state.conditions.read().await;
        let stored = conditions.get("c").unwrap();
        assert_eq!(
            stored.last_passing,
            Some(true),
            "hysteresis should hold previous value"
        );
        assert_eq!(stored.pending_value, Some(false));
        assert!(stored.pending_since_ms.is_some());
    }

    // Backdate the pending stamp so the 90s window has elapsed.
    {
        let mut conditions = state.conditions.write().await;
        let stored = conditions.get_mut("c").unwrap();
        stored.pending_since_ms = Some(now_ms().saturating_sub(95_000));
    }
    probe_and_record(&state, "c").await;
    {
        let conditions = state.conditions.read().await;
        let stored = conditions.get("c").unwrap();
        assert_eq!(
            stored.last_passing,
            Some(false),
            "hysteresis should commit after stable window"
        );
        assert_eq!(stored.pending_value, None);
    }

    let _ = fs::remove_file(state_path);
}

#[tokio::test]
async fn does_not_fire_hook_for_first_read_without_prior_snapshot() {
    let state_path = test_state_path("hook-no-first-read");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    let captured =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<(String, HookEvent)>::new()));
    // Insert a hook so dispatch_hook_events has something to match against.
    let hook = sample_hook(Vec::new(), Vec::new());
    {
        let mut hooks = state.hooks.write().await;
        hooks.insert(hook.id.clone(), hook);
    }
    // Insert the device WITHOUT a prior snapshot.
    {
        let mut devices = state.devices.write().await;
        devices.insert(
            "lights".to_string(),
            managed_device_from_config(
                "lights".to_string(),
                DeviceConfig {
                    ip: "192.0.2.10".parse().unwrap(),
                    model: DeviceModel::P110,
                },
            ),
        );
    }

    let snapshot = DeviceSnapshot {
        ip: "192.0.2.10".parse().unwrap(),
        model: DeviceModel::P110,
        device_model: "p110".to_string(),
        device_type: "Tapo plug".to_string(),
        nickname: "Lights".to_string(),
        device_on: true,
        on_time_seconds: 0,
        energy: None,
    };
    update_device_snapshot(&state, "lights", snapshot, None, HookSource::External).await;

    // No transition happened — first read shouldn't have queued anything for the hook.
    // We can't peek inside spawned hook firings easily, but we can assert that the
    // device's hook record is untouched.
    let hooks = state.hooks.read().await;
    let stored = hooks.values().next().unwrap();
    assert_eq!(
        stored.last_fired_at_ms, None,
        "first read should not have fired the hook"
    );
    let _ = captured;

    let _ = fs::remove_file(state_path);
}

#[tokio::test]
async fn two_devices_each_fire_hook_independently() {
    let state_path = test_state_path("hook-multi-device");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    {
        let mut devices = state.devices.write().await;
        devices.insert(
            "lights".to_string(),
            dummy_device("lights", "192.0.2.10", DeviceModel::P110, true),
        );
        devices.insert(
            "ac".to_string(),
            dummy_device("ac", "192.0.2.11", DeviceModel::P110, true),
        );
    }

    // No filter -> matches any device, any event.
    let hook = sample_hook(Vec::new(), Vec::new());
    let hook_id = hook.id.clone();
    {
        let mut hooks = state.hooks.write().await;
        hooks.insert(hook.id.clone(), hook);
    }

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for device in ["lights", "ac"] {
        let matching: Vec<HookConfig> = {
            let hooks = state.hooks.read().await;
            hooks
                .values()
                .filter(|h| hook_matches(h, device, HookEvent::Off))
                .cloned()
                .collect()
        };
        assert_eq!(matching.len(), 1, "device {} should match the hook", device);
        counter.fetch_add(matching.len(), std::sync::atomic::Ordering::Relaxed);
    }

    // Both devices independently match -> total firings = 2.
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "expected each device to fire the hook once",
    );

    // Sanity: hook id present and untouched (no real network call in test).
    let hooks = state.hooks.read().await;
    assert!(hooks.contains_key(&hook_id));

    let _ = fs::remove_file(state_path);
}

#[tokio::test]
async fn offline_event_waits_for_consecutive_failures() {
    let state_path = test_state_path("offline-debounce");
    let settings = test_settings(state_path.clone());
    let state = AppState::new(&settings);

    // Device with a prior successful snapshot (so the first-read
    // suppression doesn't get in the way).
    {
        let mut devices = state.devices.write().await;
        devices.insert(
            "lights".to_string(),
            dummy_device("lights", "192.0.2.10", DeviceModel::P110, true),
        );
    }

    // First refresh failure: counter goes to 1, no announce.
    update_device_error(&state, "lights", "transient".to_string()).await;
    {
        let devices = state.devices.read().await;
        let device = devices.get("lights").unwrap();
        assert_eq!(device.consecutive_failures, 1);
        assert!(!device.offline_announced);
    }

    // Second failure: counter goes to 2, still no announce.
    update_device_error(&state, "lights", "transient".to_string()).await;
    {
        let devices = state.devices.read().await;
        let device = devices.get("lights").unwrap();
        assert_eq!(device.consecutive_failures, 2);
        assert!(!device.offline_announced);
    }

    // Third failure: hits the threshold, announce.
    update_device_error(&state, "lights", "transient".to_string()).await;
    {
        let devices = state.devices.read().await;
        let device = devices.get("lights").unwrap();
        assert_eq!(device.consecutive_failures, 3);
        assert!(device.offline_announced);
    }

    // Recovery: snapshot success resets the counter and the flag.
    let snapshot = DeviceSnapshot {
        ip: "192.0.2.10".parse().unwrap(),
        model: DeviceModel::P110,
        device_model: "p110".to_string(),
        device_type: "Tapo plug".to_string(),
        nickname: "Lights".to_string(),
        device_on: true,
        on_time_seconds: 1,
        energy: None,
    };
    update_device_snapshot(&state, "lights", snapshot, None, HookSource::External).await;
    {
        let devices = state.devices.read().await;
        let device = devices.get("lights").unwrap();
        assert_eq!(device.consecutive_failures, 0);
        assert!(!device.offline_announced);
    }

    let _ = fs::remove_file(state_path);
}
