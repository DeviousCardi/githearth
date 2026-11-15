# M4.1 acceptance evidence — U15, U18, U23b

Date: 2026-11-15. Collected by: HavishJadav.

This document records black-box evidence for three M4.1 cases whose code was
already merged. It is not a code summary: every assertion below was produced by
installing the server with `githearth-server setup` and driving the running
product. No file under `docs/plan/` and no code was changed, and
`m4-1-signoff.md` was not written.

## Verdict summary

| Case | Verdict | Clause(s) not demonstrated |
| --- | --- | --- |
| U15 | **pass** | none |
| U18 | **pass** after PR #189 (`e29e57e`); the pre-fix record is kept below | none — the numeric and delay clauses that previously failed now pass |
| U19 | **pass** on three clauses; **one not demonstrated** | pin expiry on the stated retention: the shortest configurable window is 1 day, so expiry was not observed. The 30-day migration backfill against the documented 90-day default is a decision pending. Detail: `m4-1-evidence-u19.md`. |
| U23b | **pass** after PR #189 (`e29e57e`); the pre-fix record is kept below | none as written; the site audit chain is read by SQL because no product API exposes it (the record exists and never contains the credential) |

## Under test

| Item | U15 (original round) | U18 / U23b (re-run after the fix) |
| --- | --- | --- |
| Worktree | `/Users/suniljadav/Aarav/Github/githearth-m41-b10` | `/Users/suniljadav/Aarav/Github/githearth-m41-b13` |
| Branch / commit | `m41/evidence-fresh` at `99a4e05` (clean) | `m41/evidence-ship` at `e29e57e` — the merge of PR #189 (clean) |
| Binary | `target/release/githearth-server` built from that tree (mtime 15:52, commit 15:48) | `target/release/githearth-server` built from `e29e57e` (mtime 17:16) |
| Scratch database | `githearth_b10_evidence` (recreated for each case) | `githearth_b13_u18` and `githearth_b13_u23b` (recreated for each case) |
| PostgreSQL | 17.11 (Homebrew) | 17.11 (Homebrew) |
| Host | macOS 26.6, `git` 2.50.1 (Apple Git-155), OpenSSH 10.3 | same |
| HTTP / SSH | `127.0.0.1:18080` / `127.0.0.1:12222` | `127.0.0.1:18180` / `12322` (U18); `18181` / `12323` (U23b) |

U15 is the original record and was not re-collected. U18 and U23b were re-run on
the fixed tree in this round; their “before” measurements are kept unchanged and
the new output follows each case.

Each case was installed on its own fresh database and server process, because
the sign-in throttle state (`login_attempts`) is shared by every request from
one address and would otherwise contaminate U18. The full transcripts, including
every request and response, are in the reports directory:

```
/Users/suniljadav/Aarav/Github/githearth-m41-reports/
  b10-evidence/                    # U15 (original)
    scripts/lib.py scripts/run.sh scripts/u15.py scripts/u18.py scripts/u23b.py
    scripts/window_poll.py
    u15/logs/{setup.log,u15.log,server.log}
    u18/logs/{setup.log,u18.log,server.log,window-poll.log}
    u23b/logs/{setup.log,u23b.log,server.log}
  b13-evidence/                    # U18 and U23b (after the fix)
    u18.py  u23b.py
    u18.log  u23b.log
    u18/{etc/config.toml,server.log,body,headers}
    u23b/{etc/config.toml,server.log,body,headers}
```

U18's window poll ran against the live server and waited the window out in real
time; its transcript is in `b13-evidence/u18.log`.


Passwords are scratch values read from the environment by the scripts; the
transcript below writes them as `$ADMIN_PW`, `$ALICE_PW`, `$BOB_PW`,
`$THROTTLE_PW`, `$ADMIN2_PW`, `$WINDOW_PW` and `$NEW_ADMIN_PW`.

## Installation (repeated for each case)

The database is recreated, then `setup` reads the database URL from
`GITHEARTH_DATABASE_URL` and the first administrator password from stdin. The
command below is the U15 invocation; U18 and U23b repeated it with their own
directory.

```console
$ psql "$DB_ADMIN/postgres" -c "DROP DATABASE IF EXISTS githearth_b10_evidence" \
    -c "CREATE DATABASE githearth_b10_evidence"
DROP DATABASE
CREATE DATABASE

$ GITHEARTH_DATABASE_URL=postgres://githearth:…@127.0.0.1:57939/githearth_b10_evidence \
  githearth-server setup \
    --config-path  …/b10-evidence/u15/etc/config.toml \
    --http-port 18080 --ssh-port 12222 \
    --storage-root …/b10-evidence/u15/repos \
    --ssh-host-key-path …/b10-evidence/u15/etc/ssh_host_key
created      …/u15/etc/config.toml (mode 0600, database URL from the environment or stdin)
created      …/u15/etc/ssh_host_key (Ed25519)
created      migrated to v54
created      first administrator `admin` (27c1eb5c-e8f2-4444-96ca-6dfe7066fd71)
ok   git: 2.50
ok   storage: …/u15/repos writable
warn disk: 29.97 GiB free
ok   database: connected
ok   schema: latest v54
ok   lifecycle: no stuck repositories
ok   jobs: no dead, late or stuck jobs
ok   outbox: 0 pending, 0 delivering, 0 delivered delivery(ies)
ok   housekeeping: repositories are up to date
9 checks, 0 failed, 1 warnings, 0 skipped
setup complete: start the server with `githearth-server serve --config …/u15/etc/config.toml`

$ githearth-server serve --config …/u15/etc/config.toml     # background; /health/ready 200
```

