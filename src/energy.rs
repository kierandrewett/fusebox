use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Datelike, Days, Duration as ChronoDuration, NaiveDate, Utc};
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, Workbook};
use serde::{Deserialize, Serialize};
use tapo::{ApiClient, requests::EnergyDataInterval, requests::PowerDataInterval};
use tapoctl::{DeviceConfig, DeviceModel};
use tracing::warn;

use crate::api_error::AppError;
use crate::devices::device_operation_lock;
use crate::state::AppState;
use crate::time::now_ms;

pub(crate) const ALL_TIME_USAGE_START_YEAR: i32 = 2020;

// Static web assets and the index handler moved to crate::web.

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryResponse {
    pub(crate) series: Vec<UsageHistorySeries>,
    pub(crate) totals: Vec<UsageHistoryPoint>,
    pub(crate) errors: Vec<UsageHistoryError>,
    pub(crate) updated_at_ms: u128,
    pub(crate) range: &'static str,
    pub(crate) range_label: &'static str,
    pub(crate) interval: &'static str,
    pub(crate) start_date: String,
    pub(crate) end_date: String,
    pub(crate) unit: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistorySeries {
    pub(crate) device_name: String,
    pub(crate) points: Vec<UsageHistoryPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryPoint {
    pub(crate) timestamp_ms: i64,
    pub(crate) value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryError {
    pub(crate) device_name: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UsageHistoryQuery {
    pub(crate) range: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageHistoryRange {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) interval_label: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) start: UsageHistoryStart,
    pub(crate) kind: UsageHistoryKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UsageHistoryStart {
    Duration(ChronoDuration),
    YearToDate,
    AllTime,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UsageHistoryKind {
    Power {
        interval: PowerExportInterval,
        range_limit: ChronoDuration,
    },
    EnergyDaily,
    EnergyMonthly,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportDevice {
    pub(crate) name: String,
    pub(crate) config: DeviceConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportSpec {
    pub(crate) sheet_name: &'static str,
    pub(crate) value_format: &'static str,
    pub(crate) kind: ExportKind,
}

#[derive(Debug, Clone)]
pub(crate) enum ExportKind {
    EnergyHourly {
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    EnergyDaily {
        start_date: NaiveDate,
    },
    EnergyMonthly {
        start_date: NaiveDate,
    },
    PowerEvery5Minutes {
        ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    },
    PowerHourly {
        ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ExportTable {
    pub(crate) sheet_name: &'static str,
    pub(crate) value_format: &'static str,
    pub(crate) rows: Vec<ExportRow>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportRow {
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) values: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportError {
    pub(crate) sheet_name: &'static str,
    pub(crate) device_name: String,
    pub(crate) message: String,
}

pub(crate) async fn energy_history(
    State(state): State<AppState>,
    Query(query): Query<UsageHistoryQuery>,
) -> Json<UsageHistoryResponse> {
    Json(build_usage_history(&state, query.range.as_deref()).await)
}

pub(crate) async fn export_energy_workbook(
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let buffer = build_energy_export_workbook(&state).await?;

    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"fusebox-energy.xlsx\"",
            ),
        ],
        buffer,
    )
        .into_response())
}

pub(crate) fn estimate_energy_cost_pence(energy_wh: u64, price_pence_per_kwh: f64) -> f64 {
    energy_wh as f64 / 1000.0 * price_pence_per_kwh
}

pub(crate) async fn build_energy_export_workbook(state: &AppState) -> Result<Vec<u8>> {
    let devices = export_devices(state).await;
    let device_names = devices
        .iter()
        .map(|device| device.name.clone())
        .collect::<Vec<_>>();
    let specs = export_specs(Utc::now())?;
    let mut tables = Vec::with_capacity(specs.len());
    let mut errors = Vec::new();

    for spec in specs {
        let (table, mut sheet_errors) = collect_export_table(state, &devices, &spec).await;
        tables.push(table);
        errors.append(&mut sheet_errors);
    }

    write_export_workbook(&device_names, &tables, &errors)
}

pub(crate) async fn build_usage_history(
    state: &AppState,
    range_key: Option<&str>,
) -> UsageHistoryResponse {
    let range = usage_history_range(range_key);
    let devices = export_devices(state).await;
    let now = Utc::now();
    let start = usage_history_start_datetime(range.start, now);
    let mut series = Vec::with_capacity(devices.len());
    let mut totals_by_timestamp: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
    let mut errors = Vec::new();

    for device in devices {
        match read_usage_history_entries(state, &device.config, &range, start, now).await {
            Ok(entries) => {
                let mut points = Vec::new();

                for (timestamp, value) in entries {
                    if let Some(value) = value {
                        points.push(UsageHistoryPoint {
                            timestamp_ms: timestamp.timestamp_millis(),
                            value,
                        });
                        *totals_by_timestamp.entry(timestamp).or_default() += value;
                    }
                }

                series.push(UsageHistorySeries {
                    device_name: device.name,
                    points,
                });
            }
            Err(error) => errors.push(UsageHistoryError {
                device_name: device.name,
                message: error.to_string(),
            }),
        }
    }

    let totals = totals_by_timestamp
        .into_iter()
        .map(|(timestamp, value)| UsageHistoryPoint {
            timestamp_ms: timestamp.timestamp_millis(),
            value,
        })
        .collect();

    UsageHistoryResponse {
        series,
        totals,
        errors,
        updated_at_ms: now_ms(),
        range: range.key,
        range_label: range.label,
        interval: range.interval_label,
        start_date: start.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        end_date: now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        unit: range.unit,
    }
}

pub(crate) async fn read_usage_history_entries(
    state: &AppState,
    device: &DeviceConfig,
    range: &UsageHistoryRange,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    match range.kind {
        UsageHistoryKind::Power {
            interval,
            range_limit,
        } => {
            let ranges = split_datetime_ranges(start, end, range_limit);

            read_power_entries(state, device, &ranges, interval).await
        }
        UsageHistoryKind::EnergyDaily => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Daily {
                    start_date: start.date_naive(),
                },
            )
            .await
        }
        UsageHistoryKind::EnergyMonthly => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Monthly {
                    start_date: start.date_naive(),
                },
            )
            .await
        }
    }
}

pub(crate) fn usage_history_start_datetime(
    start: UsageHistoryStart,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    match start {
        UsageHistoryStart::Duration(duration) => now.checked_sub_signed(duration).unwrap_or(now),
        UsageHistoryStart::YearToDate => date_start_datetime(current_year_start(now.date_naive())),
        UsageHistoryStart::AllTime => {
            let start_date = NaiveDate::from_ymd_opt(ALL_TIME_USAGE_START_YEAR, 1, 1)
                .unwrap_or_else(|| current_year_start(now.date_naive()));

            date_start_datetime(start_date)
        }
    }
}

pub(crate) fn current_year_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), 1, 1).unwrap_or(date)
}

