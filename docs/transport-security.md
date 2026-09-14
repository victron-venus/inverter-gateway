# Transport security and migration

## Gateway server

IGW accepts incoming API requests and connects to MQTT. It has no outbound HTTP
client and does not send Cloudflare Access service-token headers to another
server. HTTPS support here cannot change a consuming application's redirect
behavior.

The optional native HTTPS listener uses the same router and shared state as HTTP:
bearer/read-token authorization, command allowlisting, health, snapshot, energy,
and SSE remain identical. Both protocols use one MQTT bridge. HTTP stays on its
existing address with its existing responses; no HTTP-to-HTTPS redirect is added.

Configure all three variables together:

```sh
HTTPS_BIND=0.0.0.0:8443
GATEWAY_TLS_CERT_FILE=/run/secrets/igw-tls/tls.crt
GATEWAY_TLS_KEY_FILE=/run/secrets/igw-tls/tls.key
```

Use a certificate with a DNS subject alternative name matching the client URL.
Mount its PEM chain and private key read-only and keep the key outside Git and
release artifacts. Partial configuration, invalid certificates/keys, and bind
failures stop startup before either listener begins accepting requests. An HTTPS
configuration failure does not downgrade to an HTTP-only service. SIGTERM drains
both listeners for at most five seconds, including open SSE streams.

Certificates are loaded at startup. After certificate renewal, restart the
Deployment and check the served certificate's serial and expiry. Clients must
verify the certificate and hostname; do not use `-k`, `verify=false`, or insecure
certificate callbacks as a migration solution.

## MQTT

`MQTT_TLS=1` (or `true`) selects verified TLS. Use the certificate's DNS name in
`MQTT_HOST`; an IP address requires an IP SAN. A legacy certificate with only a
Common Name is insufficient. A certificate's issuer must be trusted by the system
store or by the optional PEM `MQTT_CA_FILE`.

When `MQTT_PORT` is absent, TCP uses 1883 and TLS uses 8883. An explicit port
continues to take precedence. The example environment explicitly selects 1883,
so change it to 8883 or remove that setting when enabling TLS. Unknown TLS flag
values and a CA file configured without TLS are rejected. Missing/invalid CA
files fail startup; unknown issuers, wrong names, and handshake failures prevent
MQTT CONNECT credentials from being sent and never retry with plaintext TCP.

TLS must be enabled only after checking the actual broker certificate. Existing
Cerbo installations using port 1883 remain compatible; shipping TLS support does
not imply that their broker connection has been migrated.

## Deployment and client sequence

1. Merge the gateway change through required CI, publish a beta, and verify the
   release manifest and OCI asset before deploying those exact bytes.
2. Add HTTPS to k3s while preserving HTTP host port 9150, NodePort 30150, existing
   credentials, MQTT identity, and the current Cloudflare tunnel route. The
   optional `deploy/k3s-https` overlay adds host port 9151 and NodePort 30151 and
   mounts the existing `s-wildcard-tls` Secret. On another site, adapt that Secret
   name and check port allocation before applying. Use a verified immutable image.
3. Verify trusted HTTPS health, authenticated snapshot/energy/SSE, rejected
   unauthorized writes, and unchanged HTTP responses with current telemetry.
4. Migrate consuming projects separately. Require HTTPS for remote gateway URLs
   before adding credentials. Disable automatic redirects on every authenticated
   request, or explicitly allow only a bounded sequence within the exact original
   HTTPS origin (scheme, host, effective port). Reject origin changes and HTTPS
   downgrades. Do not rely on libraries stripping `Authorization`: custom
   `CF-Access-Client-Id` and `CF-Access-Client-Secret` may still be forwarded.
5. Test two-server redirects with sentinel credentials, including polling,
   connection tests, commands, SSE, and reconnects. Verify no second server ever
   receives credentials on rejected redirects. Migrate the tunnel origin to a
   verified HTTPS route and migrate every HTTP consumer before removing HTTP.

The initial audit identified default redirect-following clients in
`inverter-desktop` (polling, commands, connection test) and
`inverter-dashboard-go` (polling and commands). The Python `inverter-dashboard`
client currently uses httpx without redirect following, but still permits HTTP
URLs. These are follow-up projects; this gateway release does not claim to fix
their URL validation or redirect policies.

The gateway tests exercise trusted TLS, certificate/name rejection, equivalent
HTTP/HTTPS authorization, no redirect responses, and MQTT credential protection.
These checks are distinct from release CI and from live broker/device validation.