(Key lines; the complete `setup.log` is in the U15 log directory.)

U18 and U23b produced the same 9-check result; their first administrators were
`70022ec1-ce18-4fed-ae14-02fba08f449f` and
`419653f3-7eb2-492c-a3ee-1df01550acfa`.

The re-run installs on `e29e57e` reported **10 checks, 0 failed, 1 warning**
(the email check is now part of `setup`); their first administrators were
`f8cbc1c3-e106-4311-bf98-f0a47637384e` (U18) and
`c70efc4a-c467-426a-bcbe-445c85bf689d` (U23b).

---

## U15 — disabled account and removed member

> **Case U15.** “A disabled account's and a removed member's existing sessions,
> access tokens and SSH keys are denied on the API and UI as well as on both Git
> transports” — `docs/plan/08-two-person-roadmap.md:119`

### Scenario

`python3 …/b10-evidence/scripts/u15.py`, against the U15 installation. The
script creates the `lab` organization and a private repository `app`, adds
`alice` and `bob` as members with an explicit `read` grant, issues each a session,
a personal access token and an SSH key, and seeds one commit. It then exercises
each credential before and after the account is disabled / the membership is
removed, and again after the cause is removed.

### Baseline (both accounts)

| Surface | Command | Observed |
| --- | --- | --- |
| API | `GET /api/v1/organizations/lab/repositories/app` (session) | `200` |
| UI | `GET /organizations/lab/repositories/app` (session cookie) | `200`, page title `Repository` |
| Git HTTP (password) | `git clone http://alice:$ALICE_PW@…/lab/app.git` | `exit 0` |
| Git HTTP (PAT) | `git clone http://x:$ALICE_PAT@…/lab/app.git` | `exit 0` |
| Git SSH (key) | `git clone ssh://git@…:12222/lab/app.git` | `exit 0` |

### Disabled account — denial

```
$ POST /api/v1/admin/accounts/<alice-id>/disable
HTTP 200
{"id":"…","username":"alice","is_admin":false,"suspended":true,…}

$ GET /api/v1/me                                      (existing session)
HTTP 401  {"error":"invalid or expired credentials"}
$ GET /api/v1/organizations/lab/repositories/app      (existing session)
HTTP 401  {"error":"invalid or expired credentials"}
$ GET /api/v1/me                                      (existing PAT)
HTTP 401  {"error":"invalid or expired credentials"}
$ GET /api/v1/organizations/lab/repositories/app      (existing PAT)
HTTP 401  {"error":"invalid or expired credentials"}
$ GET /organizations/lab/repositories/app             (session cookie)
HTTP 303
location: /login

$ git clone http://alice:$ALICE_PW@…/lab/app.git
fatal: Authentication failed for 'http://127.0.0.1:18080/lab/app.git/'
exit 128
$ git clone http://x:$ALICE_PAT@…/lab/app.git
fatal: Authentication failed for 'http://127.0.0.1:18080/lab/app.git/'
exit 128
$ git clone ssh://git@…:12222/lab/app.git            (SSH key)
git@127.0.0.1: Permission denied (password,publickey,hostbased,keyboard-interactive).
fatal: Could not read from remote repository.
exit 128
```

### Disabled account — positive half

```
$ POST /api/v1/admin/accounts/<alice-id>/enable
HTTP 200  {"username":"alice","suspended":false,…}

$ GET …/repositories/app   (the pre-disable session)
HTTP 401            # disabling deletes the account's sessions: re-enabling starts signed out

$ POST /api/v1/login {"username":"alice","password":$ALICE_PW}
HTTP 200  {"token":"…"}
$ GET …/repositories/app   (fresh session)                 HTTP 200
$ GET /organizations/lab/repositories/app (fresh session)  HTTP 200, title Repository
$ git clone http://alice:$ALICE_PW@…/lab/app.git           exit 0
$ git clone http://x:$ALICE_PAT@…/lab/app.git              exit 0   # same PAT
$ git clone ssh://git@…/lab/app.git                        exit 0   # same SSH key
```

### Removed member — denial

```
$ DELETE /api/v1/organizations/lab/members/bob
HTTP 204

$ GET /api/v1/me                                  (bob session)
HTTP 200  {"id":"…","username":"bob","is_admin":false}
# the account still exists; the organization's resources are what is denied:
$ GET /api/v1/organizations/lab/repositories/app  (bob session)   HTTP 404 {"error":"not found"}
$ GET /api/v1/organizations/lab/repositories/app  (bob PAT)       HTTP 404 {"error":"not found"}
$ GET /organizations/lab/repositories/app         (bob session)   HTTP 404, <title>Not found</title>

$ git clone http://bob:$BOB_PW@…/lab/app.git
fatal: repository 'http://127.0.0.1:18080/lab/app.git/' not found
exit 128
$ git clone http://x:$BOB_PAT@…/lab/app.git
fatal: repository 'http://127.0.0.1:18080/lab/app.git/' not found
exit 128
$ git clone ssh://git@…:12222/lab/app.git         (bob key)
githearth: repository not found
fatal: Could not read from remote repository.
exit 128
```

