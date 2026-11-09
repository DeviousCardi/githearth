# TLS ingress runbook

## Architecture

TLS terminates at a reverse proxy on the same host.

- **GitHearth** binds `127.0.0.1:8080` (plain HTTP, loopback only).
- **SSH (2222)** is served directly by GitHearth — not proxied.
- The reverse proxy handles TLS, HSTS, and forwards requests to `127.0.0.1:8080`.
- The app currently does **not** consume `X-Forwarded-For`, `X-Forwarded-Proto`, or
  `X-Forwarded-Host` headers. It performs no IP-based rate limiting, audit
  logging of client IPs, cookie domain filtering, or host header validation.
  These headers are included in the recommended config for future use.

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
| `Cache-Control: no-store` (API) | all JSON + git responses | `lib.rs:347` |
| `Cache-Control: no-store` (UI) | all browser pages | `ui.rs:687` |
| `Cache-Control: no-cache` (Git) | info_refs, upload-pack, receive-pack | `git_http.rs:158,301,517` |
| `X-Frame-Options: DENY` | all browser pages | `ui.rs:688` |
| `X-Content-Type-Options: nosniff` | **missing** | — |
| `Referrer-Policy` | **missing** | — |
| `Content-Security-Policy` | **missing** | listed as future M2 work in `docs/plan/07-qualification-and-decisions.md:72` |
| `X-Forwarded-*` consumption | **none** — no IP-based rate limiting, audit, cookie domain, or host validation | `config.rs`, `lib.rs`, `ui.rs` |

### Gaps (filed as issues)

1. **`X-Content-Type-Options: nosniff`** (#87) — not set on any response.
2. **`Referrer-Policy`** (#87) — not set (suggest `strict-origin-when-cross-origin`).
3. **`Content-Security-Policy`** (#87) — not set. Listed as planned pre-M2 work in
   `docs/plan/07-qualification-and-decisions.md` but never implemented.
4. **No consumption of `X-Forwarded-*` headers** (#88) — client IP is not available
   for audit logs or rate limiting behind a proxy.
