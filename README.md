# Fusebox

Local web control board for Tapo P110 plugs, with LAN discovery, WebSocket live updates, energy readings, and a fusebox-style browser UI.

## Features

- **LAN discovery:** scans the local network for supported Tapo plugs using `tapoctl` discovery.
- **Remembered devices:** saves discovered device configs to disk and reloads them on the next start.
- **Local control:** toggles plugs from the browser through the local Tapo API.
- **Sampled switch feedback:** plays a physical switch sample for browser toggles, with a lower-pitched OFF sound.
- **Live updates:** streams device snapshots to the browser over WebSocket instead of polling indefinitely.
- **Usage history chart:** plots selectable Tapo power history ranges with separate Chart.js lines for the total and each plug.
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

## Docker

Build and run the container locally:

```bash
docker build -t fusebox:local .
docker run --rm \
  --env-file .env \
  -e FUSEBOX_BIND=0.0.0.0:8787 \
  -e FUSEBOX_STATE_PATH=/data/state.json \
  -p 127.0.0.1:8787:8787 \
  -v fusebox-state:/data \
  fusebox:local
```

Or use the Compose example:

```bash
cp compose.example.yml compose.yml
docker compose up --build
```

The container stores its remembered device state at `/data/state.json`, backed by the `fusebox-state` volume in the example. Tapo credentials still come from `.env` at runtime and are not baked into the image.

The Compose example publishes the web UI on `127.0.0.1:8787` for safety. LAN discovery may be limited by Docker bridge networking. If the container is on a Docker-only network, set `FUSEBOX_DISCOVERY_TARGETS` to the real LAN CIDR for your plugs, for example `192.168.0.0/24`. Fusebox logs each selected target before scanning, so `docker logs fusebox` should show the CIDR and broadcast address it is trying.

On Linux, if explicit discovery targets still cannot see plugs on your LAN, you can try host networking by setting `network_mode: host`, removing the `ports` block, and setting `FUSEBOX_BIND=127.0.0.1:8787` unless you intentionally want to expose the unauthenticated UI beyond the host.

### Homelab reverse proxy setup

If Fusebox will sit behind a Tailscale-only Caddy container, put both containers on the same Docker network and let Caddy proxy to the service name:

```bash
docker network create intranet
cp compose.homelab.example.yml compose.yml
docker compose up --build
```

The homelab example joins the external `intranet` network and does not publish any ports to the host. Caddy can still reach Fusebox by service name because both containers are on the same Docker network. A matching Caddy route can use:

```caddyfile
fuse.drewett.dev {
  reverse_proxy fusebox:8787
}
```

This should still allow Fusebox to make normal outbound connections from the container to LAN plug IPs through Docker bridge NAT. The part that may not work reliably is broadcast-based LAN discovery. Set `FUSEBOX_DISCOVERY_TARGETS=192.168.0.0/24` or your actual plug LAN CIDR so Fusebox scans the LAN instead of only the Docker `intranet` CIDR. If toggling a remembered plug works but scanning still does not find new plugs, the container has LAN access but discovery broadcast is being blocked or isolated by Docker networking.

For full LAN discovery on a Linux homelab host, use one of these approaches:

- **Host networking:** run Fusebox with `network_mode: host`. This gives the process the host's LAN network stack, which is the most likely to make discovery work. The trade-off is that it cannot also join the Docker `intranet` network, so Caddy needs to proxy to the host address, for example `http://192.168.0.10:8787`, or Caddy also needs host networking. If you bind Fusebox to `0.0.0.0`, protect it with host firewall rules because Fusebox does not have browser authentication yet.
- **Macvlan:** give Fusebox its own LAN IP and also attach it to `intranet` for Caddy. This keeps Caddy service-name routing and gives Fusebox first-class LAN presence, but it is more fiddly: you need a macvlan parent interface, a LAN IP range, and often a host shim if the homelab host itself needs to talk to the container.
- **Bridge plus remembered devices:** keep the `intranet` example and rely on direct plug control once devices are known. This is the simplest option if discovery is not essential after initial setup.

### Macvlan plus private Caddy proxy

Use this when Fusebox needs proper LAN discovery and also needs to stay reachable by a Caddy container on the private `intranet` network.

Pick an unused LAN IP for Fusebox and find the host LAN interface:

```bash
ip route get 192.168.0.1
```

Create a writable state directory for the container user:

```bash
sudo install -d -o 10001 -g 10001 /srv/fusebox-data
```

Add the macvlan values to `.env`:

```env
FUSEBOX_DATA_DIR=/srv/fusebox-data
FUSEBOX_INTRANET_IP=10.10.0.30
FUSEBOX_DISCOVERY_TARGETS=192.168.0.0/24
FUSEBOX_LAN_PARENT=enp42s0
FUSEBOX_LAN_IP=192.168.0.50
FUSEBOX_LAN_SUBNET=192.168.0.0/24
FUSEBOX_LAN_GATEWAY=192.168.0.1
FUSEBOX_LAN_IP_RANGE=192.168.0.48/28
```

Use the macvlan Compose file:

```bash
docker network create intranet
cp compose.macvlan.example.yml compose.yml
docker compose up --build
```

The service joins both networks:

- `tapo_lan`: macvlan network for discovery and direct LAN access to Tapo plugs.
- `intranet`: private Docker network for Caddy to proxy to `fusebox:8787`.

The example binds Fusebox to `FUSEBOX_INTRANET_IP:8787`, not `0.0.0.0`, so the web UI stays on the private Docker network instead of also listening on the macvlan LAN IP. Pick an unused `intranet` IP from that Docker network's subnet.

Keep the Caddy route unchanged:

```caddyfile
fuse.drewett.dev {
  reverse_proxy fusebox:8787
}
```

Macvlan usually prevents the host itself from reaching the container's LAN IP unless you add a host-side macvlan shim. Caddy does not need that shim when it also shares the `intranet` network with Fusebox.

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
| `FUSEBOX_DISCOVERY_TARGETS` | Comma or space separated IPv4 addresses/CIDRs to scan instead of auto-detected local interfaces. Useful in Docker when plugs live on a different LAN, for example `192.168.0.0/24`. | No | auto-detected interfaces, then `255.255.255.255` fallback |
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
GET  /api/energy/history.json?range=7d
GET  /api/energy/export.xlsx
GET  /ws/devices
POST /api/scan
POST /api/devices/{name}/toggle
POST /api/devices/{name}/power
```

The usage history API accepts `5m`, `30m`, `1h`, `6h`, `12h`, `1d`, `3d`, `7d`, `30d`, `3m`, `6m`, `1y`, `ytd`, and `all`. Short ranges return power readings in W. Longer calendar ranges return energy readings in kWh so the graph stays readable.

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
- The usage history chart loads Chart.js from jsDelivr because Fusebox does not have a frontend build pipeline yet.
- Real control and energy verification needs access to the same LAN as the plugs.

## Audio Credits

- `/assets/switch.wav`: `Switch Light 06.wav` by tbrook, from <https://freesound.org/s/348224/>, licensed under Creative Commons 0.

## Related Projects

- [tapoctl](https://github.com/kierandrewett/tapoctl): Rust CLI and library for discovering, reading, and controlling Tapo plugs on the local network.

## Licence

Licensed under the Mozilla Public License Version 2.0. See [LICENSE](LICENSE).
