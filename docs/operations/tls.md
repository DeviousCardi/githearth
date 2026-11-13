# TLS ingress runbook

## Architecture

TLS terminates at a reverse proxy on the same host.

- **GitHearth** binds `127.0.0.1:8080` (plain HTTP, loopback only).
- **SSH (2222)** is served directly by GitHearth — not proxied.
- The reverse proxy handles TLS, HSTS, and forwards requests to `127.0.0.1:8080`.
- `X-Forwarded-For` is consumed when the proxy address is listed in
  `trusted_proxies`: the resolved client IP is written to audit events. The app
  does not consume `X-Forwarded-Proto` or `X-Forwarded-Host`, performs no
  IP-based rate limiting, and does not filter cookie domains or validate the
  Host header; set `trusted_proxies` to the proxy's address (not a public
  range).

## nginx configuration

Save as e.g. `/etc/nginx/sites-available/githearth`, then symlink into
`sites-enabled` and reload nginx.

Replace `/etc/ssl/certs/githearth.crt` and `/etc/ssl/private/githearth.key`
with your real certificate paths.

```nginx
# TLS ingress for GitHearth — terminates TLS and proxies to loopback.
# Assumes systemd-managed nginx with a single worker or shared SSL session cache.

upstream githearth_backend {
    server 127.0.0.1:8080;
    keepalive 16;
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name git.example.com;

    ssl_certificate     /etc/ssl/certs/githearth.crt;
    ssl_certificate_key /etc/ssl/private/githearth.key;
    ssl_protocols       TLSv1.2 TLSv1.3;
    ssl_ciphers         HIGH:!aNULL:!MD5;
    ssl_prefer_server_ciphers on;

    # HSTS: browsers use HTTPS only, for one year (31536000 s).
    add_header Strict-Transport-Security "max-age=31536000" always;

    # Body size must at least match GITHEARTH_GIT_PUSH_LIMIT_BYTES (default 1 GiB)
    client_max_body_size 2048m;

    # Timeouts must at least match GITHEARTH_GIT_TIMEOUT_SECONDS (default 600 s)
    proxy_read_timeout 700s;
    proxy_send_timeout 700s;

    location / {
        proxy_pass http://githearth_backend;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host $host;

        # Streaming request bodies (required for Git push)
        proxy_request_buffering off;

        # No response buffering for git-upload-pack (clone/fetch)
        proxy_buffering off;

    }
}
```

### Verification checklist

1. `curl -k https://127.0.0.1/health/ready` returns 200.
2. Git clone/push over HTTPS works (streaming + body size).
3. `curl -kI https://127.0.0.1/` includes `Strict-Transport-Security`.
4. Plain HTTP to the TLS port is rejected.
5. Login sets a session cookie with `Secure; HttpOnly; SameSite=Lax`.

## Secure cookies

`secure_cookies = true` (the default) adds `Secure` to every session and CSRF
cookie. **Always keep this enabled behind TLS.**

`GITHEARTH_SECURE_COOKIES=false` is only intended for plain-HTTP loopback
development (e.g. local Docker Compose on `127.0.0.1`). Browsers will not send
a `Secure` cookie over plain HTTP.

## Certificate renewal

Renew certificates with your ACME client (certbot, acme.sh, lego) before expiry.
After renewal, reload nginx:

```
systemctl reload nginx
```

GitHearth does not need to be restarted — it never sees the certificate.

## Session and TLS defaults

| Default | Value | Source |
|---------|-------|--------|
| `secure_cookies` | `true` | `config.rs:34` |
| Session cookie name | `githearth_session` | `ui.rs:131` |
| CSRF cookie name | `githearth_csrf` | `ui.rs:335` |
| Cookie `Path` | `/` | `ui.rs:131` |
| Cookie `HttpOnly` | `true` | `ui.rs:131` |
| Cookie `SameSite` | `Lax` | `ui.rs:131` |
| Cookie `Secure` | conditional on `secure_cookies` | `ui.rs:125-128` |
| Cookie `Max-Age` (session) | `token_ttl_seconds` (default 3600) | `ui.rs:132` |
| Cookie `Max-Age` (CSRF) | `token_ttl_seconds` (default 3600) | `ui.rs:335` |
| Session token TTL | 3600 s (1 h) | `config.rs:62` |
| Personal access token expiry | chosen at creation: 1–3650 days, or none | `tokens.rs` `create_token` |
| CSRF token size | 128 bits (16 bytes hex → 32 chars) | `ui.rs:317` |
| Auth concurrency (login throttling) | 2 concurrent argon2 verifications; queue 2 s timeout | `config.rs:64`, `lib.rs:506` |
| `Cache-Control: no-store` | all responses (JSON, UI and Git), set by the `observe` middleware | `lib.rs` `observe` |
| `X-Frame-Options: DENY` | all browser pages | `ui.rs` `page_headers` |
| `X-Content-Type-Options: nosniff` | all responses | `lib.rs` `observe` |
| `Referrer-Policy: same-origin` | all responses | `lib.rs` `observe` |
| `Content-Security-Policy` | all browser pages (`default-src 'none'`, inline styles only, `frame-ancestors 'none'`) | `ui.rs` `PAGE_CSP` |
| `X-Forwarded-For` consumption | trusted proxies only (`trusted_proxies`), used for the audit client IP | `client_ip.rs`, `lib.rs` |
| `X-Forwarded-Proto` / `X-Forwarded-Host` | not consumed; no IP-based rate limiting, cookie-domain filtering or Host validation | `client_ip.rs` |

### Remaining gaps

1. **`X-Forwarded-Proto` / `X-Forwarded-Host`** are not consumed, and there is no
   IP-based rate limiting or Host-header validation. The proxy is expected to
   enforce the public hostname and TLS; set `secure_cookies = true` behind TLS.
2. **HSTS** is set by the reverse proxy, not the app (the app may serve plain
   loopback HTTP). The nginx example below sets it.
