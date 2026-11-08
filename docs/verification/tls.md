# TLS ingress verification

Date: 2026-11-08. State: **the TLS runbook's nginx configuration works end to end** for the UI, the API and Smart HTTP Git. Verified with [`scripts/verify-tls.py`](../../scripts/verify-tls.py) against the native release binary.

The drill found one server bug: an HTTP push behind nginx hung once it exceeded the socket and pipe buffers, and over HTTP/2 it failed with `PROTOCOL_ERROR`. The server answered before reading the pack, and nginx proxies half-duplex. The fix is in PR #85, which this branch builds on.

The harness subclasses the M2 harness in [`scripts/verify-m2.py`](../../scripts/verify-m2.py).
- **It tests the runbook's config verbatim.** It extracts the `nginx` block from [tls.md](../operations/tls.md) and substitutes only host-specific values: the ports, the certificate paths, and the IPv6 listener, which it drops so the drill listens on loopback only. If the runbook changes so that a substitution no longer matches, the drill fails.
- **Certificates are throwaway.** A CA and a server certificate for `127.0.0.1` are generated per run with `openssl`.
- **Secrets stay out of URLs and the repository.** Credentials reach `git` only through a temporary credential helper.
- **Everything is cleaned up.** The nginx container, the PostgreSQL container, the server and the temporary directories are removed on exit, including on failure.

## Environment

| Item | Value |
| --- | --- |
| Source revision | `d1096bc` (main plus the fix in #85) |
| Server | `githearth-server` release build, default configuration (`secure_cookies = true`) |
| Proxy | `nginx:stable` container with `--network host`, HTTP/2 and TLS 1.2/1.3, config from the runbook |
| PostgreSQL | `postgres:17-bookworm` disposable container |
| Git client | git 2.43.0, with `GIT_SSL_CAINFO` pointing at the throwaway CA |
| OS / kernel | Linux 7.0.0-42-generic x86_64, glibc 2.39 |

## Command

| Command | Result |
| --- | --- |
| `python3 scripts/verify-tls.py --report <path>` | **Passed, 4/4 scenarios.** Afterwards no container, volume or server process from the run remained. |

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Fresh install behind TLS | Migrates and bootstraps the server, starts nginx with the runbook config, and waits until `/health/ready` answers over HTTPS. Then it creates an organization and a private repository through the API over HTTPS. | PASS | 1.4 s |
| 2 | Push and fetch over HTTPS | Clones over HTTPS, pushes a commit, then pushes a **20 MiB random-data** commit. The push streams through nginx, and is far larger than `body_limit_bytes` and the socket buffers. Then it fetches the branch back and checks the file. | PASS | 3.6 s |
| 3 | Cookies and HSTS | Logs in through the browser form over HTTPS. The session cookie carries `Secure`, `HttpOnly` and `SameSite=Lax`, and responses carry `Strict-Transport-Security: max-age=31536000`. | PASS | 0.04 s |
| 4 | Plain HTTP refused | A plain-HTTP request to the TLS port fails. | PASS | 0.01 s |

## Not covered

- A publicly trusted certificate, ACME renewal, and OCSP stapling.
- Proxies other than nginx; only the nginx configuration is documented and tested.
- SSH; it is served directly by GitHearth, not proxied, and is covered by the M4 drills.
- The security-header gaps listed in [tls.md](../operations/tls.md#gaps-filed-as-issues): `X-Content-Type-Options`, `Referrer-Policy` and `Content-Security-Policy` are not set. They are filed as #87 and #88 rather than changed here.
