# Kubernetes deployment

This overlay describes the existing `synology-apps/inverter-gateway` service on
the `syn` Kubernetes node. It runs outside Cerbo GX. The Docker Compose examples
are an alternative installation method; do not start a second gateway during an
update of this deployment.

The `inverter-gateway` Secret must already exist in `synology-apps`. Keep the
existing MQTT credentials, portal ID, full API token, and bind address. Add a
distinct `GATEWAY_READ_TOKEN` and the installation-specific energy source settings
described in [the energy API guide](../../docs/energy-api.md). Do not commit a
rendered Secret or put tokens in command-line arguments.

The preserved host port and NodePort support the existing tunnel route. They are
network listeners, not authentication controls: keep the gateway bearer check
enabled and retain the site's firewall and Cloudflare Access policy. A local
consumer can use the ClusterIP service; an external consumer uses the protected
HTTPS hostname. Machine clients should send a descriptive `User-Agent`, such as
`IGWEnergyVoice/1.0`, as well as the required authorization headers.

## Upgrade

1. Save the current Deployment and Secret to a private, mode-600 backup location.
2. Record the running image digest. Select an immutable release image or digest
   that has passed CI, and update `deployment.yaml` accordingly.
3. Merge new energy settings into the existing Secret, preserving unrelated keys.
4. Review `kubectl diff -k deploy/k3s`, then apply only this gateway overlay:

   ```sh
   kubectl apply -k deploy/k3s
   kubectl -n synology-apps rollout status deployment/inverter-gateway --timeout=120s
   ```

5. Verify `/health`, authenticated `/v1/snapshot`, and `/v1/energy`. Compare the
   configured energy totals with the actual MQTT source values. Confirm a read
   token is rejected on both GET and POST command routes.
6. Verify the existing external HTTPS path and the installed voice adapters.

The Recreate strategy preserves one MQTT client and one host-port owner. It
causes a short update interruption; adapters must speak their unavailable
response during that interval. `/health` reports broker connectivity in its
body; an HTTP 200 alone does not establish that telemetry is available.

## Rollback

Restore the saved Secret and set the Deployment image back to the recorded
digest. Wait for rollout completion and repeat the authenticated snapshot check.
Keep private backups out of Git and release archives. Restore the previous voice
package if its required energy API is no longer available.