pub(crate) fn date_start_datetime(date: NaiveDate) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap_or_default(), Utc)
}

pub(crate) fn usage_history_range(range_key: Option<&str>) -> UsageHistoryRange {
    match range_key {
        Some("5m") => UsageHistoryRange {
            key: "5m",
            label: "5 minutes",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::minutes(5)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("30m") => UsageHistoryRange {
            key: "30m",
            label: "30 minutes",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::minutes(30)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("1h") => UsageHistoryRange {
            key: "1h",
            label: "1 hour",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(1)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("6h") => UsageHistoryRange {
            key: "6h",
            label: "6 hours",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(6)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("12h") => UsageHistoryRange {
            key: "12h",
            label: "12 hours",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(12)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("1d") => UsageHistoryRange {
            key: "1d",
            label: "1 day",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(1)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("3d") => UsageHistoryRange {
            key: "3d",
            label: "3 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(3)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
        Some("30d") => UsageHistoryRange {
            key: "30d",
            label: "30 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(30)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
        Some("3m") => UsageHistoryRange {
            key: "3m",
            label: "3 months",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(92)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("6m") => UsageHistoryRange {
            key: "6m",
            label: "6 months",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(183)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("1y") => UsageHistoryRange {
            key: "1y",
            label: "1 year",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(365)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("ytd") => UsageHistoryRange {
            key: "ytd",
            label: "year to date",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::YearToDate,
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("all") => UsageHistoryRange {
            key: "all",
            label: "all time",
            interval_label: "monthly energy",
            unit: "kWh",
            start: UsageHistoryStart::AllTime,
            kind: UsageHistoryKind::EnergyMonthly,
        },
        _ => UsageHistoryRange {
            key: "7d",
            label: "7 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(7)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
    }
}

pub(crate) async fn export_devices(state: &AppState) -> Vec<ExportDevice> {
    let devices = state.devices.read().await;

    devices
        .values()
        .filter(|device| matches!(device.config.model, DeviceModel::P110 | DeviceModel::P115))
        .map(|device| ExportDevice {
            name: device.name.clone(),
            config: device.config.clone(),
        })
        .collect()
}

pub(crate) fn export_specs(now: DateTime<Utc>) -> Result<Vec<ExportSpec>> {
    let today = now.date_naive();
    let week_start = today
        .checked_sub_days(Days::new(6))
        .ok_or_else(|| anyhow!("failed to calculate weekly energy export start date"))?;
    let quarter_start = current_quarter_start(today)?;
    let year_start = NaiveDate::from_ymd_opt(today.year(), 1, 1)
        .ok_or_else(|| anyhow!("failed to calculate yearly energy export start date"))?;
    let power_day_start = now
        .checked_sub_signed(ChronoDuration::hours(24))
        .ok_or_else(|| anyhow!("failed to calculate 24 hour power export start time"))?;
    let power_week_start = now
        .checked_sub_signed(ChronoDuration::days(7))
        .ok_or_else(|| anyhow!("failed to calculate weekly power export start time"))?;

    Ok(vec![
        ExportSpec {
            sheet_name: "Energy - Hourly (last week)",
            value_format: "0.000",
            kind: ExportKind::EnergyHourly {
                start_date: week_start,
                end_date: today,
            },
        },
        ExportSpec {
            sheet_name: "Energy - Daily (last 3 mo)",
            value_format: "0.000",
            kind: ExportKind::EnergyDaily {
                start_date: quarter_start,
            },
        },
        ExportSpec {
            sheet_name: "Energy - Monthly (last year)",
            value_format: "0.000",
            kind: ExportKind::EnergyMonthly {
                start_date: year_start,
            },
        },
        ExportSpec {
            sheet_name: "Power - 5min (last 24h)",
            value_format: "0.0",
            kind: ExportKind::PowerEvery5Minutes {
                ranges: split_datetime_ranges(power_day_start, now, ChronoDuration::hours(12)),
            },
        },
        ExportSpec {
            sheet_name: "Power - Hourly (last week)",
            value_format: "0.0",
            kind: ExportKind::PowerHourly {
                ranges: split_datetime_ranges(power_week_start, now, ChronoDuration::days(6)),
            },
        },
    ])
}

pub(crate) fn current_quarter_start(date: NaiveDate) -> Result<NaiveDate> {
    let month = match date.month() {
        1..=3 => 1,
        4..=6 => 4,
        7..=9 => 7,
        10..=12 => 10,
        _ => return Err(anyhow!("invalid month {}", date.month())),
    };

    NaiveDate::from_ymd_opt(date.year(), month, 1)
        .ok_or_else(|| anyhow!("failed to calculate current quarter start date"))
}

pub(crate) fn split_datetime_ranges(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    max_duration: ChronoDuration,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut ranges = Vec::new();
    let mut cursor = start;

    while cursor < end {
        let next = cursor
            .checked_add_signed(max_duration)
            .filter(|candidate| *candidate < end)
            .unwrap_or(end);
        ranges.push((cursor, next));
        cursor = next;
    }

    ranges
}

pub(crate) async fn collect_export_table(
    state: &AppState,
    devices: &[ExportDevice],
    spec: &ExportSpec,
) -> (ExportTable, Vec<ExportError>) {
    let mut rows_by_timestamp: BTreeMap<DateTime<Utc>, BTreeMap<String, f64>> = BTreeMap::new();
    let mut errors = Vec::new();

    for device in devices {
        match read_export_entries(state, &device.config, spec).await {
            Ok(entries) => {
                for (timestamp, value) in entries {
                    if let Some(value) = value {
                        rows_by_timestamp
                            .entry(timestamp)
                            .or_default()
                            .insert(device.name.clone(), value);
                    }
                }
            }
            Err(error) => errors.push(ExportError {
                sheet_name: spec.sheet_name,
                device_name: device.name.clone(),
                message: error.to_string(),
            }),
        }
    }

    let rows = rows_by_timestamp
        .into_iter()
        .map(|(timestamp, values)| ExportRow { timestamp, values })
        .collect();

    (
        ExportTable {
            sheet_name: spec.sheet_name,
            value_format: spec.value_format,
            rows,
        },
        errors,
    )
}

pub(crate) async fn read_export_entries(
    state: &AppState,
    device: &DeviceConfig,
    spec: &ExportSpec,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    match &spec.kind {
        ExportKind::EnergyHourly {
            start_date,
            end_date,
        } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Hourly {
                    start_date: *start_date,
                    end_date: *end_date,
                },
            )
            .await
        }
        ExportKind::EnergyDaily { start_date } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Daily {
                    start_date: *start_date,
                },
            )
            .await
        }
        ExportKind::EnergyMonthly { start_date } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Monthly {
                    start_date: *start_date,
                },
            )
            .await
        }
        ExportKind::PowerEvery5Minutes { ranges } => {
            read_power_entries(state, device, ranges, PowerExportInterval::Every5Minutes).await
        }
        ExportKind::PowerHourly { ranges } => {
            read_power_entries(state, device, ranges, PowerExportInterval::Hourly).await
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PowerExportInterval {
    Every5Minutes,
    Hourly,
}

pub(crate) async fn read_energy_entries(
    state: &AppState,
    device: &DeviceConfig,
    interval: EnergyDataInterval,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    let operation_lock = device_operation_lock(state, device).await;
    let _operation_guard = operation_lock.lock().await;
    let result = match device.model {
        DeviceModel::P110 => {
            historical_client(state)
                .p110(device.ip.to_string())
                .await?
                .get_energy_data(interval)
                .await?
        }
        DeviceModel::P115 => {
            historical_client(state)
                .p115(device.ip.to_string())
                .await?
                .get_energy_data(interval)
                .await?
        }
        DeviceModel::P100 | DeviceModel::P105 => {
            return Err(anyhow!(
                "{} at {} does not support energy monitoring",
                device.model,
                device.ip,
            ));
        }
    };

    Ok(result
        .entries
        .into_iter()
        .map(|entry| (entry.start_date_time, Some(entry.energy as f64 / 1000.0)))
        .collect())
}

pub(crate) async fn read_power_entries(
    state: &AppState,
    device: &DeviceConfig,
    ranges: &[(DateTime<Utc>, DateTime<Utc>)],
    interval: PowerExportInterval,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    let operation_lock = device_operation_lock(state, device).await;
    let _operation_guard = operation_lock.lock().await;
    let mut entries = Vec::new();

    for (start_date_time, end_date_time) in ranges {
        let interval = match interval {
            PowerExportInterval::Every5Minutes => PowerDataInterval::Every5Minutes {
                start_date_time: *start_date_time,
                end_date_time: *end_date_time,
            },
            PowerExportInterval::Hourly => PowerDataInterval::Hourly {
                start_date_time: *start_date_time,
                end_date_time: *end_date_time,
            },
        };
        let result = match device.model {
            DeviceModel::P110 => {
                historical_client(state)
                    .p110(device.ip.to_string())
                    .await?
                    .get_power_data(interval)
                    .await?
            }
            DeviceModel::P115 => {
                historical_client(state)
                    .p115(device.ip.to_string())
                    .await?
                    .get_power_data(interval)
                    .await?
            }
            DeviceModel::P100 | DeviceModel::P105 => {
                return Err(anyhow!(
                    "{} at {} does not support energy monitoring",
                    device.model,
                    device.ip,
                ));
            }
        };

        entries.extend(
            result
                .entries
                .into_iter()
                .map(|entry| (entry.start_date_time, entry.power.map(|power| power as f64))),
        );
    }

    Ok(entries)
}

pub(crate) fn historical_client(state: &AppState) -> ApiClient {
    ApiClient::new(&state.credentials.username, &state.credentials.password)
        .with_timeout(Duration::from_secs(30))
}

pub(crate) fn write_export_workbook(
    device_names: &[String],
    tables: &[ExportTable],
    errors: &[ExportError],
) -> Result<Vec<u8>> {
    let mut workbook = Workbook::new();

    for table in tables {
        write_export_table(&mut workbook, device_names, table)?;
    }

    if !errors.is_empty() {
        write_export_errors(&mut workbook, errors)?;
    }

    workbook
        .save_to_buffer()
        .context("failed to build energy export workbook")
}

pub(crate) fn write_export_table(
    workbook: &mut Workbook,
    device_names: &[String],
    table: &ExportTable,
) -> Result<()> {
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center);
    let value_format = Format::new().set_num_format(table.value_format);
    let worksheet = workbook.add_worksheet().set_name(table.sheet_name)?;

    worksheet.set_column_width(0, 24)?;
    worksheet.write_with_format(0, 0, "Timestamp", &header_format)?;

    for (index, name) in device_names.iter().enumerate() {
        let column = (index + 1) as u16;
        worksheet.set_column_width(column, 18)?;
        worksheet.write_with_format(0, column, name, &header_format)?;
    }

    let total_column = (device_names.len() + 1) as u16;
    worksheet.set_column_width(total_column, 14)?;
    worksheet.write_with_format(0, total_column, "Total", &header_format)?;

    for (row_index, row) in table.rows.iter().enumerate() {
        let worksheet_row = (row_index + 1) as u32;
        worksheet.write(
            worksheet_row,
            0,
            row.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        )?;

        for (index, name) in device_names.iter().enumerate() {
            if let Some(value) = row.values.get(name) {
                worksheet.write_with_format(
                    worksheet_row,
                    (index + 1) as u16,
                    *value,
                    &value_format,
                )?;
            }
        }

        let total = row.values.values().sum::<f64>();
        worksheet.write_with_format(worksheet_row, total_column, total, &value_format)?;
    }

    Ok(())
}

pub(crate) fn write_export_errors(workbook: &mut Workbook, errors: &[ExportError]) -> Result<()> {
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center);
    let worksheet = workbook.add_worksheet().set_name("Export Errors")?;

    worksheet.set_column_width(0, 32)?;
    worksheet.set_column_width(1, 22)?;
    worksheet.set_column_width(2, 72)?;
    worksheet.write_with_format(0, 0, "Sheet", &header_format)?;
    worksheet.write_with_format(0, 1, "Device", &header_format)?;
    worksheet.write_with_format(0, 2, "Error", &header_format)?;

    for (index, error) in errors.iter().enumerate() {
        let row = (index + 1) as u32;
        worksheet.write(row, 0, error.sheet_name)?;
        worksheet.write(row, 1, &error.device_name)?;
        worksheet.write(row, 2, &error.message)?;
    }

    Ok(())
}