### Removed member — positive half

```
$ POST /api/v1/organizations/lab/members {"username":"bob","role":"member"}   HTTP 201
$ POST …/repositories/app/access {"subject_kind":"account",…,"role":"read"}   HTTP 201

$ GET /api/v1/organizations/lab/repositories/app   (same bob session)  HTTP 200
$ GET /organizations/lab/repositories/app          (same bob session)  HTTP 200, title Repository
$ git clone http://bob:$BOB_PW@…/lab/app.git        exit 0
$ git clone http://x:$BOB_PAT@…/lab/app.git         exit 0   # same PAT
$ git clone ssh://git@…/lab/app.git                 exit 0   # same SSH key
```

### Audit, through the product API

The organization audit endpoint (not SQL) returned the removal and verified its
chain:

```
$ GET /api/v1/organizations/lab/audit?limit=100
HTTP 200
kinds: organization.created, repository.created, organization.member_added,
       organization.member_added, repository.access_granted,
       repository.access_granted, repository.pushed,
       organization.member_removed, organization.member_added,
       repository.access_granted
$ GET /api/v1/organizations/lab/audit/verify
HTTP 200  {"rows":10,"ok":true,"first_bad_seq":null}
```

(`account.disabled` is a site-chain event; the organization endpoint does not
read that chain — see the U23b audit note.)

### Verdict per clause

| Clause | Verdict | Evidence |
| --- | --- | --- |
| Disabled account's existing **session** denied on the API | **pass** | `/api/v1/me` and repo API `401` |
| Disabled account's existing **access token** denied on the API | **pass** | `/api/v1/me` and repo API `401` |
| Disabled account's existing **SSH key** denied on both transports | **pass** | HTTP password and PAT `exit 128`; SSH `Permission denied (publickey)` `exit 128` |
| Disabled account denied on the **UI** | **pass** | repo page `303 → /login` |
| Removed member's existing **session** denied on the API | **pass** | repo API `404`; `/me` stays `200` because the account still exists (the resource, not the account, is denied) |
| Removed member's **access token** denied on the API | **pass** | repo API `404` |
| Removed member's **SSH key** denied on both transports | **pass** | HTTP `repository … not found`; SSH `repository not found`; both `exit 128` |
| Removed member denied on the **UI** | **pass** | repo page `404`, `Not found` |
| Same action after the cause is removed | **pass** | re-enable and re-add; same PAT/key and a fresh session all work again |

**Overall: PASS.** One semantic point is explicit: removing a membership does
not delete the account, so `/api/v1/me` still returns `200`; access to the
organization and its private repository is refused (`404`). Disabling an
account additionally deletes its sessions, so a re-enabled account is
deliberately signed out and must sign in again; its PAT and SSH key survive.

---

## U18 — login throttling

> **Case U18.** “Login throttling asserts the plan's defaults — 5 failures per
> account per 15 minutes and 20 per address per 15 minutes, with an exponential
> delay after the fifth failure — on the browser, the API and Git HTTP basic
> authentication; token and SSH-key authentication are exempt from the
> per-account ceiling, so an attacker cannot lock an ordinary user out
> indefinitely” — `docs/plan/08-two-person-roadmap.md:122`

> **Status: fixed by PR #189, merged as `e29e57e4aa2d79d5e320130077f042e44a8293b0`.**
> Everything under the next six headings is the original `99a4e05` record of
> what was wrong, kept unchanged. The re-run against the fixed tree, with the
> current verdict per clause, follows that record.

### Before the fix — running configuration

The configuration written by `setup` has no `login_throttle_*` key, so the
running server uses its built-in defaults. Measuring them on the running
product is the point of this section; the values are not read from source.

```console
$ grep -n login_throttle …/u18/etc/config.toml
(no login_throttle key; built-in defaults apply)
```

### Before the fix — setup

`python3 …/scripts/u18.py`: a private repository, a seed commit, and a
`throttle` account with a session, a PAT, a registered SSH key and a `read`
grant. The account was healthy on every surface before the failures:

```
baseline: SSH key reads the repo        exit 0
baseline: PAT reads the repo over HTTP  exit 0
```

### Before the fix — per-account bound

Ten wrong passwords, one request at a time, then the correct password:

| Attempt | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 (correct) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Status | 401 | 401 | 401 | 401 | 401 | 401 | 401 | 401 | 401 | 401 | **429** |
| Wall time (s) | .0165 | .0136 | .0139 | .0136 | .0132 | .0133 | .0139 | .0138 | .0123 | .0122 | 0 ms (server log) |

```
$ POST /api/v1/login {"username":"throttle","password":"wrong-password-1"}   HTTP 401
…
$ POST /api/v1/login {"username":"throttle","password":"wrong-password-10"}  HTTP 401
$ POST /api/v1/login {"username":"throttle","password":$THROTTLE_PW}         HTTP 429
{"error":"too many sign-in attempts; try again later"}
```

