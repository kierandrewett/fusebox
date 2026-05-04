# Fusebox

Local web control board for Tapo P110 plugs, with LAN discovery, live state polling, energy readings, and a fusebox-style browser UI.

## Features

- **LAN discovery:** scans the local network for supported Tapo plugs using `tapoctl` discovery.
- **Local control:** toggles plugs from the browser through the local Tapo API.
- **Energy readings:** shows current load, daily energy, monthly energy, and runtime for P110/P115-style plugs that expose energy data.
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
| `FUSEBOX_DISCOVERY_TIMEOUT_SECONDS` | Discovery timeout per scan, from 1 to 60 seconds. | No | `5` |

## API

```text
GET  /health
GET  /api/devices
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

- Device discovery is in-memory only. Restarting Fusebox clears the discovered list until the next scan.
- Energy readings depend on the device model and what the local Tapo API returns.
- Real control and energy verification needs access to the same LAN as the plugs.

## Licence

No licence has been set yet.
