#!/usr/bin/env python3
"""M2 acceptance harness: clean install -> push/clone -> browse -> issues ->
isolation -> crash recovery -> restart -> fresh restore -> measurements.

Python 3 stdlib only, following scripts/verify-foundation.py conventions:
generated secrets are never printed and never written into the repository,
git credentials reach git through a temporary credential helper (never in
URLs), and every disposable container and temp dir is cleaned up.
"""
import argparse
import base64
import html
import json
import os
import platform
import re
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "postgres:17-bookworm"
MAIN_DB = "m2"
RESTORE_DB = "m2restore"


class HarnessError(Exception):
    pass


class Skipped(Exception):
    """Raised by a scenario to signal it should be recorded as SKIP, not FAIL."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Server:
    def __init__(self, proc, port, phase):
        self.proc = proc
        self.port = port
        self.phase = phase

    def rss_kib(self):
        try:
            with open(f"/proc/{self.proc.pid}/status") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        return int(line.split()[1])
        except OSError:
            return None
        return None


class Harness:
    def __init__(self, report_path):
        self.report_path = report_path
        self.name = "githearth-m2-" + uuid.uuid4().hex[:12]
        self.folder = tempfile.TemporaryDirectory(prefix="githearth-m2-")
        self.tmp = Path(self.folder.name)
        self.db_password = secrets.token_hex(32)
        self.admin_username = "m2-admin"
        self.admin_password = secrets.token_urlsafe(32)
        self.stranger_username = "m2-stranger"
        self.env_path = self.tmp / "db.env"
        self.env_path.write_text(
            "POSTGRES_USER=githearth\n"
            "POSTGRES_DB=" + MAIN_DB + "\n"
            "POSTGRES_PASSWORD=" + self.db_password + "\n"
        )
        self.env_path.chmod(0o600)
        self.log_path = self.tmp / "app.log"
        self.log_file = open(self.log_path, "wb")
        self.dump_path = self.tmp / "m2.dump"
        self.storage_tar = self.tmp / "storage.tar"
        self.home = self.tmp / "home"
        self.home.mkdir()
        self.cred_helper = self.tmp / "git-credential-m2"
        self.cred_helper.write_text(
            "#!/bin/sh\n"
            'printf "username=%s\\npassword=%s\\n\\n" "$GITHEARTH_HTTP_USER" "$GITHEARTH_HTTP_PASS"\n'
        )
        self.cred_helper.chmod(0o755)
        self.binary = self.build()
        self.opener = urllib.request.build_opener(NoRedirect)
        self.git_ops = []
        self.scenarios = []
        self.lifetimes = []
        self.failures = []
        self.server = None
        self.http_base = None
        self.storage_root = self.tmp / "storage"
        self.main_work = self.tmp / "work"
        self.clone = self.tmp / "clone"
        self.admin_token = None
        self.measurements = {}

    # ------------------------------------------------------------------ infra

    def build(self):
        subprocess.run(
            ["cargo", "build", "-p", "githearth-server", "--release"],
            cwd=ROOT,
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        target_dir = os.environ.get("CARGO_TARGET_DIR")
        base = Path(target_dir) if target_dir else (ROOT / "target")
        binary = base / "release" / "githearth-server"
        if not binary.exists():
            raise HarnessError("release binary not found after build")
        return str(binary)

    def docker(self, args, **kw):
        return subprocess.run(["docker"] + args, check=True, **kw)

    def sql(self, db, query):
        out = subprocess.run(
            [
                "docker", "exec", "-i", self.name, "psql",
                "-U", "githearth", "-d", db, "-t", "-A", "-v", "ON_ERROR_STOP=1",
            ],
            input=query,
            text=True,
            capture_output=True,
            check=True,
        )
        return out.stdout.strip()

    def free_port(self):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            return sock.getsockname()[1]

    def start_db(self):
        self.db_port = self.free_port()
        self.docker(
            [
                "run", "--detach", "--rm", "--name", self.name,
                "--env-file", str(self.env_path),
                "--publish", f"127.0.0.1:{self.db_port}:5432",
                IMAGE,
            ],
            stdout=subprocess.DEVNULL,
        )
        for _ in range(120):
            if subprocess.run(
                ["docker", "exec", self.name, "pg_isready", "-h", "127.0.0.1",
                 "-U", "githearth", "-d", MAIN_DB],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ).returncode == 0:
                return
            time.sleep(0.5)
        raise HarnessError("PostgreSQL startup timed out")

    def db_url(self, db):
        return f"postgres://githearth:{self.db_password}@127.0.0.1:{self.db_port}/{db}"

    def server_env(self, db, storage_root):
        env = dict(os.environ)
        for key in list(env):
            if key.startswith("GITHEARTH_"):
                del env[key]
        env.update(
            {
                "GITHEARTH_DATABASE_URL": self.db_url(db),
                "GITHEARTH_STORAGE_ROOT": str(storage_root),
                "GITHEARTH_SECURE_COOKIES": "false",
                "GITHEARTH_RECONCILE_INTERVAL_SECONDS": "5",
                "GITHEARTH_RECONCILE_GRACE_SECONDS": "30",
            }
        )
        return env

    def start_server(self, db, storage_root, phase):
        port = self.free_port()
        env = self.server_env(db, storage_root)
        env["GITHEARTH_BIND"] = f"127.0.0.1:{port}"
        proc = subprocess.Popen(
            [self.binary, "serve"], env=env, stdout=self.log_file, stderr=self.log_file
        )
        server = Server(proc, port, phase)
        self.server = server
        self.http_base = f"http://127.0.0.1:{port}"
        self.wait_ready(proc, port, phase)
        return server

    def wait_ready(self, proc, port, phase):
        base = f"http://127.0.0.1:{port}"
        for _ in range(300):
            if proc.poll() is not None:
                raise HarnessError(f"{phase} server exited during startup")
            try:
                status, _, _ = self.api("GET", "/health/ready", base=base)
                if status == 200:
                    return
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.1)
        raise HarnessError(f"{phase} server readiness timed out")

    def api(self, method, path, data=None, token=None, base=None, headers=None, body=None):
        url = (base or self.http_base) + path
        hdrs = {}
        if token is not None:
            hdrs["Authorization"] = "Bearer " + token
        if headers:
            hdrs.update(headers)
        req_data = None
        if body is not None:
            req_data = body
        elif data is not None:
            hdrs["Content-Type"] = "application/json"
            req_data = json.dumps(data).encode()
        req = urllib.request.Request(url, data=req_data, headers=hdrs, method=method)
        try:
            with self.opener.open(req, timeout=60) as response:
                return response.status, response.getheaders(), response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.headers.items(), error.read()

    def api_json(self, method, path, data=None, token=None, base=None, headers=None):
        status, _, body = self.api(method, path, data=data, token=token, base=base, headers=headers)
        parsed = None
        if body:
            try:
                parsed = json.loads(body)
            except ValueError:
                parsed = None
        return status, parsed

    @staticmethod
    def get_cookie(headers, name):
        for key, value in headers:
            if key.lower() == "set-cookie":
                for part in value.split(";"):
                    part = part.strip()
                    if part.startswith(name + "="):
                        return part[len(name) + 1:]
        return None

    # ------------------------------------------------------------------- git

    def git_env(self, user=None, password=None):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_SSL_NO_VERIFY": "1",
            "LC_ALL": "C",
        }
        if user is not None:
            env["GITHEARTH_HTTP_USER"] = user
            env["GITHEARTH_HTTP_PASS"] = password
        return env

    def git(self, *args, user=None, password=None, cwd=None, check=True, label=None, timeout=600):
        cmd = ["git", "-c", f"credential.helper={self.cred_helper}"] + list(args)
        start = time.time()
        out = subprocess.run(
            cmd, cwd=cwd or ROOT, env=self.git_env(user, password),
            capture_output=True, text=True, timeout=timeout,
        )
        self.git_ops.append(
            {"name": label or args[0], "seconds": round(time.time() - start, 3)}
        )
        if check and out.returncode != 0:
            raise HarnessError(f"git {' '.join(args)} failed: {out.stderr.strip()[:400]}")
        return out

    def wgit(self, *args, check=True, label=None):
        return self.git(*args, cwd=str(self.main_work), check=check, label=label)

    def cgit(self, *args, check=True, label=None):
        return self.git(*args, cwd=str(self.clone), check=check, label=label)

    @property
    def repo_url(self):
        return f"http://127.0.0.1:{self.server.port}/m2-org/m2-repo.git"

    def repo_storage_path(self, storage_root=None):
        root = Path(storage_root) if storage_root else self.storage_root
        sid = self.repo_id.replace("-", "")
        return root / "repositories" / sid[:2] / (sid + ".git")

    # ------------------------------------------------------- process handling

    def record_lifetime(self):
        if self.server is not None:
            self.lifetimes.append(
                {"phase": self.server.phase, "peak_rss_kib": self.server.rss_kib()}
            )

    def stop_server(self):
        if self.server is None or self.server.proc.poll() is not None:
            return
        self.record_lifetime()
        self.server.proc.terminate()
        try:
            self.server.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.server.proc.kill()
            self.server.proc.wait()
        self.server = None

    def kill_server(self):
        if self.server is None or self.server.proc.poll() is not None:
            return
        self.record_lifetime()
        self.server.proc.kill()
        try:
            self.server.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            pass
        self.server = None

    def children(self, pid):
        try:
            with open(f"/proc/{pid}/task/{pid}/children") as f:
                return [int(x) for x in f.read().split()]
        except OSError:
            return []

    def find_receive_pack(self):
        if self.server is None:
            return None
        for pid in self.children(self.server.proc.pid):
            try:
                with open(f"/proc/{pid}/cmdline", "rb") as f:
                    cmdline = f.read().replace(b"\0", b" ").decode("utf-8", "replace")
            except OSError:
                continue
            if (
                "receive-pack" in cmdline
                and "--stateless-rpc" in cmdline
                and "--advertise-refs" not in cmdline
            ):
                return pid
        return None

    def wait_journal_gt(self, j0, db=MAIN_DB, repo_id=None):
        repo_id = repo_id or self.repo_id
        for _ in range(400):
            ja = int(
                self.sql(
                    db,
                    "SELECT journal_applied FROM repositories WHERE id='" + repo_id + "'",
                )
            )
            if ja > j0:
                return ja
            time.sleep(0.05)
        raise HarnessError("journal_applied did not advance")

    def journal_applied(self, db=MAIN_DB, repo_id=None):
        repo_id = repo_id or self.repo_id
        return int(
            self.sql(
                db, "SELECT journal_applied FROM repositories WHERE id='" + repo_id + "'"
            )
        )

    # -------------------------------------------------------------- scenarios

    def scenario(self, name, fn):
        start = time.time()
        try:
            fn()
            result = "PASS"
            detail = ""
        except Skipped as error:
            result = "SKIP"
            detail = str(error)
        except HarnessError as error:
            result = "FAIL"
            detail = str(error)
        except Exception as error:  # keep going; report and exit non-zero
            result = "FAIL"
            detail = f"{type(error).__name__}: {error}"
        self.scenarios.append(
            {"name": name, "result": result, "wall_seconds": round(time.time() - start, 3)}
        )
        print(f"{result}: {name}")
        if detail:
            print(f"    {detail}")
        if result == "FAIL":
            self.failures.append(name)

    def s1_fresh_install(self):
        env = self.server_env(MAIN_DB, self.storage_root)
        subprocess.run(
            [self.binary, "migrate"], env=env, check=True,
            capture_output=True, text=True,
        )
        boot = subprocess.run(
            [self.binary, "bootstrap-admin", "--username", self.admin_username],
            env=env, input=self.admin_password, text=True, capture_output=True,
        )
        if boot.returncode != 0:
            raise HarnessError("bootstrap-admin failed: " + boot.stderr.strip()[:300])
        # Second bootstrap is refused; that is why the unrelated user is inserted
        # through SQL below (the path the server tests use).
        again = subprocess.run(
            [self.binary, "bootstrap-admin", "--username", "replacement-admin"],
            env=env, input=secrets.token_urlsafe(32), text=True, capture_output=True,
        )
        if again.returncode == 0 or "already exists" not in again.stderr:
            raise HarnessError("second bootstrap-admin was not refused")

        self.start_server(MAIN_DB, self.storage_root, "initial")

        status, session = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.admin_username, "password": self.admin_password},
        )
        if status != 200 or not session.get("token"):
            raise HarnessError(f"login status {status}")
        self.admin_token = session["token"]

        status, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "m2-org", "name": "M2 Org"}, token=self.admin_token,
        )
        if status != 201:
            raise HarnessError(f"create organization status {status}")
        self.org_id = org["id"]

        status, repo = self.api_json(
            "POST", "/api/v1/organizations/m2-org/repositories",
            {"name": "m2-repo", "visibility": "private"}, token=self.admin_token,
        )
        if status != 201:
            raise HarnessError(f"create repository status {status}")
        if repo.get("visibility") != "private" or repo.get("status") != "active":
            raise HarnessError("repository not private/active: " + json.dumps(repo))
        self.repo_id = repo["id"]

        # Unrelated user: no memberships, no grants. It shares the admin's
        # Argon2 hash so it can authenticate (password == admin password).
        self.sql(
            MAIN_DB,
            "INSERT INTO accounts (id, username, password_hash) "
            "SELECT gen_random_uuid(), '" + self.stranger_username + "', password_hash "
            "FROM accounts WHERE is_admin LIMIT 1;",
        )

    def s2_push_clone_browse(self):
        work = self.main_work
        work.mkdir()
        self.wgit("init", "-b", "main")
        self.wgit("config", "user.name", "M2 Test")
        self.wgit("config", "user.email", "m2@example.com")
        (work / "src" / "lib").mkdir(parents=True)
        (work / "README.md").write_text("# README\n")
        (work / "src" / "lib" / "file.txt").write_text("lib content\n")
        self.wgit("add", "-A")
        self.wgit("commit", "-m", "initial: README and nested file")
        self.sha_a = self.wgit("rev-parse", "HEAD").stdout.strip()

        self.binary_bytes = os.urandom(1024)
        (work / "assets").mkdir()
        (work / "assets" / "blob.bin").write_bytes(self.binary_bytes)
        self.wgit("add", "assets/blob.bin")
        self.wgit("commit", "-m", "add binary file")
        self.sha_b = self.wgit("rev-parse", "HEAD").stdout.strip()

        self.wgit("mv", "README.md", "README2.md")
        self.wgit("commit", "-m", "rename README")
        self.sha_c = self.wgit("rev-parse", "HEAD").stdout.strip()
        self.wgit("tag", "v1")

        self.wgit("checkout", "-b", "topic")
        (work / "topic.txt").write_text("topic\n")
        self.wgit("add", "topic.txt")
        self.wgit("commit", "-m", "topic file")
        self.sha_d = self.wgit("rev-parse", "HEAD").stdout.strip()
        self.wgit("checkout", "main")

        self.git(
            "push", self.repo_url, "main", "topic", "v1", cwd=str(work),
            user=self.admin_username, password=self.admin_password,
            label="push fixture",
        )

        self.git(
            "clone", self.repo_url, str(self.clone),
            user=self.admin_username, password=self.admin_password,
            label="clone back",
        )
        self.cgit("config", "user.name", "M2 Test")
        self.cgit("config", "user.email", "m2@example.com")
        for ref, expected in [("HEAD", self.sha_c), ("origin/topic", self.sha_d), ("v1", self.sha_c)]:
            actual = self.cgit("rev-parse", ref).stdout.strip()
            if actual != expected:
                raise HarnessError(f"clone rev-parse {ref}: {actual} != {expected}")

        base = self.http_base
        repo_api = "/api/v1/organizations/m2-org/repositories/m2-repo"

        status, refs = self.api_json("GET", repo_api + "/refs", token=self.admin_token)
        if status != 200:
            raise HarnessError(f"refs status {status}")
        branches = {b["name"]: b["target"] for b in refs["branches"]}
        tags = {t["name"]: t["target"] for t in refs["tags"]}
        if branches != {"main": self.sha_c, "topic": self.sha_d}:
            raise HarnessError("browse branches mismatch: " + json.dumps(branches))
        if tags != {"v1": self.sha_c}:
            raise HarnessError("browse tags mismatch: " + json.dumps(tags))
        if refs.get("default_branch") != "main":
            raise HarnessError("browse default_branch mismatch")

        status, tree = self.api_json("GET", repo_api + "/tree/main", token=self.admin_token)
        if status != 200:
            raise HarnessError(f"tree status {status}")
        got = {e["name"]: (e["kind"], e["id"]) for e in tree["entries"]}
        expected = {}
        for line in self.cgit("ls-tree", "main").stdout.splitlines():
            meta, name = line.split("\t", 1)
            mode, typ, oid = meta.split()
            if typ == "tree":
                kind = "tree"
            elif typ == "commit":
                kind = "commit"
            elif mode.startswith("12"):
                kind = "symlink"
            else:
                kind = "blob"
            expected[name] = (kind, oid)
        if got != expected:
            raise HarnessError("browse tree mismatch")

        status, tree = self.api_json(
            "GET", repo_api + "/tree/main?path=src%2Flib", token=self.admin_token
        )
        if status != 200 or "file.txt" not in {e["name"] for e in tree["entries"]}:
            raise HarnessError("browse nested tree failed")

        status, blob = self.api_json(
            "GET", repo_api + "/blob/main?path=README2.md", token=self.admin_token
        )
        if status != 200 or blob.get("text") != "# README\n":
            raise HarnessError("browse text blob failed")
        status, blob = self.api_json(
            "GET", repo_api + "/blob/main?path=assets%2Fblob.bin", token=self.admin_token
        )
        if status != 200 or not blob.get("binary"):
            raise HarnessError("browse binary blob not binary")
        if base64.b64decode(blob["base64"]) != self.binary_bytes:
            raise HarnessError("browse binary blob content mismatch")

        commit_ids = []
        cursor = None
        while True:
            q = "limit=2" + (f"&cursor={cursor}" if cursor else "")
            status, page = self.api_json("GET", f"{repo_api}/commits/main?{q}", token=self.admin_token)
            if status != 200:
                raise HarnessError(f"commits status {status}")
            commit_ids.extend(item["id"] for item in page["items"])
            if page.get("next") is None:
                break
            cursor = page["next"]
        expected_ids = self.cgit("log", "--format=%H", "main").stdout.split()
        if commit_ids != expected_ids:
            raise HarnessError("browse commits mismatch")

        status, detail = self.api_json(
            "GET", f"{repo_api}/commit/{self.sha_c}", token=self.admin_token
        )
        if status != 200:
            raise HarnessError(f"commit status {status}")
        files = {(f["status"], f["old_path"], f["new_path"]) for f in detail["files"]}
        diff = self.cgit(
            "diff-tree", "-r", "-M", "--name-status", "-z", "--no-commit-id",
            self.sha_b, self.sha_c,
        ).stdout
        if files != self.parse_name_status(diff):
            raise HarnessError("browse commit diff mismatch: " + json.dumps(sorted(files)))

        status, detail = self.api_json(
            "GET", f"{repo_api}/commit/{self.sha_b}", token=self.admin_token
        )
        binaries = [f for f in detail["files"] if f["new_path"] == "assets/blob.bin"]
        if len(binaries) != 1 or binaries[0]["status"] != "A" or not binaries[0]["binary"]:
            raise HarnessError("browse binary commit diff mismatch")

        # Browser UI: login form, then one tree page containing a file name.
        status, headers, body = self.api("GET", "/login")
        if status != 200 or b"Sign in" not in body:
            raise HarnessError("browser login form not served")
        csrf_cookie = self.get_cookie(headers, "githearth_csrf")
        match = re.search(rb'name="csrf_token" value="([0-9a-f]{32})"', body)
        if not csrf_cookie or not match:
            raise HarnessError("no CSRF token on login form")
        form = urllib.parse.urlencode(
            {
                "username": self.admin_username,
                "password": self.admin_password,
                "csrf_token": match.group(1).decode(),
            }
        ).encode()
        status, headers, _ = self.api(
            "POST", "/login", body=form,
            headers={
                "Content-Type": "application/x-www-form-urlencoded",
                "Cookie": f"githearth_csrf={csrf_cookie}",
            },
        )
        session_cookie = self.get_cookie(headers, "githearth_session")
        if not session_cookie:
            raise HarnessError("browser login did not set a session cookie")
        self.browser_cookie = session_cookie
        status, _, body = self.api(
            "GET", "/organizations/m2-org/repositories/m2-repo/tree/main",
            headers={"Cookie": f"githearth_session={session_cookie}"},
        )
        if status != 200 or b"README2.md" not in body:
            raise HarnessError("browser tree page missing file name")

    @staticmethod
    def parse_name_status(text):
        tokens = text.split("\0")
        if tokens and tokens[-1] == "":
            tokens.pop()
        files = set()
        i = 0
        while i < len(tokens):
            status = tokens[i]
            if status.startswith("R") or status.startswith("C"):
                files.add((status, tokens[i + 1], tokens[i + 2]))
                i += 3
            else:
                files.add((status, tokens[i + 1], tokens[i + 1]))
                i += 2
        return files

    def s3_issues_comments(self):
        tok = self.admin_token
        repo_path = "/api/v1/organizations/m2-org/repositories/m2-repo"

        body1 = {"title": "First issue", "body": "Hello"}
        status, issue = self.api_json(
            "POST", repo_path + "/issues", data=body1, token=tok,
            headers={"Idempotency-Key": "m2-key-1"},
        )
        status2, issue2 = self.api_json(
            "POST", repo_path + "/issues", data=body1, token=tok,
            headers={"Idempotency-Key": "m2-key-1"},
        )
        if status != 201 or status2 != status or issue != issue2:
            raise HarnessError("issue idempotent retry not identical")
        self.first_issue_number = issue["number"]
        self.first_issue_id = issue["id"]
        self.issue_numbers = [issue["number"]]
        self.issue_titles = ["First issue"]

        count = int(self.sql(MAIN_DB, "SELECT count(*) FROM issues WHERE repository_id='" + self.repo_id + "'"))
        if count != 1:
            raise HarnessError(f"issue count after retry {count}")
        outbox = int(self.sql(MAIN_DB, "SELECT count(*) FROM outbox_events WHERE kind='issue.created'"))
        audit = int(self.sql(MAIN_DB, "SELECT count(*) FROM audit_events WHERE kind='issue.created'"))
        if outbox != 1 or audit != 1:
            raise HarnessError(f"issue outbox/audit duplicated: {outbox}/{audit}")

        cbody = {"body": "first comment"}
        status, comment = self.api_json(
            "POST", f"{repo_path}/issues/{self.first_issue_number}/comments",
            data=cbody, token=tok, headers={"Idempotency-Key": "m2-key-c1"},
        )
        status2, comment2 = self.api_json(
            "POST", f"{repo_path}/issues/{self.first_issue_number}/comments",
            data=cbody, token=tok, headers={"Idempotency-Key": "m2-key-c1"},
        )
        if status != 201 or status2 != status or comment != comment2:
            raise HarnessError("comment idempotent retry not identical")
        ccount = int(self.sql(
            MAIN_DB,
            "SELECT count(*) FROM issue_comments WHERE issue_id='" + self.first_issue_id + "'",
        ))
        if ccount != 1:
            raise HarnessError(f"comment count after retry {ccount}")
        coutbox = int(self.sql(MAIN_DB, "SELECT count(*) FROM outbox_events WHERE kind='issue.commented'"))
        caudit = int(self.sql(MAIN_DB, "SELECT count(*) FROM audit_events WHERE kind='issue.commented'"))
        if coutbox != 1 or caudit != 1:
            raise HarnessError(f"comment outbox/audit duplicated: {coutbox}/{caudit}")

        for i in (2, 3):
            status, more = self.api_json(
                "POST", repo_path + "/issues", data={"title": f"Issue {i}", "body": ""}, token=tok,
            )
            if status != 201:
                raise HarnessError(f"create issue {i} status {status}")
            self.issue_numbers.append(more["number"])
            self.issue_titles.append(f"Issue {i}")
        for i in (2, 3):
            status, _ = self.api_json(
                "POST", f"{repo_path}/issues/{self.first_issue_number}/comments",
                data={"body": f"comment {i}"}, token=tok,
            )
            if status != 201:
                raise HarnessError(f"create comment {i} status {status}")

        got = []
        cursor = None
        while True:
            q = "limit=2" + (f"&after={cursor}" if cursor else "")
            status, page = self.api_json("GET", f"{repo_path}/issues?{q}", token=tok)
            if status != 200:
                raise HarnessError(f"issue list status {status}")
            got.extend(page["items"])
            if page.get("next") is None:
                break
            cursor = page["next"]
        if sorted(i["number"] for i in got) != sorted(self.issue_numbers):
            raise HarnessError("issue pagination mismatch")

        got = []
        cursor = None
        while True:
            q = "limit=2" + (f"&after={cursor}" if cursor else "")
            status, page = self.api_json(
                "GET", f"{repo_path}/issues/{self.first_issue_number}/comments?{q}", token=tok
            )
            if status != 200:
                raise HarnessError(f"comment list status {status}")
            got.extend(page["items"])
            if page.get("next") is None:
                break
            cursor = page["next"]
        if len(got) != 3:
            raise HarnessError("comment pagination mismatch")

        self.s3_issue_pages()

    def s3_issue_pages(self):
        """Browser pages: list, create, view and comment through the HTML forms."""
        ui = "/organizations/m2-org/repositories/m2-repo/issues"
        cookie = {"Cookie": f"githearth_session={self.browser_cookie}"}
        form_headers = dict(cookie, **{"Content-Type": "application/x-www-form-urlencoded"})

        def page(path):
            status, _, body = self.api("GET", path, headers=cookie)
            if status != 200:
                raise HarnessError(f"issue page {path} status {status}")
            match = re.search(rb'name="csrf_token" value="([0-9a-f]{32})"', body)
            key = re.search(rb'name="idempotency_key" value="([0-9a-f]{32})"', body)
            if not match or not key:
                raise HarnessError(f"no CSRF token or idempotency key on {path}")
            return body, match.group(1).decode(), key.group(1).decode()

        status, _, body = self.api("GET", ui, headers=cookie)
        if status != 200:
            raise HarnessError(f"issue list page status {status}")
        for title in self.issue_titles:
            if html.escape(title).encode() not in body:
                raise HarnessError(f"issue list page missing {title!r}")
        status, _, _ = self.api("GET", ui)
        if status != 303:
            raise HarnessError(f"issue list without a session status {status}")

        _, csrf, key = page(ui + "/new")
        status, _, _ = self.api(
            "POST", ui, headers=form_headers,
            body=urllib.parse.urlencode({"csrf_token": "0" * 32, "title": "forged"}).encode(),
        )
        if status != 403:
            raise HarnessError(f"issue form with a bad CSRF token status {status}")
        title = "UI issue <b>escaped</b>"
        issue_form = urllib.parse.urlencode(
            {"csrf_token": csrf, "idempotency_key": key, "title": title, "body": "made in the browser"}
        ).encode()
        locations = []
        for _ in range(2):  # a double submit must replay, not duplicate
            status, headers, _ = self.api("POST", ui, headers=form_headers, body=issue_form)
            location = dict((k.lower(), v) for k, v in headers).get("location", "")
            if status != 303 or not location.startswith(ui + "/"):
                raise HarnessError(f"issue form status {status} location {location!r}")
            locations.append(location)
        if locations[0] != locations[1]:
            raise HarnessError(f"resubmitted issue form went to {locations}")
        number = int(location.rsplit("/", 1)[1])
        self.issue_numbers.append(number)
        self.issue_titles.append(title)

        body, csrf, key = page(location)
        if html.escape(title).encode() not in body or b"<b>escaped</b>" in body:
            raise HarnessError("issue page does not show the escaped title")
        comment_form = urllib.parse.urlencode(
            {"csrf_token": csrf, "idempotency_key": key, "body": "browser comment"}
        ).encode()
        for _ in range(2):
            status, _, _ = self.api(
                "POST", location + "/comments", headers=form_headers, body=comment_form
            )
            if status != 303:
                raise HarnessError(f"comment form status {status}")
        body, _, _ = page(location)
        if b"browser comment" not in body:
            raise HarnessError("issue page missing the new comment")
        status, comments = self.api_json(
            "GET",
            f"/api/v1/organizations/m2-org/repositories/m2-repo/issues/{number}/comments",
            token=self.admin_token,
        )
        if status != 200 or [c["body"] for c in comments["items"]] != ["browser comment"]:
            raise HarnessError("API does not see exactly one browser comment")
        created = int(self.sql(
            MAIN_DB, "SELECT count(*) FROM issues WHERE title='" + title.replace("'", "''") + "'"
        ))
        if created != 1:
            raise HarnessError(f"resubmitted issue form created {created} issues")

    def s4_isolation_archive(self):
        repo_path = "/api/v1/organizations/m2-org/repositories/m2-repo"
        status, session = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.stranger_username, "password": self.admin_password},
        )
        if status != 200:
            raise HarnessError(f"stranger login status {status}")
        self.stranger_token = session["token"]

        def basic(user, pw):
            return "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()

        status, headers, _ = self.api(
            "GET", "/m2-org/m2-repo.git/info/refs?service=git-upload-pack"
        )
        if status != 401 or not any(k.lower() == "www-authenticate" for k, _ in headers):
            raise HarnessError(f"unauthenticated git status {status}")

        for service in ("git-upload-pack", "git-receive-pack"):
            status, _, _ = self.api(
                "GET", f"/m2-org/m2-repo.git/info/refs?service={service}",
                headers={"Authorization": basic(self.stranger_username, self.admin_password)},
            )
            if status != 404:
                raise HarnessError(f"stranger git {service} status {status}")

        for path in (
            "/api/v1/organizations/" + self.org_id,
            "/api/v1/organizations/m2-org/repositories",
            repo_path,
            repo_path + "/refs",
            repo_path + "/issues",
        ):
            status, _ = self.api_json("GET", path, token=self.stranger_token)
            if status != 404:
                raise HarnessError(f"stranger {path} status {status}")

        out = self.git(
            "clone", self.repo_url, str(self.tmp / "stranger-clone"),
            user=self.stranger_username, password=self.admin_password, check=False,
        )
        if out.returncode == 0:
            raise HarnessError("stranger clone unexpectedly succeeded")

        status, repo = self.api_json("POST", repo_path + "/archive", token=self.admin_token)
        if status != 200 or not repo.get("archived"):
            raise HarnessError("archive failed")

        self.cgit("checkout", "-b", "archtest")
        (self.clone / "arch.txt").write_text("x\n")
        self.cgit("add", "arch.txt")
        self.cgit("commit", "-m", "archive probe")
        out = self.git(
            "push", self.repo_url, "archtest", cwd=str(self.clone),
            user=self.admin_username, password=self.admin_password, check=False,
        )
        if out.returncode == 0 or "permission denied" not in out.stderr:
            raise HarnessError("push to archived repository not refused")

        self.git(
            "clone", self.repo_url, str(self.tmp / "archive-clone"),
            user=self.admin_username, password=self.admin_password, label="archive clone",
        )

        status, repo = self.api_json("POST", repo_path + "/unarchive", token=self.admin_token)
        if status != 200 or repo.get("archived"):
            raise HarnessError("unarchive failed")

    def s5_crash_recovery(self):
        self.s5a_crash()
        self.s5b_lost_metadata()

    def s5a_crash(self):
        self.cgit("checkout", "-b", "big")
        big = self.clone / "big.bin"
        big.write_bytes(os.urandom(50 * 1024 * 1024))
        self.cgit("add", "big.bin")
        self.cgit("commit", "-m", "big commit")
        self.big_sha = self.cgit("rev-parse", "HEAD").stdout.strip()

        before = int(
            self.sql(MAIN_DB, "SELECT count(*) FROM repository_pushes WHERE repository_id='" + self.repo_id + "'")
        )

        push = subprocess.Popen(
            ["git", "-c", f"credential.helper={self.cred_helper}", "push", self.repo_url, "big"],
            cwd=str(self.clone),
            env=self.git_env(self.admin_username, self.admin_password),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.time() + 120
        while time.time() < deadline and push.poll() is None:
            child = self.find_receive_pack()
            if child is not None:
                try:
                    os.kill(child, signal.SIGKILL)
                except OSError:
                    pass
                break
            time.sleep(0.005)
        self.kill_server()
        try:
            push.wait(timeout=60)
        except subprocess.TimeoutExpired:
            push.kill()
            push.wait()
        push.stdout.read()
        push.stderr.read()

        self.start_server(MAIN_DB, self.storage_root, "after-crash")
        rp = str(self.repo_storage_path())
        out = self.git(
            "--git-dir", rp, "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads/big",
            check=True,
        ).stdout.strip()
        branch_exists = bool(out)
        expected_delta = 1 if branch_exists else 0
        for _ in range(300):
            after = int(
                self.sql(MAIN_DB, "SELECT count(*) FROM repository_pushes WHERE repository_id='" + self.repo_id + "'")
            )
            if after - before == expected_delta:
                break
            time.sleep(0.1)
        after = int(
            self.sql(MAIN_DB, "SELECT count(*) FROM repository_pushes WHERE repository_id='" + self.repo_id + "'")
        )
        if after - before != expected_delta:
            raise HarnessError(f"push metadata delta {after - before}, expected {expected_delta}")
        if branch_exists:
            oid = out.split()[1]
            if oid != self.big_sha:
                raise HarnessError("big branch moved to an unexpected oid")
            updates = int(
                self.sql(
                    MAIN_DB,
                    "SELECT count(*) FROM ref_updates WHERE repository_id='" + self.repo_id
                    + "' AND ref_name='refs/heads/big' AND new_oid='" + self.big_sha + "'",
                )
            )
            if updates != 1:
                raise HarnessError(f"big branch ref_updates rows {updates}")

        self.cgit("checkout", "main")
        (self.clone / "after-crash.txt").write_text("ok\n")
        self.cgit("add", "after-crash.txt")
        self.cgit("commit", "-m", "normal after crash")
        self.sha_e = self.cgit("rev-parse", "HEAD").stdout.strip()
        self.git(
            "push", self.repo_url, "main", cwd=str(self.clone),
            user=self.admin_username, password=self.admin_password, label="push after crash",
        )

    def s5b_lost_metadata(self):
        self.cgit("checkout", "main")
        (self.clone / "lost.txt").write_text("lost\n")
        self.cgit("add", "lost.txt")
        self.cgit("commit", "-m", "lost metadata push")
        self.sha_lost = self.cgit("rev-parse", "HEAD").stdout.strip()
        j0 = self.journal_applied()
        self.git(
            "push", self.repo_url, "main", cwd=str(self.clone),
            user=self.admin_username, password=self.admin_password, label="push for lost-metadata",
        )
        j1 = self.wait_journal_gt(j0)

        self.stop_server()
        self.sql(
            MAIN_DB,
            "DELETE FROM ref_updates WHERE repository_id='" + self.repo_id
            + "' AND journal_sequence > " + str(j0) + ";",
        )
        self.sql(
            MAIN_DB,
            "DELETE FROM repository_pushes WHERE repository_id='" + self.repo_id
            + "' AND journal_sequence > " + str(j0) + ";",
        )
        self.sql(
            MAIN_DB,
            "UPDATE repositories SET journal_applied = " + str(j0)
            + " WHERE id='" + self.repo_id + "';",
        )

        self.start_server(MAIN_DB, self.storage_root, "after-lost-metadata")
        for _ in range(400):
            if self.journal_applied() >= j1:
                break
            time.sleep(0.05)
        if self.journal_applied() < j1:
            raise HarnessError("journal_applied was not reapplied")
        pushes = int(
            self.sql(
                MAIN_DB,
                "SELECT count(*) FROM repository_pushes WHERE repository_id='" + self.repo_id
                + "' AND journal_sequence > " + str(j0),
            )
        )
        if pushes != 1:
            raise HarnessError(f"reapplied push rows {pushes}, expected exactly 1")
        refs = int(
            self.sql(
                MAIN_DB,
                "SELECT count(*) FROM ref_updates WHERE repository_id='" + self.repo_id
                + "' AND journal_sequence > " + str(j0),
            )
        )
        if refs != 1:
            raise HarnessError(f"reapplied ref_updates rows {refs}, expected exactly 1")

    def s6_restart(self):
        self.stop_server()
        self.start_server(MAIN_DB, self.storage_root, "after-restart")
        tok = self.admin_token
        status, me = self.api_json("GET", "/api/v1/me", token=tok)
        if status != 200 or me.get("username") != self.admin_username:
            raise HarnessError(f"me after restart status {status}")
        status, _ = self.api_json("GET", "/api/v1/organizations/" + self.org_id, token=tok)
        if status != 200:
            raise HarnessError("organization unreadable after restart")
        status, repo = self.api_json(
            "GET", "/api/v1/organizations/m2-org/repositories/m2-repo", token=tok
        )
        if status != 200 or repo["id"] != self.repo_id:
            raise HarnessError("repository unreadable after restart")
        status, refs = self.api_json(
            "GET", "/api/v1/organizations/m2-org/repositories/m2-repo/refs", token=tok
        )
        if status != 200 or "main" not in {b["name"] for b in refs["branches"]}:
            raise HarnessError("refs unreadable after restart")
        status, issues = self.api_json(
            "GET", "/api/v1/organizations/m2-org/repositories/m2-repo/issues", token=tok
        )
        if status != 200 or len(issues["items"]) != len(self.issue_numbers):
            raise HarnessError("issues unreadable after restart")
        out = self.git(
            "ls-remote", self.repo_url, cwd=str(ROOT),
            user=self.admin_username, password=self.admin_password, label="ls-remote restart",
        )
        if "refs/heads/main" not in out.stdout:
            raise HarnessError("git ls-remote missing main after restart")

    def s7_fresh_restore(self):
        with open(self.dump_path, "wb") as dump:
            subprocess.run(
                ["docker", "exec", self.name, "pg_dump", "-U", "githearth", "-d", MAIN_DB, "-Fc"],
                check=True, stdout=dump,
            )

        self.cgit("checkout", "main")
        (self.clone / "restore.txt").write_text("after dump\n")
        self.cgit("add", "restore.txt")
        self.cgit("commit", "-m", "commit after dump")
        self.sha_f = self.cgit("rev-parse", "HEAD").stdout.strip()
        j0 = self.journal_applied()
        self.git(
            "push", self.repo_url, "main", cwd=str(self.clone),
            user=self.admin_username, password=self.admin_password, label="push after dump",
        )
        self.wait_journal_gt(j0)

        status, repo2 = self.api_json(
            "POST", "/api/v1/organizations/m2-org/repositories",
            {"name": "m2-repo2", "visibility": "private"}, token=self.admin_token,
        )
        if status != 201:
            raise HarnessError(f"create repo2 status {status}")
        self.repo2_id = repo2["id"]

        subprocess.run(
            ["tar", "-C", str(self.storage_root), "--exclude=run", "-cf", str(self.storage_tar), "."], check=True
        )

        self.docker(["exec", self.name, "createdb", "-U", "githearth", RESTORE_DB], stdout=subprocess.DEVNULL)
        with open(self.dump_path, "rb") as dump:
            subprocess.run(
                ["docker", "exec", "-i", self.name, "pg_restore", "-U", "githearth",
                 "--no-owner", "-d", RESTORE_DB],
                check=True, stdin=dump,
            )

        new_storage = self.tmp / "storage-restore"
        new_storage.mkdir()
        subprocess.run(
            ["tar", "-C", str(new_storage), "-xf", str(self.storage_tar)], check=True
        )

        self.stop_server()
        self.start_server(RESTORE_DB, new_storage, "restored")

        for _ in range(400):
            reapplied = int(
                self.sql(
                    RESTORE_DB,
                    "SELECT count(*) FROM ref_updates WHERE repository_id='" + self.repo_id
                    + "' AND ref_name='refs/heads/main' AND new_oid='" + self.sha_f + "'",
                )
            )
            if reapplied >= 1:
                break
            time.sleep(0.1)
        else:
            raise HarnessError("later push was not reapplied from the journal")

        sid = self.repo2_id.replace("-", "")
        for _ in range(400):
            in_repos = (new_storage / "repositories" / sid[:2] / (sid + ".git")).exists()
            orphans = new_storage / "orphans"
            in_orphans = (
                orphans.exists() and any(p.name.startswith(sid) for p in orphans.iterdir())
            )
            if not in_repos and in_orphans:
                break
            time.sleep(0.1)
        else:
            raise HarnessError("repository created after the dump was not orphaned")

        status, session = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.admin_username, "password": self.admin_password},
        )
        if status != 200:
            raise HarnessError(f"restored login status {status}")
        self.restore_token = session["token"]

        status, _ = self.api_json(
            "GET", "/api/v1/organizations/m2-org/repositories/m2-repo2", token=self.restore_token
        )
        if status != 404:
            raise HarnessError(f"repo2 served after restore: {status}")

        restore_clone = self.tmp / "restore-clone"
        self.git(
            "clone", self.repo_url, str(restore_clone),
            user=self.admin_username, password=self.admin_password, label="restore clone",
        )
        for ref, expected in [("HEAD", self.sha_f), ("origin/topic", self.sha_d), ("v1", self.sha_c)]:
            actual = self.git("rev-parse", ref, cwd=str(restore_clone)).stdout.strip()
            if actual != expected:
                raise HarnessError(f"restore clone rev-parse {ref}: {actual} != {expected}")

        repo_api = "/api/v1/organizations/m2-org/repositories/m2-repo"
        status, refs = self.api_json("GET", repo_api + "/refs", token=self.restore_token)
        branches = {b["name"]: b["target"] for b in refs["branches"]}
        tags = {t["name"]: t["target"] for t in refs["tags"]}
        if branches.get("main") != self.sha_f or branches.get("topic") != self.sha_d:
            raise HarnessError("restore browse branches mismatch")
        if tags.get("v1") != self.sha_c:
            raise HarnessError("restore browse tags mismatch")

        status, page = self.api_json("GET", repo_api + "/issues?limit=100", token=self.restore_token)
        if status != 200 or sorted(i["title"] for i in page["items"]) != sorted(self.issue_titles):
            raise HarnessError("restore issues mismatch")
        status, page = self.api_json(
            "GET", f"{repo_api}/issues/{self.first_issue_number}/comments?limit=100",
            token=self.restore_token,
        )
        if status != 200 or len(page["items"]) != 3:
            raise HarnessError("restore comments mismatch")

        status, session = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.stranger_username, "password": self.admin_password},
        )
        status, _ = self.api_json("GET", repo_api, token=session["token"])
        if status != 404:
            raise HarnessError(f"restore stranger status {status}")

    def dir_size(self, path):
        try:
            out = subprocess.check_output(
                ["du", "-sb", str(path)], text=True, stderr=subprocess.DEVNULL
            )
            return int(out.split()[0])
        except Exception:
            return None

    def docker_stats(self):
        try:
            out = subprocess.check_output(
                ["docker", "stats", "--no-stream", "--format", "{{json .}}", self.name],
                text=True,
            )
            return json.loads(out.strip().splitlines()[0])
        except Exception as error:
            return {"unavailable": str(error)}

    def s8_measurements(self):
        environment = {}
        environment["git_version"] = self.git("--version").stdout.strip()
        environment["postgres_version"] = self.sql(MAIN_DB, "SHOW server_version;")
        environment["source_revision"] = self.git("rev-parse", "HEAD").stdout.strip()
        environment["os"] = platform.platform() + " " + platform.machine()
        try:
            environment["uname"] = os.uname().release
        except AttributeError:
            pass
        environment["cpu_count"] = os.cpu_count()
        try:
            environment["docker_version"] = subprocess.check_output(["docker", "--version"], text=True).strip()
        except Exception as error:
            environment["docker_version"] = "unavailable: " + str(error)

        m = {}
        try:
            m["database_size_bytes"] = int(self.sql(MAIN_DB, "SELECT pg_database_size('" + MAIN_DB + "')"))
        except Exception as error:
            m["database_size_bytes"] = "unavailable: " + str(error)
        try:
            m["restore_database_size_bytes"] = int(self.sql(RESTORE_DB, "SELECT pg_database_size('" + RESTORE_DB + "')"))
        except Exception as error:
            m["restore_database_size_bytes"] = "unavailable: " + str(error)
        m["storage_size_bytes"] = self.dir_size(self.storage_root)
        m["restore_storage_size_bytes"] = self.dir_size(self.tmp / "storage-restore")
        m["container_memory"] = self.docker_stats()

        self.stop_server()

        leaked = []
        text = self.log_path.read_text(errors="replace")
        for label, secret in (
            ("db_password", self.db_password),
            ("admin_password", self.admin_password),
            ("admin_token", self.admin_token or ""),
            ("stranger_token", getattr(self, "stranger_token", "") or ""),
            ("restore_token", getattr(self, "restore_token", "") or ""),
        ):
            if secret and secret in text:
                leaked.append(label)
        if leaked:
            raise HarnessError("secrets leaked into server log: " + ", ".join(leaked))

        self.environment = environment
        self.measurements = m

    # ------------------------------------------------------------------ run

    def run(self):
        try:
            self.start_db()
            self.scenario("fresh_install", self.s1_fresh_install)
            self.scenario("push_clone_browse", self.s2_push_clone_browse)
            self.scenario("issues_comments", self.s3_issues_comments)
            self.scenario("isolation_archive", self.s4_isolation_archive)
            self.scenario("crash_recovery", self.s5_crash_recovery)
            self.scenario("restart", self.s6_restart)
            self.scenario("fresh_restore", self.s7_fresh_restore)
            self.scenario("measurements", self.s8_measurements)
        finally:
            self.cleanup()
        self.write_report()
        passed = sum(1 for s in self.scenarios if s["result"] == "PASS")
        total = len(self.scenarios)
        print(f"M2 verification: {passed}/{total} scenarios passed")
        print(f"report: {self.report_path}")
        return 0 if passed == total else 1

    def write_report(self):
        report = {
            "environment": getattr(self, "environment", {}),
            "scenarios": self.scenarios,
            "git_operations": self.git_ops,
            "server_lifetimes": self.lifetimes,
            "measurements": self.measurements,
        }
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)

    def cleanup(self):
        if self.server is not None and self.server.proc.poll() is None:
            self.record_lifetime()
            try:
                self.server.proc.kill()
                self.server.proc.wait(timeout=20)
            except Exception:
                pass
            self.server = None
        subprocess.run(
            ["docker", "rm", "--force", "--volumes", self.name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.log_file.close()
        self.folder.cleanup()


def main():
    parser = argparse.ArgumentParser(description="Verify the M2 workflow end to end.")
    parser.add_argument("--report", help="write measurements JSON to this path")
    args = parser.parse_args()
    report_path = args.report
    if report_path is None:
        fd, report_path = tempfile.mkstemp(prefix="m2-report-", suffix=".json")
        os.close(fd)
    harness = Harness(report_path)
    sys.exit(harness.run())


if __name__ == "__main__":
    main()
