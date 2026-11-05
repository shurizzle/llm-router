# Deployment

The router is a single stateless process. It needs only its config file and the provider
credentials in its environment.

## Running locally

```bash
cargo build --release
SYNTHETIC_API_KEY=... NANOGPT_API_KEY=... ./target/release/llm-router --config config.example.json
```

Defaults:

- config path: `--config`, then `LLM_ROUTER_CONFIG`, then `/etc/llm-router/config.json`;
- listen address: `listen` in the config, then `LLM_ROUTER_LISTEN`, then `:8787`.

## Docker

Build the image locally:

```bash
docker build -t llm-router:local .
```

The image is a static musl build on a distroless base. It ships `config.example.json` as
`/etc/llm-router/config.json`; mount your own file over it. The image has no shell, and the
health check runs the binary itself with `--healthcheck`.

```bash
docker run -d --name llm-router \
  -p 127.0.0.1:8787:8787 \
  -e SYNTHETIC_API_KEY \
  -v "$PWD/config.json:/etc/llm-router/config.json:ro" \
  llm-router:local
curl -s localhost:8787/readyz
```

## Compose

```bash
export SYNTHETIC_API_KEY=... NANOGPT_API_KEY=...
docker compose up -d --build
```

The provided `compose.yaml` binds `127.0.0.1:8787`, passes both provider keys and the optional
client token, and mounts `config.example.json` read-only. Use a private config file for real
deployments.

## Kubernetes

```yaml
containers:
  - name: llm-router
    image: llm-router:0.2.1
    ports: [{ name: http, containerPort: 8787 }]
    env:
      - name: SYNTHETIC_API_KEY
        valueFrom: { secretKeyRef: { name: llm-router, key: synthetic-api-key, optional: true } }
      - name: NANOGPT_API_KEY
        valueFrom: { secretKeyRef: { name: llm-router, key: nanogpt-api-key, optional: true } }
      - name: LLM_ROUTER_CLIENT_TOKEN
        valueFrom: { secretKeyRef: { name: llm-router, key: client-token } }
    volumeMounts:
      - { name: config, mountPath: /etc/llm-router, readOnly: true }
    livenessProbe:  { httpGet: { path: /livez,  port: http } }
    readinessProbe: { httpGet: { path: /readyz, port: http }, periodSeconds: 2 }
    securityContext:
      runAsNonRoot: true
      readOnlyRootFilesystem: true
      allowPrivilegeEscalation: false
      capabilities: { drop: [ALL] }
terminationGracePeriodSeconds: 90
```

Notes:

- `readinessProbe` fails as soon as `SIGTERM` arrives, before listeners close, so endpoints are
  removed from rotation first.
- Keep `terminationGracePeriodSeconds` above `LLM_ROUTER_DRAIN_DELAY` plus the longest streamed
  request you expect. The process waits up to 60 seconds for in-flight connections after the
  drain delay.
- Scrape `/metrics` on the `http` port with a ServiceMonitor or PodMonitor, or on the admin
  listener if you separate it.

## Exposing the router publicly

Without protection, anyone who can reach the router can spend your provider keys.

1. Set a long random `LLM_ROUTER_CLIENT_TOKEN` (for example `openssl rand -hex 32`).
2. Set `LLM_ROUTER_ADMIN_LISTEN` (for example `:9090`) so probes and metrics do not require
   the token and the API port answers nothing without it.
3. Point probes and scraping at the admin port.
4. Route only the API port through the gateway.

Clients must send `X-Llm-Router-Token`. With Claude Code:

```bash
export ANTHROPIC_CUSTOM_HEADERS="X-Llm-Router-Token: <token>"
```

A missing or wrong token closes the connection without an HTTP response, so clients report a
connection error. A reverse proxy in front still answers on its own after the router drops the
connection, typically with `502` or `503`; rate-limit unauthenticated traffic there as well.

## Health check

`--healthcheck` connects to `127.0.0.1:<port>`, requests `/livez`, and exits non-zero on
failure. It uses the admin port when `LLM_ROUTER_ADMIN_LISTEN` is set, otherwise the API port.
Timeouts are 3 seconds.

`--check-config` loads, expands and validates the config file, prints
`{"msg":"config ok","path":...}` and exits 0 without binding. Use it in CI and before a
restart or `SIGHUP` reload; an invalid file exits 1 with the `fatal` line.

## Reloading

The config file is watched, so saving it reloads the config file, credentials, client token,
routing table, quota tasks and the egress client. `SIGHUP` triggers the same reload and stays
useful when the watch is disabled. Listeners and in-flight requests keep running; the next
request on a keep-alive connection uses the new routing table. A reload that fails to load or
validate logs `reload failed` and leaves the previous configuration in place. `listen`,
`LLM_ROUTER_ADMIN_LISTEN` and `LLM_ROUTER_DRAIN_DELAY` are read only at startup. Files pulled
in through `{file:...}` templates and `network.ca_file` are not watched; edit the config file
to pick up their new contents.

The watch uses the `notify` crate, enabled by the default `watch` Cargo feature. Build with
`--no-default-features` to remove it; `SIGHUP` still works.

```bash
./llm-router --check-config --config /etc/llm-router/config.json && kill -HUP "$(pidof llm-router)"
```

## Scaling and availability

- The process is stateless; replicas do not coordinate, and any replica can serve any request.
- Metrics are per process. Aggregate with Prometheus, not by assuming shared counters.
- Quota polling runs independently in each replica, so an upstream quota endpoint sees one
  scrape per replica per interval.
- A replica whose default-route credential is missing fails startup by design; make the secret
  required in that case.

## TLS and certificates

Upstreams are called with `rustls` and the system certificate store. On minimal container
images or host distributions without CA certificates, outbound HTTPS fails; install
`ca-certificates` (or mount the store) in that environment. Add private roots with
`network.ca_file` (PEM bundle, merged into the native store):

```json
"network": { "ca_file": "./certs/internal-ca.pem" }
```

The router itself serves plain HTTP; terminate TLS at a reverse proxy or ingress.

## Egress proxy

Every upstream can be reached through an HTTP CONNECT proxy:

```json
"network": { "proxy": "{env:HTTPS_PROXY}" }
```

Only `http://` proxy URLs are supported; credentials come from the URL userinfo
(`http://user:password@proxy:3128`) and are sent as Basic auth on CONNECT. The tunnel is used
for every upstream, including plain `http://` ones, and for quota polling. `https://` proxies
are rejected at startup.

## launchd (macOS)

A per-user LaunchAgent keeps the router running and restarts it on crash:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.example.llm-router</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/.local/bin/llm-router</string>
    <string>--config</string>
    <string>/Users/me/.config/llm-router/config.json</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>/Users/me/Library/Logs/llm-router.log</string>
  <key>StandardErrorPath</key><string>/Users/me/Library/Logs/llm-router.log</string>
</dict>
</plist>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.example.llm-router.plist
launchctl kickstart -k gui/$(id -u)/dev.example.llm-router   # restart
launchctl bootout gui/$(id -u)/dev.example.llm-router        # stop and unload
kill -HUP "$(pgrep -f dev.example.llm-router | head -1)"     # reload config
```

After replacing the binary, restart with `kickstart -k`. An invalid config makes the service
crash-loop every `ThrottleInterval` seconds; run `--check-config` first.