The sixth through tenth attempts are still **accepted**. The mean wall time for
attempts 1–5 is 0.0142 s and for attempts 6–10 is 0.0131 s: the delay does not
grow after the fifth failure; there is no delay at all. The eleventh request is
refused before the password is hashed; the server log records `elapsed_ms: 0`
for it, while the accepted attempts each cost about 13 ms of hashing.

### Before the fix — browser and Git HTTP

```
$ curl -sS -D headers -o body http://127.0.0.1:18080/login        # fetches CSRF
$ curl -sS -b 'githearth_csrf=$CSRF' \
    -d 'username=throttle&password=$THROTTLE_PW&csrf_token=$CSRF' \
    -o body -w '%{http_code}' http://127.0.0.1:18080/login
HTTP 429
page contains 'Too many sign-in attempts': True

$ git ls-remote http://throttle:$THROTTLE_PW@…/lab/app.git
fatal: unable to access '…/lab/app.git/': The requested URL returned error: 429
exit 128
```

### Before the fix — token and SSH-key exemption

While the username is throttled for password sign-in:

```
$ GET /api/v1/me                     (existing session)        HTTP 200
$ GET /api/v1/me                     (PAT)                     HTTP 200
$ git ls-remote http://x:$THROTTLE_PAT@…/lab/app.git           exit 0
$ git ls-remote ssh://git@…/lab/app.git   (SSH key)             exit 0
```

### Before the fix — per-address bound

The ten per-account failures above are also ten address failures. Forty further
wrong passwords for forty **distinct unknown usernames** from `127.0.0.1` were
accepted, making fifty recorded address failures; the next one was refused:

```
probe 1: HTTP 401     probe 20: HTTP 401     probe 21: HTTP 401     probe 40: HTTP 401
first 429 at probe 41 (recorded address failures before it: 50)
probes accepted (401): 40   429s: 1
```

So the 20th and 21st recorded failures are both accepted, and the ceiling is
reached at **50**, not 20.

### Before the fix — window (no permanent lockout)

After the last recorded failure, a background poll signed in every 15 s with
the account's correct password and logged the elapsed time:

```console
$ tail …/u18/logs/window-poll.log
elapsed=751s status=429
elapsed=766s status=429
elapsed=781s status=429
elapsed=796s status=429
elapsed=811s status=429
elapsed=826s status=429
elapsed=841s status=429
elapsed=856s status=429
elapsed=871s status=429
elapsed=886s status=429
elapsed=901s status=200
cleared after 901s
```

The poll saw `429` through 886 s and `200` at 901 s. The bound is therefore a
sliding window of about 900 s, not a permanent lock: the account's password
sign-in recovered by itself. (901 s includes the 15 s polling interval and the
few seconds the probe stage took after the ten per-account failures.)

### Before the fix — verdict per clause

| Clause | Verdict | Evidence |
| --- | --- | --- |
| **5** failures per account per 15 minutes | **not demonstrated — contradicted** | 10 failures accepted; the 11th (even with the correct password) is `429`. The running default is 10, not 5. |
| **20** failures per address per 15 minutes | **not demonstrated — contradicted** | 50 failures accepted; the 51st is `429`. The running default is 50, not 20. |
| **Exponential delay after the fifth failure** | **not demonstrated — absent** | attempts 6–10 are accepted with the same ~13 ms latency; no delay grows after the fifth. |
| Asserts the bound on the **browser** | **pass** | form POST `429`, page says “Too many sign-in attempts” |
| Asserts the bound on the **API** | **pass** | JSON login `429` with the documented body |
| Asserts the bound on **Git HTTP basic** | **pass** | `git ls-remote` fails with `The requested URL returned error: 429`, `exit 128` |
| **Token and SSH-key authentication are exempt** from the per-account ceiling | **pass** | session, PAT, PAT-over-HTTP and SSH-key clone all succeed during the password throttle |
| Cannot lock an ordinary user out **indefinitely** | **pass** | credentials still work during the window; the password sign-in clears at the window (below) |
| 15-minute window | **pass** | `429` through 886 s; correct password `200` at 901 s |

**Overall: NOT PASSED as written.** The transport coverage and the
token/SSH exemption pass, but the three numeric/behavioural clauses the case
names — 5 per account, 20 per address, exponential delay after the fifth — do
not match the running product. The shipped defaults are 10 per account and 50
per address in a 900 s window, as documented in
`docs/operations/accounts.md`; the plan's 5/20 wording and the code have
diverged. This is recorded as a finding, not worked around: the evidence above
is the product's actual behaviour with the configuration `setup` wrote.

### Re-run after the fix — `e29e57e`

Collected by `python3 …/b13-evidence/u18.py` on a fresh `githearth_b13_u18`
install with the binary built from `e29e57e`. The whole transcript is
`b13-evidence/u18.log`.

#### Running configuration and the measured defaults

```console
$ grep -n login_throttle …/b13-evidence/u18/etc/config.toml
(no login_throttle key; built-in defaults apply)
```

