mod api_error;
mod automations;
mod conditions;
mod devices;
mod energy;
mod hooks;
#[cfg(test)]
mod legacy_tests;
mod migration;
mod schedules;
mod settings;
mod state;
mod time;
mod web;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{delete, get, post};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::automations::api::{
    create_automation, delete_automation, export_automation, import_automation, list_automations,
    update_automation,
};
use crate::automations::engine::run_automation_engine;
use crate::conditions::{
    create_condition, delete_condition, list_conditions, probe_condition, run_condition_poller,
    update_condition,
};
use crate::devices::{
    devices_websocket, initial_refresh_devices, list_devices, monitor_devices,
    publish_device_list, release_device_override, run_override_expiry_sweeper, scan_devices,
    scan_for_devices, set_device_power, toggle_device,
};
use crate::energy::{energy_history, export_energy_workbook};
use crate::hooks::{create_hook, delete_hook, list_hooks, test_hook, update_hook};
use crate::schedules::{
    create_schedule, delete_schedule, list_schedules, run_scheduler, update_schedule,
};
use crate::settings::Settings;
use crate::state::{AppState, load_persisted_state};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let settings = Settings::from_env()?;
    let state = AppState::new(&settings);

    if let Err(error) = load_persisted_state(&state).await {
        warn!(%error, path = %state.state_path.display(), "failed to load persisted state");
    }
    publish_device_list(&state, None).await;

    tokio::spawn(initial_refresh_devices(state.clone()));
    tokio::spawn(monitor_devices(state.clone()));
    tokio::spawn(scan_for_devices(state.clone()));
    tokio::spawn(run_scheduler(state.clone()));
    tokio::spawn(run_condition_poller(state.clone()));
    tokio::spawn(run_automation_engine(state.clone()));
    tokio::spawn(run_override_expiry_sweeper(state.clone()));

    let app = Router::new()
        .route("/", get(crate::web::index))
        .route("/favicon.ico", get(crate::web::favicon))
        .route("/assets/switch.wav", get(crate::web::switch_sound))
        .route("/assets/app.js", get(crate::web::app_bundle))
        .route("/health", get(crate::web::health))
        .route("/api/devices", get(list_devices))
        .route("/api/energy/history.json", get(energy_history))
        .route("/api/energy/export.xlsx", get(export_energy_workbook))
        .route("/ws/devices", get(devices_websocket))
        .route("/api/scan", post(scan_devices))
        .route("/api/devices/{name}/toggle", post(toggle_device))
        .route("/api/devices/{name}/power", post(set_device_power))
        .route(
            "/api/devices/{name}/release-override",
            post(release_device_override),
        )
        .route("/api/schedules", get(list_schedules).post(create_schedule))
        .route(
            "/api/schedules/{id}",
            delete(delete_schedule).patch(update_schedule),
        )
        .route(
            "/api/conditions",
            get(list_conditions).post(create_condition),
        )
        .route(
            "/api/conditions/{id}",
            delete(delete_condition).patch(update_condition),
        )
        .route("/api/conditions/{id}/probe", post(probe_condition))
        .route("/api/hooks", get(list_hooks).post(create_hook))
        .route("/api/hooks/{id}", delete(delete_hook).patch(update_hook))
        .route("/api/hooks/{id}/test", post(test_hook))
        .route(
            "/api/automations",
            get(list_automations).post(create_automation),
        )
        .route("/api/automations/import", post(import_automation))
        .route("/api/automations/{id}/export", get(export_automation))
        .route(
            "/api/automations/{id}",
            delete(delete_automation).patch(update_automation),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(settings.bind_address)
        .await
        .with_context(|| format!("failed to bind Fusebox to {}", settings.bind_address))?;

    info!("Fusebox listening on http://{}", settings.bind_address);
    axum::serve(listener, app).await?;

    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
