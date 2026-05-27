use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

use crate::conditions::condition_intent_for_device;
use crate::hooks::{HookEvent, HookSource, dispatch_hook_events};
use crate::legacy::{
    device_operation_lock, get_device_config, publish_device_list, retry_tapo_handshake,
    update_device_snapshot,
};
use crate::state::{
    AppState, MAX_MANUAL_OVERRIDE_SECONDS, MIN_MANUAL_OVERRIDE_SECONDS, save_persisted_state,
};
use crate::time::now_ms;

pub(crate) fn compute_effective(
    manual_override: Option<bool>,
    schedule_intent: Option<bool>,
    condition_intent: Option<bool>,
) -> Option<bool> {
    if let Some(manual) = manual_override {
        return Some(manual);
    }
    if condition_intent == Some(false) {
        return Some(false);
    }
    if let Some(schedule) = schedule_intent {
        return Some(schedule);
    }
    condition_intent
}

/// Reconcile a device's actual state with the computed effective state.
/// Skips if the device doesn't exist or if the effective state is None.
pub(crate) async fn reconcile_device(state: &AppState, device_name: &str, source: HookSource) {
    let device_cfg = match get_device_config(state, device_name).await {
        Ok(cfg) => cfg,
        Err(_) => return,
    };

    let intent = {
        let intents = state.device_intents.read().await;
        intents.get(device_name).cloned().unwrap_or_default()
    };
    let condition_intent = condition_intent_for_device(state, device_name).await;
    let effective = compute_effective(
        intent.manual_override,
        intent.schedule_intent,
        condition_intent,
    );
    let Some(target) = effective else {
        return;
    };

    let (current_state, nickname, model) = {
        let devices = state.devices.read().await;
        let device = devices.get(device_name);
        let current = device.and_then(|d| d.snapshot.as_ref().map(|s| s.device_on));
        let nickname = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.nickname.clone())
            .unwrap_or_else(|| device_name.to_string());
        let model = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.device_model.clone())
            .unwrap_or_else(|| {
                device
                    .map(|d| d.config.model.to_string())
                    .unwrap_or_default()
            });
        (current, nickname, model)
    };
    if current_state == Some(target) {
        info!(
            device = %device_name,
            target,
            "reconcile noop: device already at target state, no hook will fire"
        );
        return;
    }
    if current_state.is_none() {
        warn!(
            device = %device_name,
            target,
            "reconciling without a prior snapshot; set_power may fail if device is offline"
        );
    }

    info!(
        device = %device_name,
        manual = ?intent.manual_override,
        schedule = ?intent.schedule_intent,
        condition = ?condition_intent,
        target,
        "reconciling device state",
    );

    let operation_lock = device_operation_lock(state, &device_cfg).await;
    let _operation_guard = operation_lock.lock().await;

    if let Err(error) =
        retry_tapo_handshake(|| state.controller.set_power(&device_cfg, target)).await
    {
        warn!(device = %device_name, %error, "reconcile set_power failed");
        return;
    }

    // Optimistically update the cached snapshot so the later readback
    // (or absence of one) doesn't re-fire the same transition event.
    {
        let mut devices = state.devices.write().await;
        if let Some(device) = devices.get_mut(device_name) {
            if let Some(snap) = device.snapshot.as_mut() {
                snap.device_on = target;
            }
            device.updated_at_ms = Some(now_ms());
        }
    }

    // Fire the transition hook directly so it isn't dropped if the
    // post-set readback fails (which can happen on some Tapo plugs).
    dispatch_hook_events(
        state,
        device_name,
        &nickname,
        &model,
        if target {
            HookEvent::On
        } else {
            HookEvent::Off
        },
        source,
        current_state,
        Some(target),
    )
    .await;

    // Readback keeps energy/runtime stats fresh. update_device_snapshot
    // sees prev_on == new_on (thanks to the optimistic update above) and
    // won't refire the event.
    if let Ok(snapshot) = retry_tapo_handshake(|| state.controller.read_device(&device_cfg)).await {
        update_device_snapshot(state, device_name, snapshot, None, source).await;
    } else {
        publish_device_list(state, None).await;
    }
}

pub(crate) async fn set_schedule_intent(state: &AppState, device_name: &str, intent: bool) {
    let mut intents = state.device_intents.write().await;
    let entry = intents.entry(device_name.to_string()).or_default();
    entry.schedule_intent = Some(intent);
    // Schedule firing automatically releases any manual override.
    entry.manual_override = None;
    entry.manual_override_until_ms = None;
}

pub(crate) async fn set_manual_override(
    state: &AppState,
    device_name: &str,
    target: bool,
    duration_seconds: Option<u64>,
) {
    let mut intents = state.device_intents.write().await;
    let entry = intents.entry(device_name.to_string()).or_default();
    entry.manual_override = Some(target);
    entry.manual_override_until_ms = duration_seconds.map(|secs| {
        let bounded = secs
            .max(MIN_MANUAL_OVERRIDE_SECONDS)
            .min(MAX_MANUAL_OVERRIDE_SECONDS);
        now_ms() + (bounded as u128) * 1000
    });
}

pub(crate) async fn clear_manual_override(state: &AppState, device_name: &str) {
    let mut intents = state.device_intents.write().await;
    if let Some(entry) = intents.get_mut(device_name) {
        entry.manual_override = None;
        entry.manual_override_until_ms = None;
    }
}

pub(crate) async fn run_override_expiry_sweeper(state: AppState) {
    sleep(Duration::from_secs(2)).await;
    loop {
        let now = now_ms();
        let expired: Vec<String> = {
            let intents = state.device_intents.read().await;
            intents
                .iter()
                .filter_map(|(name, intent)| match intent.manual_override_until_ms {
                    Some(until) if until <= now && intent.manual_override.is_some() => {
                        Some(name.clone())
                    }
                    _ => None,
                })
                .collect()
        };

        if !expired.is_empty() {
            for name in &expired {
                clear_manual_override(&state, name).await;
                info!(device = %name, "manual override expired, returning to auto");
            }
            if let Err(error) = save_persisted_state(&state).await {
                warn!(%error, "failed to persist override expiry");
            }
            for name in &expired {
                reconcile_device(&state, name, HookSource::Manual).await;
            }
        }

        sleep(Duration::from_secs(5)).await;
    }
}