With no override in the file `setup` wrote, the running product's defaults were
then measured by driving it (below): **5** failures per account, **20** per
address, a **900 s** window, and a delay of `200 ms × 2^(n−5)` capped at **2 s**.
The values are not read from source.

#### The fifth failure, the sixth, and the delay

| Attempt | 1 | 2 | 3 | 4 | 5 | 6 (correct) | 7 (correct) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Status | 401 | 401 | 401 | 401 | 401 | **429** | **429** |
| Wall time (ms) | 17.6 | 18.0 | 16.7 | 16.5 | 16.7 | **209.4** | **215.4** |

```console
$ POST /api/v1/login {"username":"throttle","password":"wrong-password-1"}   HTTP 401
…
$ POST /api/v1/login {"username":"throttle","password":"wrong-password-5"}   HTTP 401
$ POST /api/v1/login {"username":"throttle","password":$THROTTLE_PW}         HTTP 429
{"error":"too many sign-in attempts; try again later"}
```

The fifth failure itself is not delayed (all five are 16.5–18.0 ms). The sixth
is delayed **209.4 ms**, much longer than the first (17.6 ms), and a correct
password pays the same wait inside the window. A blocked attempt does not grow
the count, so the seventh is still the same base step (215.4 ms) rather than a
new power.

#### Per-address bound, growth and the 2 s cap

The five account failures are also five address failures. Fifteen further wrong
passwords for distinct unknown usernames were accepted, making twenty recorded
address failures; the next was refused:

| Recorded address failures | 6 | 7 | 8 | 9 | 10 | 11–20 | 21 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Status | 401 | 401 | 401 | 401 | 401 | 401 | **429** |
| Wall time (ms) | 223.7 | 433.9 | 847.3 | 1646.5 | 2048.1 | 2028–2049 | 2017.1 |

```console
$ # probes 1..15 accepted (401); probe 16 refused
first 429 at probe 16 (recorded failures 20)
$ POST /api/v1/login {"username":"address-probe-16",…}   HTTP 429
```

The steps after the fifth failure are **200, 400, 800, 1600, 2000 ms**, and the
wait then stays at the 2 s cap. The ceiling is reached at **20**, not 50.

#### Browser and Git HTTP basic

```console
$ curl -sS http://127.0.0.1:18180/login                # CSRF found: True
$ curl -sS -b 'githearth_csrf=$CSRF' \
    -d 'username=throttle&password=$THROTTLE_PW&csrf_token=$CSRF' \
    http://127.0.0.1:18180/login
HTTP 429
page contains 'Too many sign-in attempts': True

$ git ls-remote http://throttle:$THROTTLE_PW@…/lab/app.git
fatal: unable to access '…/lab/app.git/': The requested URL returned error: 429
exit 128
```

#### Token and SSH-key exemption

While the username and address are throttled for password sign-in:

```console
$ GET /api/v1/me                            (existing session)   HTTP 200
$ GET /api/v1/me                            (PAT)                HTTP 200
$ git ls-remote http://x:$THROTTLE_PAT@…/lab/app.git             exit 0
$ git ls-remote ssh://git@…:12322/lab/app.git  (SSH key)          exit 0
```

#### Window: waited out in real time

After five fresh failures on a second account, the account's correct password was
polled every 30 s against the same running server:

```console
$ # tail of b13-evidence/u18.log
  elapsed= 816s status=429 wall=223 ms
  elapsed= 846s status=429 wall=223 ms
  elapsed= 877s status=429 wall=222 ms
  elapsed= 907s status=200 wall=38 ms
cleared after 907s
```

The refusal held through 877 s and the same correct password was accepted at
907 s. The 907 s includes the 30 s polling interval; the bound is the 900 s
sliding window, not a permanent lock.

#### Verdict per clause (current tree)

| Clause | Verdict | Evidence |
| --- | --- | --- |
| **5** failures per account per 15 minutes | **pass** | five `401`, the sixth `429`; measured on the running defaults |
| **20** failures per address per 15 minutes | **pass** | 20 recorded failures accepted, the 21st `429` |
| **Exponential delay after the fifth failure** | **pass** | 200, 400, 800, 1600, 2000 ms steps, capped at 2 s; sixth attempt 209.4 ms vs first 17.6 ms |
| Asserts the bound on the **browser** | **pass** | form POST `429`, page says “Too many sign-in attempts” |
| Asserts the bound on the **API** | **pass** | JSON login `429` with the documented body |
| Asserts the bound on **Git HTTP basic** | **pass** | `git ls-remote` fails with `The requested URL returned error: 429`, `exit 128` |
| **Token and SSH-key authentication are exempt** from the per-account ceiling | **pass** | session, PAT, PAT-over-HTTP and SSH-key clone all succeed while the password is `429` |
| Cannot lock an ordinary user out **indefinitely** | **pass** | credentials still work during the window; the password clears at the window |
| 15-minute window | **pass** | `429` through 877 s; correct password `200` at 907 s |

**Overall: PASS after PR #189.** The three clauses that failed on `99a4e05` —
5 per account, 20 per address, an exponential delay after the fifth — now match
the plan on the running configuration, and the transport coverage and the
token/SSH exemption still pass.

---

## U23b — break-glass `reset-admin`

