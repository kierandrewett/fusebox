# Fusebox

Local web control board for Tapo P110 plugs, with LAN discovery, WebSocket live updates, energy readings, and a fusebox-style browser UI.

## Features

- **LAN discovery:** scans the local network for supported Tapo plugs using `tapoctl` discovery.
- **Remembered devices:** saves discovered device configs to disk and reloads them on the next start.
- **Local control:** toggles plugs from the browser through the local Tapo API.
- **Live updates:** streams device snapshots to the browser over WebSocket instead of polling indefinitely.
- **Energy readings:** shows current load, daily energy, monthly energy, estimated UK cost, and runtime for P110/P115-style plugs that expose energy data.
- **Spreadsheet export:** generates an Excel workbook with hourly, daily, monthly, and power-history sheets.
- **Safe default bind:** listens on `127.0.0.1:8787` unless you explicitly change `FUSEBOX_BIND`.

## Getting Started

```bash
git clone https://github.com/kierandrewett/fusebox.git
cd fusebox
cp .env.example .env
```

Edit `.env` and set your Tapo cloud credentials, then run:

```bash
cargo run
```

Open [http://127.0.0.1:8787](http://127.0.0.1:8787).

## Environment Variables

| Variable | Description | Required | Default |
| --- | --- | --- | --- |
| `TAPO_USERNAME` | Tapo cloud account email used for local device authentication. | Yes | none |
| `TAPO_PASSWORD` | Tapo cloud account password used for local device authentication. | Yes | none |
| `FUSEBOX_BIND` | Socket address the web server listens on. Keep this local unless you add authentication. | No | `127.0.0.1:8787` |
| `FUSEBOX_REFRESH_SECONDS` | Poll interval for discovered devices. | No | `10` |
| `FUSEBOX_SCAN_SECONDS` | Background LAN discovery interval. Manual scan is still available in the UI. | No | `60` |
| `FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH` | Estimated UK electricity unit rate used for cost display. | No | `27.03` |
| `FUSEBOX_DISCOVERY_TIMEOUT_SECONDS` | Discovery timeout per scan, from 1 to 60 seconds. | No | `5` |
| `FUSEBOX_STATE_PATH` | JSON file used to remember discovered device configs. | No | `$XDG_CONFIG_HOME/fusebox/state.json` or `$HOME/.config/fusebox/state.json` |
| `RUST_LOG` | Logging filter. Fusebox defaults to `info` when this is unset. | No | `info` |

## State File

Fusebox persists discovered device names, IP addresses, and models to a JSON state file. It does not store Tapo credentials or live energy snapshots.

By default the file lives at:

```text
$XDG_CONFIG_HOME/fusebox/state.json
```

If `XDG_CONFIG_HOME` is not set, Fusebox uses:

```text
$HOME/.config/fusebox/state.json
```

Set `FUSEBOX_STATE_PATH` if you want the file somewhere else, for example when running under systemd or inside a container.

## Spreadsheet Export

The `GET /api/energy/export.xlsx` route builds a new workbook when you request it. It does not read from Fusebox's state file, and it does not use a database of readings collected by Fusebox.

For each remembered P110/P115 device, Fusebox signs in to the plug over the local Tapo API and asks the device for its own history:

- `Energy - Hourly (last week)`: hourly energy history, written as kWh.
- `Energy - Daily (last 3 mo)`: daily energy history from the start of the current quarter, written as kWh.
- `Energy - Monthly (last year)`: monthly energy history from 1 January, written as kWh.
- `Power - 5min (last 24h)`: 5-minute power history, written as W.
- `Power - Hourly (last week)`: hourly power history, written as W.

The numbers are the historical values returned by the plug through the Tapo API. Fusebox only reshapes them into workbook sheets and totals the device columns for each timestamp. If a plug cannot return a sheet, the workbook includes an `Export Errors` sheet with the device name and error message.

Cost values are not exported yet. The costs shown in the UI are estimates calculated from the live Wh counters and `FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH`.

## API

```text
GET  /health
GET  /api/devices
GET  /api/energy/export.xlsx
GET  /ws/devices
POST /api/scan
POST /api/devices/{name}/toggle
POST /api/devices/{name}/power
```

Set a device explicitly with JSON:

```bash
curl -X POST http://127.0.0.1:8787/api/devices/lights/power \
  -H 'content-type: application/json' \
  -d '{"on": true}'
```

## Security Notes

Fusebox does not implement browser authentication yet. The default bind address is localhost for that reason. Do not bind it to `0.0.0.0` or expose it outside a trusted machine unless you add an authentication layer in front of it.

Credentials are read from environment variables only. Do not commit `.env` files.

## Requirements

- Rust with edition 2024 support.
- Tapo P110/P115-style plugs on the same LAN.
- Tapo credentials for local device authentication.

## Current Limitations

- The state file remembers discovered devices, but Fusebox does not automatically prune devices that have been removed from the LAN.
- Energy readings depend on the device model and what the local Tapo API returns.
- Real control and energy verification needs access to the same LAN as the plugs.

## Licence

No licence has been set yet.
