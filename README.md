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
| `GET` | `/health` | none | Liveness |
| `GET` | `/v1/snapshot` | optional bearer | Curated JSON snapshot |
| `GET` | `/v1/events` | optional bearer | SSE stream of snapshot updates |
| `GET` | `/v1/commands/{name}` | optional bearer | Returns 404 if command unknown |
| `POST` | `/v1/commands/{name}` | optional bearer | Executes whitelisted command |

### Auth model

* Edge: **Cloudflare Access** validates the user before traffic reaches the
  tunnel. This is the primary barrier.
* App: optional `GATEWAY_API_TOKEN` adds a second-layer bearer check
  (handy for scripts that bypass Access, e.g. local tests).
* Commands are whitelist-only — no raw MQTT passthrough.

## Configuration

| Env var | Default | Description |
|---|---|---|
| `MQTT_HOST` | — | Cerbo host (required) |
| `MQTT_PORT` | `1883` | Cerbo MQTT port |
| `MQTT_USERNAME` | — | Victron MQTT user |
| `MQTT_PASSWORD` | — | Victron MQTT password |
| `MQTT_CLIENT_ID` | `inverter-gateway` | MQTT client id |
| `VICTRON_TOPIC_PREFIX` | `N/%instance%/` | Victron topic prefix |
| `HTTP_BIND` | `0.0.0.0:8080` | HTTP bind address |
| `GATEWAY_API_TOKEN` | (unset) | Optional bearer token |
| `RUST_LOG` | `info,inverter_gateway=debug` | tracing filter |

## Run locally

```bash
cp .env.example .env
# fill in Cerbo creds

cargo run --release
# or
docker compose -f docker-compose.example.yml --env-file .env up --build
```

The server listens on `:8080` and connects to the broker on first start.

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
5. The container binds only to host loopback; nothing is published to the LAN
   beyond what the tunnel loopback already exposes.

The repository contains no real hostname, tunnel id, or credential. The
deployment configuration lives in
`~/victron/terraform-github-victron/local.secrets.tfvars` (out of scope for
this repo).

## Whitelisted commands (v1)

| Name | Topic suffix | Payload | Notes |
|---|---|---|---|
| `silence_alarm` | `vebus/0/Alarm` | `{"SilenceAlarm":"1"}` | Acknowledge alarm |
| `reboot` | `system/0/Reboot` | `{"Value":1}` | Restarts the inverter |

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