> **Case U23b.** “With the only site administrator unable to sign in (lost
> password, or a lost second factor once M11a ships) and no other administrator
> able to act, `reset-admin` run from the host restores administration, issues a
> single-use credential, revokes the old administrator sessions and is audited”
> — `docs/plan/08-two-person-roadmap.md:129`

> **Status: fixed by PR #189, merged as `e29e57e4aa2d79d5e320130077f042e44a8293b0`.**
> Everything under the next seven headings is the original `99a4e05` record,
> kept unchanged. The re-run against the fixed tree follows it.

### Before the fix — setup and the “before” state

`python3 …/scripts/u23b.py`. One administrator (`admin`) was bootstrapped by
`setup`; the script creates a second account, promotes it to administrator,
signs it in, then disables it. It also issues the first administrator a session,
a PAT and an SSH key. The only enabled administrator is then throttled with ten
wrong passwords, so even the correct password is refused:

```
$ POST /api/v1/admin/accounts {"username":"admin2",…}           HTTP 201
$ POST /api/v1/admin/accounts/<admin2-id>/promote               HTTP 200  is_admin=true
$ POST /api/v1/admin/accounts/<admin2-id>/disable               HTTP 200  suspended=true
$ POST /api/v1/login {"username":"admin2",…}                    HTTP 401
$ GET  /api/v1/me (admin2's old session)                        HTTP 401

admin wrong-password attempt 1 … 10: HTTP 401
admin wrong-password attempt 11:     HTTP 429
$ POST /api/v1/login {"username":"admin","password":$ADMIN_PW}  HTTP 429
{"error":"too many sign-in attempts; try again later"}

$ GET /api/v1/admin/accounts?limit=100    (admin's old session)
HTTP 200
[{"username":"admin","is_admin":true,"suspended":false,…},
 {"username":"admin2","is_admin":true,"suspended":true,…}]
```

So: the only enabled administrator cannot sign in, and the only other
administrator is suspended and cannot act.

### Before the fix — the host command

```
$ printf '%s' "$NEW_ADMIN_PW" | \
    githearth-server --config …/u23b/etc/config.toml reset-admin --username admin
{"timestamp":"2026-11-15T10:36:26.283056Z","level":"INFO",
 "fields":{"message":"administrator reset",
           "id":"419653f3-7eb2-492c-a3ee-1df01550acfa"},
 "target":"githearth_server"}
exit 0

$ githearth-server reset-admin --help
Break-glass: restore an existing account as an enabled site administrator.
The password is read from stdin (never command arguments)

Usage: githearth-server reset-admin [OPTIONS] --username <USERNAME>

Options:
      --config <CONFIG>
      --username <USERNAME>
  -h, --help                 Print help
```

The command takes **no credential to issue**: it takes the operator's chosen
new password on stdin.

### Before the fix — old credentials after the reset

```
$ GET /api/v1/me  (old admin session)   HTTP 401  {"error":"invalid or expired credentials"}
$ GET /api/v1/me  (old admin PAT)       HTTP 401  {"error":"invalid or expired credentials"}
$ git ls-remote ssh://git@…/lab/app.git (old admin SSH key)
git@127.0.0.1: Permission denied (password,publickey,hostbased,keyboard-interactive).
fatal: Could not read from remote repository.
exit 128
```

### Before the fix — administration restored

```
$ POST /api/v1/login {"username":"admin","password":$NEW_ADMIN_PW}
HTTP 200  {"token":"…"}
$ GET /api/v1/me (new session)
HTTP 200  {"id":"…","username":"admin","is_admin":true}
$ POST /api/v1/admin/accounts {"username":"restored-check",…}
HTTP 201
```

### Before the fix — the issued credential

```
$ POST /api/v1/login {"username":"admin","password":$NEW_ADMIN_PW}   # second time
HTTP 200  {"token":"…"}
$ GET /api/v1/admin/accounts?limit=100
… {"username":"admin","must_change_password":false,…} …
```

`reset-admin` did **not** issue a single-use credential. It set the
operator-supplied password directly, cleared `must_change_password`, and the
same password signed in twice. This is the case's finding.

### Before the fix — audit

No product API or page reads the site audit chain (`organization_id IS NULL`).
The organization endpoint sees only that organization's rows:

```
$ GET /api/v1/organizations/lab/audit?limit=100
HTTP 200
kinds: ['organization.created', 'repository.created']
```

The record exists in the site chain, read directly only because the product
offers no API for it:

```
$ psql "$DB" -At -F '|' -c "SELECT seq, kind, actor_id, …, organization_id IS NULL AS site_chain \
    FROM audit_events WHERE kind = 'administrator.reset' ORDER BY seq DESC LIMIT 3"
9|administrator.reset|419653f3-7eb2-492c-a3ee-1df01550acfa|2026-11-15T10:36:26Z|t
```

### Before the fix — verdict per clause

