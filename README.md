# inverter-gateway

Remote HTTPS/SSE gateway for a Victron Cerbo GX, fronted by Cloudflare
Tunnel + Cloudflare Access. Subscribes to the local MQTT broker, exposes a
curated JSON snapshot, streams changes over SSE, and accepts a whitelisted
set of commands.

This is the **read-mostly** layer of the project. The richer client
(inverter-desktop) will gain a Remote Gateway profile in a follow-up PR.

## Endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET` | `/health` | none | Liveness (returns `mqtt_connected` status) |
| `GET` | `/v1/snapshot` | **required** | Curated JSON snapshot |
| `GET` | `/v1/events` | **required** | SSE stream of snapshot updates |
| `GET` | `/v1/commands/{name}` | **required** | Returns 404 if command unknown |
| `POST` | `/v1/commands/{name}` | **required** | Executes whitelisted command |

### Auth model

* **Edge: Cloudflare Access** validates the user before traffic reaches the
  tunnel. This is the primary barrier.
* **App: bearer token** is required by default (`GATEWAY_API_TOKEN`).
  Set `GATEWAY_ALLOW_INSECURE=1` only as a local LAN escape hatch —
  the server prints a loud warning on boot.
* Commands are whitelist-only — no raw MQTT passthrough.

### /health

Returns `mqtt_connected` reflecting the live MQTT connection state:

```json
{"status":"ok","mqtt_connected":true}
```

`/health` is intentionally unauthenticated (used by k8s/CF health probes).

## Configuration

| Env var | Default | Description |
|---|---|---|
| `MQTT_HOST` | — | Cerbo host (required) |
| `MQTT_PORT` | `1883` | Cerbo MQTT port |
| `MQTT_USERNAME` | — | Victron MQTT user |
| `MQTT_PASSWORD` | — | Victron MQTT password |
| `MQTT_CLIENT_ID` | `inverter-gateway` | MQTT client id |
| `VICTRON_PORTAL_ID` | — | Portal id → builds `N/<portal_id>/` prefix automatically |
| `VICTRON_TOPIC_PREFIX` | `N/<portal_id>/` | Victron topic prefix. **Replace with your real portal id.** |
| `HTTP_BIND` | `127.0.0.1:8080` | HTTP bind — **must be loopback** |
| `GATEWAY_API_TOKEN` | — | Bearer token (required; set `GATEWAY_ALLOW_INSECURE=1` for local tests) |
| `GATEWAY_ALLOW_INSECURE` | `0` | Set `1` to skip token check (local LAN only) |
| `GATEWAY_CORS_ORIGINS` | (none) | Comma-separated allowed origins; empty = same-origin only |
| `RUST_LOG` | `info,inverter_gateway=debug` | tracing filter |

### Finding your portal id

The `VICTRON_TOPIC_PREFIX` must match your Cerbo's VRM portal id. To find it:

1. Open **VRM Portal** → your device → "Info" → "System instance".
2. Or run `dbus-spy` on Cerbo and look at the MQTT root topic.
3. Example: `N/abc123def/` — `abc123def` is the portal id.

### CORS

By default no CORS headers are added (same-origin policy). If a browser
app needs to call the API directly, set `GATEWAY_CORS_ORIGINS` to the
origin(s), e.g. `https://app.example.com`.

## Run locally

```bash
cp .env.example .env
# fill in Cerbo creds and set GATEWAY_API_TOKEN
# optionally GATEWAY_ALLOW_INSECURE=1 for quick local tests

cargo run --release
# or
docker compose -f docker-compose.example.yml --env-file .env up --build
```

## Deploy on Synology (LAN) via Cloudflare Tunnel

1. **Container Manager → Project → Create**, point at the cloned repo, no
   environment file change. Or `docker compose up -d` over SSH.
2. **Cloudflare Zero Trust → Access → Tunnels** — reuse the existing tunnel
   that already fronts your other services.
3. Add a public hostname:
   * Subdomain: pick one (e.g. `gateway.example.invalid`)
   * Service: `http://127.0.0.1:8080`
   * Path: leave empty
4. **Access policy**: pin a single email (or IdP group) and require MFA.
5. The container binds only to host loopback (`127.0.0.1:8080`);
   nothing is published to the LAN beyond what the tunnel loopback already exposes.

The repository contains no real hostname, tunnel id, or credential. The
deployment configuration lives in
`~/victron/terraform-github-victron/local.secrets.tfvars` (out of scope for
this repo).

## Whitelisted commands (v1)

| Name | Topic suffix | Payload | Notes |
|---|---|---|---|
| `silence_alarm` | `vebus/0/Alarm` | `{"SilenceAlarm":"1"}` | Acknowledge active alarm |

Adding a command: edit `src/whitelist.rs::builtin_whitelist`. Anything not
in that table returns 404.

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three commands on every PR.

## License

MIT — see [LICENSE](LICENSE).