| Clause | Verdict | Evidence |
| --- | --- | --- |
| Only site administrator **unable to sign in** | **pass** | correct password `429`; admin2 suspended and `401` |
| **No other administrator able to act** | **pass** | the only enabled administrator is `admin`; `admin2` is `is_admin=true, suspended=true` and its session is `401` |
| `reset-admin` **run from the host restores administration** | **pass** | exit 0; new password logs in; `is_admin=true`; creates an account `201` |
| **Issues a single-use credential** | **fail — finding** | no credential is issued; the operator supplies the password on stdin and it is reusable (two `200` logins) |
| **Revokes the old administrator sessions** | **pass** | old session `401`; also PAT `401` and SSH key denied |
| **Is audited** | **pass for the record; product-API verification not demonstrated** | `administrator.reset` at site-chain seq 9; no product endpoint exposes the site chain |

**Overall: NOT PASSED as written.** Administration is restored, old sessions
(and tokens/keys) are revoked, and the action is recorded — but the
“single-use credential” clause is not implemented by the host command. The
product's *administrator reset API* does issue a single-use code (it sets
`must_change_password`); the break-glass CLI deliberately does not, and
`docs/plan/04-forge-roadmap.md:130` says “No administrator-chosen temporary
password is permitted.” This is a spec/implementation divergence in
`reset-admin`, recorded rather than worked around.

---

### Re-run after the fix — `e29e57e`

Collected by `python3 …/b13-evidence/u23b.py` on a fresh `githearth_b13_u23b`
install with the binary built from `e29e57e`. The whole transcript is
`b13-evidence/u23b.log`; the printed credential is redacted here.

The “before” state is reproduced: `admin2` is promoted and suspended, and the
only enabled administrator is refused even with the correct password.

```console
$ POST /api/v1/admin/accounts {"username":"admin2",…}           HTTP 201
$ POST /api/v1/admin/accounts/<admin2-id>/promote               HTTP 200  is_admin=true
$ POST /api/v1/admin/accounts/<admin2-id>/disable               HTTP 200  suspended=true
admin wrong-password 1..5:                                      HTTP 401
$ POST /api/v1/login {"username":"admin","password":$ADMIN_PW}  HTTP 429
{"error":"too many sign-in attempts; try again later"}
```

#### The host command now issues a credential

```console
$ githearth-server --config …/u23b/etc/config.toml reset-admin --username admin < /dev/null
exit 0
{"timestamp":"2026-11-15T11:47:07.423383Z","level":"INFO",
 "fields":{"message":"administrator reset","id":"c70efc4a-c467-426a-bcbe-445c85bf689d"},
 "target":"githearth_server"}
single-use credential (shown once): <64 hex, redacted here>
redeem before: 2026-11-15T12:47:07Z
redeem with: POST /api/v1/password-reset/redeem {"username":"admin","code":"<credential>","new_password":"…"} or /reset in the browser
```

The command reads nothing from stdin (it ran with `< /dev/null`), prints one
64-hex-character (256-bit) code and its expiry, and the database stores only its
SHA-256 hash: `encode(token_hash,'hex')` is 64 hex characters and differs from
the printed code. A second `reset-admin` retires the previous unredeemed code —
the earlier code then redeems `401` while the newer one redeems `204`.

#### The old credentials after the reset

```console
$ GET /api/v1/me                            (old admin session)  HTTP 401 {"error":"invalid or expired credentials"}
$ GET /api/v1/me                            (old admin PAT)      HTTP 401 {"error":"invalid or expired credentials"}
$ git ls-remote ssh://git@…:12323/lab/app.git  (old admin key)
git@127.0.0.1: Permission denied (publickey,hostbased,keyboard-interactive).
fatal: Could not read from remote repository.
exit 128
$ POST /api/v1/login {"username":"admin","password":$ADMIN_PW}   HTTP 401
```

`accounts.must_change_password` is `t`, so the account can do nothing but redeem
the code.

#### The credential: issued once, redeemed once, refused on reuse, expiring

```console
$ POST /api/v1/password-reset/redeem {"username":"admin","code":<credential>,…}
HTTP 204
$ POST /api/v1/password-reset/redeem {"username":"admin","code":<credential>,…}   # same code
HTTP 401 {"error":"invalid or expired reset code"}
$ POST /api/v1/login {"username":"admin","password":$NEW_ADMIN_PW}
HTTP 200  {"token":"…"}
$ GET /api/v1/me                     (new session)   HTTP 200  is_admin=true
$ POST /api/v1/admin/accounts {"username":"restored-check",…}    HTTP 201
```

Expiry is 60 minutes (`PASSWORD_RESET_TTL_MINUTES`), so the run aged the scratch
row (`UPDATE password_reset_tokens SET expires_at = clock_timestamp() - interval
'1 minute'`) rather than waiting an hour; the expired code was then refused
`401 {"error":"invalid or expired reset code"}`, and the account stayed locked
(`login` `401`) until a fresh code was redeemed. This is a fixture ageing, not a
faked product response, and the transcript marks it.

The administrator-reset **API** and `reset-admin` use the same
`issue_password_reset_code` and the same redeem endpoint: an API-issued code
redeemed once (`204`), again (`401`), and both issuers left rows in the one
`password_reset_tokens` table (6 rows across the run).

#### Audit

```console
$ psql … SELECT count(*) FROM audit_events WHERE kind='administrator.reset' AND organization_id IS NULL
5
$ psql … SELECT payload::text FROM outbox_events WHERE kind='administrator.reset'
{"actor_id":"c70efc4a-…","account_id":"c70efc4a-…","organization_id":null,"reset_expires_at":"2026-11-15T12:47:07Z"}
… (5 payloads; none contains a credential)
$ GET /api/v1/audit          HTTP 404
$ GET /api/v1/admin/audit    HTTP 404
$ GET /api/v1/organizations/lab/audit?limit=100
HTTP 200  kinds: organization.created, repository.created
```

The action is on the site audit chain, and the record never contains the
credential. As before, no product API exposes the site chain (`/api/v1/audit`
and `/api/v1/admin/audit` are `404`; the organization endpoint shows only its own
rows), so this clause is verified from the chain directly, not through the
product.

#### Verdict per clause (current tree)

| Clause | Verdict | Evidence |
| --- | --- | --- |
| Only site administrator **unable to sign in** | **pass** | correct password `429`; `admin2` suspended |
| **No other administrator able to act** | **pass** | the only enabled administrator is `admin`; `admin2` is `is_admin=true, suspended=true` |
| `reset-admin` **run from the host restores administration** | **pass** | exit 0; code redeemed; `is_admin=true`; creates an account `201` |
| **Issues a single-use credential** | **pass** | 64-hex code printed once from `reset-admin` (stdin closed); only its SHA-256 stored; redeems `204`, reuse `401`; second issue retires the first |
| **Revokes the old administrator sessions** | **pass** | old session `401`, PAT `401`, SSH key `exit 128`, old password `401` |
| **Is audited** | **pass for the record** | 5 `administrator.reset` site-chain rows; outbox payloads carry no credential; no product API exposes the site chain |

**Overall: PASS after PR #189.** The pre-fix finding — “no credential is
issued; the operator supplies a reusable password” — is fixed by the same
single-use credential path the administrator-reset API uses, and the old
sessions, tokens and keys are revoked in the same transaction.

---

## What could not be demonstrated, and findings

The findings recorded on `99a4e05` (kept as the record):

1. **U18 default values.** The plan says 5 per account and 20 per address; the
   running installation refuses at 10 and 50. The 900 s window is the same.
2. **U18 exponential delay.** There is no delay after the fifth failure; the
   sixth through tenth are accepted at the same latency.
3. **U23b single-use credential.** `reset-admin` issues no credential; it sets
   an operator-chosen, reusable password and clears `must_change_password`.
4. **Site audit exposure.** `administrator.reset` is written to the site chain
   but no product API or page reads that chain, so U23b's audit record cannot be
   checked through the product; it was read with `psql`. The organization audit
   API is the only audit surface exposed.

### Status at `e29e57e`

1. **Fixed** by PR #189: the running defaults are 5 per account and 20 per
   address per 900 s, measured through the product in the U18 re-run above.
2. **Fixed** by PR #189: the sixth attempt waits `200 ms × 2^(n−5)`, capped at
   2 s, charged before the password is hashed and without holding the Argon2
   slot; the count growth and the cap are in the U18 re-run above.
3. **Fixed** by PR #189: `reset-admin` issues one 256-bit single-use,
   60-minute credential through the same path as the administrator-reset API,
   and revokes the old sessions, tokens and keys; evidence in the U23b re-run.
4. **Still open, and outside the case's wording.** The site audit chain is still
   not exposed by a product API; the `administrator.reset` rows and outbox
   payloads are read with `psql`. The record exists and carries no credential,
   so U23b's “is audited” clause passes on the record; only a product-level way
   to read the site chain is missing.

No code was changed in either collection round, and nothing under `docs/plan/`
was touched. The failures above were recorded first and fixed by PR #189; this
document keeps both the before and the after.

## Reproduction

Original round (U15, and the pre-fix U18/U23b):

```console
$ export GITHEARTH_TEST_DATABASE_URL=postgres://…/githearth_test   # from …/githearth-m41-reports/env.sh
$ # env.sh supplies the host, user and password; scripts/run.sh hardcodes the scratch
$ # database name githearth_b10_evidence and never writes githearth_test
$ …/b10-evidence/scripts/run.sh u15      # or u23b
$ KEEP_SERVER=1 …/b10-evidence/scripts/run.sh u18   # leaves the server up for the 15-minute poll
```

`run.sh` recreates the scratch database, runs `setup` with a password on stdin,
starts `serve`, runs the case script, and (except for U18, which needs the
server during the window poll) stops the server. The case scripts print every
command and its real output.

Re-run after the fix (`e29e57e`):

```console
$ cargo build --release -p githearth-server
$ …/b13-evidence/u23b.py                 # ~1 min; writes b13-evidence/u23b.log
$ …/b13-evidence/u18.py                  # ~19 min, including the real 900 s poll
```

Both scripts recreate `githearth_b13_u18` / `githearth_b13_u23b`, run `setup`,
start `serve` on 18180/12322 and 18181/12323, and print every request and
response into `b13-evidence/u18.log` and `b13-evidence/u23b.log`. The only
direct SQL is read-only inspection, plus the two marked fixture actions: the
`login_attempts` reset between independent U18 phases, and ageing the reset
code's `expires_at` by one minute so the 60-minute expiry could be observed.

