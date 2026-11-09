#!/usr/bin/env python3
"""M4 qualification harness: install from runbook, HTTP/SSH agreement, quota,
backup-restore, upgrade drill, restart+jobs.

Reuses verify-m2.Harness via importlib subclassing. Python 3 stdlib only.
"""
import argparse
import importlib.util
import json
import os
import platform
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "postgres:17-bookworm"
MAIN_DB = "m4"
RESTORE_DB = "m4restore"
UPGRADE_FROM_REV = "9b4a3724323616a26da0ecaeca67a8fac482a21c"


# ---- Load verify-m2 and subclass its Harness ------------------------------

def _load_m2():
    spec = importlib.util.spec_from_file_location(
        "verify_m2", ROOT / "scripts" / "verify-m2.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

_m2 = _load_m2()

# s2: (actor, repository state, operation) -> allowed. Anything absent is denied
# (the non-member and the suspended account are denied everywhere).
EXPECTED_ACCESS = {
    ("owner", "active", "clone"): True, ("owner", "active", "push"): True,
    ("owner", "archived", "clone"): True, ("owner", "archived", "push"): False,
    ("reader", "active", "clone"): True, ("reader", "active", "push"): False,
    ("reader", "archived", "clone"): True, ("reader", "archived", "push"): False,
}
HarnessError = _m2.HarnessError


class M4Server:
    """Like _m2.Server but tracks ssh_port separately so M4 can use it."""
    __slots__ = ("proc", "http_port", "ssh_port", "phase")
    def __init__(self, proc, http_port, ssh_port, phase):
        self.proc = proc; self.http_port = http_port
        self.ssh_port = ssh_port; self.phase = phase
    def rss_kib(self):
        try:
            with open(f"/proc/{self.proc.pid}/status") as f:
                for line in f:
                    if line.startswith("VmHWM:"): return int(line.split()[1])
        except OSError: pass
        return None
    @property
    def port(self): return self.http_port  # satisfy _m2.Server shape


class M4Harness(_m2.Harness):
    """Subclass M2 Harness: override only what M4 needs differently."""

    def __init__(self, report_path):
        self._m4_admin_username = "m4-admin"
        self._m4_admin_password = secrets.token_urlsafe(32)
        self._from_rev = UPGRADE_FROM_REV
        self._extra_procs = []
        self._old_worktree = None
        self._old_target_dir = None
        self._old_db = None
        self.auth_matrix = []
        # Override name before parent __init__ so it uses our container name
        self.name = "githearth-m4-" + uuid.uuid4().hex[:12]
        super().__init__(report_path)
        # Patch DB name written by parent (it writes MAIN_DB="m2", we need "m4")
        self.env_path.write_text(
            "POSTGRES_USER=githearth\n"
            "POSTGRES_DB=" + MAIN_DB + "\n"
            "POSTGRES_PASSWORD=" + self.db_password + "\n")
        self.env_path.chmod(0o600)
        self.admin_username = self._m4_admin_username
        self.admin_password = self._m4_admin_password
        self.stranger_username = "m4-stranger"
        self.ssh_host_key_path = self.tmp / "ssh_host_key"
        self.ssh_port = None
        self.admin_ssh_key = None
        self.cli_binary = _build_cli()

    # ---- override parent infra methods touched by M4 needs ------------

    def sql(self, db, query):
        out = subprocess.run(
            ["docker", "exec", "-i", self.name, "psql",
             "-U", "githearth", "-d", db, "-t", "-A", "-v", "ON_ERROR_STOP=1"],
            input=query, text=True, capture_output=True)
        if out.returncode != 0:
            raise HarnessError(f"SQL failed ({out.returncode}): {out.stderr.strip()[:200]}")
        return out.stdout.strip()

    def server_env(self, db, storage_root, ssh_enabled=True, extra=None):
        env = super().server_env(db, storage_root)
        if ssh_enabled:
            env["GITHEARTH_SSH_ENABLED"] = "true"
            env["GITHEARTH_SSH_BIND"] = "127.0.0.1:0"
            env["GITHEARTH_SSH_HOST_KEY_PATH"] = str(self.ssh_host_key_path)
        if extra: env.update(extra)
        return env

    def start_server(self, db, storage_root, phase, ssh_enabled=True, extra=None):
        env = self.server_env(db, storage_root, ssh_enabled=ssh_enabled, extra=extra)
        hp = self.free_port()
        env["GITHEARTH_BIND"] = f"127.0.0.1:{hp}"
        proc = subprocess.Popen(
            [self.binary, "serve"], env=env,
            stdout=self.log_file, stderr=self.log_file)
        self._extra_procs.append(proc)
        sp = None
        if ssh_enabled:
            _wait_ssh_log(self, proc, phase)
            sp = _last_ssh_port(self)
        srv = M4Server(proc, hp, sp, phase)
        self.server = srv; self.http_base = f"http://127.0.0.1:{hp}"; self.ssh_port = sp
        self.wait_ready(proc, hp, phase)
        if ssh_enabled and sp:
            for _ in range(100):
                try:
                    s = socket.create_connection(("127.0.0.1", sp), timeout=1)
                    s.close(); break
                except OSError: time.sleep(0.1)
            else:
                raise HarnessError(f"{phase}: SSH port {sp} never became reachable")
        return srv

    def stop_server(self):
        super().stop_server()

    def record_lifetime(self):
        if self.server is not None:
            self.lifetimes.append(
                {"phase": self.server.phase,
                 "peak_rss_kib": self.server.rss_kib()})

    # ---- property override -------------------------------------------

    @property
    def repo_url(self):
        return f"http://127.0.0.1:{self.server.http_port}/{self.org_slug}/{self.repo_name}.git"

    # ---- SSH helpers -------------------------------------------------

    def generate_ssh_host_key(self):
        if self.ssh_host_key_path.exists(): return
        subprocess.run(
            [self.binary, "ssh-keygen"],
            env={**os.environ,
                 "GITHEARTH_SSH_HOST_KEY_PATH": str(self.ssh_host_key_path),
                 "GITHEARTH_DATABASE_URL": "postgres://nobody@127.0.0.1:1/none",
                 "GITHEARTH_STORAGE_ROOT": str(self.tmp / "dummy-storage")},
            check=True, capture_output=True, text=True)

    def ssh_keygen(self, stem):
        priv = Path(stem)
        subprocess.run(
            ["ssh-keygen", "-t", "ed25519", "-f", str(priv),
             "-N", "", "-C", "m4-test", "-q"],
            check=True, capture_output=True)
        return priv, priv.with_suffix(".pub")

    def ssh_fingerprint(self, key_path):
        out = subprocess.run(
            ["ssh-keygen", "-lf", str(key_path), "-E", "sha256"],
            capture_output=True, text=True, check=True).stdout
        return out.split()[1]

    def _ssh_key_env(self, key_path):
        return (
            # No -p: it would override the port in each ssh:// URL (every server has its own).
            f"ssh -i {key_path} "
            f"-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "
            f"-o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR")

    def ssh_git_env(self, key_path):
        env = self.git_env()
        env["GIT_SSH_COMMAND"] = self._ssh_key_env(key_path)
        return env

    def ssh_git(self, key_path, *args, cwd=None, check=True, label=None, timeout=600):
        cmd = ["git"] + list(args)
        start = time.time()
        out = subprocess.run(
            cmd, cwd=cwd or ROOT, env=self.ssh_git_env(key_path),
            capture_output=True, text=True, timeout=timeout)
        self.git_ops.append(
            {"name": label or ("ssh:" + args[0]), "seconds": round(time.time() - start, 3)})
        if check and out.returncode != 0:
            raise HarnessError(f"ssh git {' '.join(args)} failed: {out.stderr.strip()[:400]}")
        return out

    def ssh_repo_url(self, org=None, repo=None, port=None):
        return (f"ssh://git@127.0.0.1:{port or self.ssh_port}/"
                f"{org or self.org_slug}/{repo or self.repo_name}.git")

    # ---- API helpers on top of parent api/api_json -------------------

    def api_session(self, user, pw, base=None):
        st, s = self.api_json("POST", "/api/v1/login",
                               {"username": user, "password": pw}, base=base)
        if st != 200 or not s.get("token"):
            raise HarnessError(f"login for {user} status {st}")
        return s["token"]

    def api_create_token(self, session, name="m4pat", scopes=None, base=None):
        if scopes is None: scopes = ["repo:read", "repo:write"]
        st, tok = self.api_json("POST", "/api/v1/user/tokens",
                                 {"name": name, "scopes": scopes}, token=session, base=base)
        if st != 201: raise HarnessError(f"create token status {st}: {tok}")
        return tok["token"]

    def api_revoke_token(self, session, tid, base=None):
        st, _ = self.api_json("DELETE", f"/api/v1/user/tokens/{tid}", token=session, base=base)
        if st != 204: raise HarnessError(f"revoke token status {st}")

    def api_list_tokens(self, session, base=None):
        st, toks = self.api_json("GET", "/api/v1/user/tokens", token=session, base=base)
        return toks if st == 200 else []

    def api_register_ssh_key(self, session, pubkey_path, base=None):
        pubkey = str(Path(pubkey_path).read_text()).strip()
        st, key = self.api_json("POST", "/api/v1/user/ssh-keys",
                                 {"name": "m4key", "public_key": pubkey}, token=session, base=base)
        if st != 201: raise HarnessError(f"register SSH key status {st}: {key}")
        return key["id"]

    def api_revoke_ssh_key(self, session, kid, base=None):
        st, _ = self.api_json("DELETE", f"/api/v1/user/ssh-keys/{kid}", token=session, base=base)
        if st != 204: raise HarnessError(f"revoke SSH key status {st}")

    # ---- auth-matrix helpers -----------------------------------------

    def _try_clone_http(self, user, token):
        cd = self.tmp / f"ac-{uuid.uuid4().hex[:6]}"
        proc = self.git("clone", self.repo_url, str(cd), user=user, password=token, check=False)
        shutil.rmtree(str(cd), ignore_errors=True)
        return _classify(proc)

    def _try_push_http(self, user, token):
        cd = self.tmp / f"ap-{uuid.uuid4().hex[:6]}"
        self.git("clone", self.repo_url, str(cd),
                 user=self.admin_username, password=self.admin_password, check=True)
        (cd / f"p-{uuid.uuid4().hex[:4]}.txt").write_text("push\n")
        subprocess.run(["git", "-C", str(cd), "add", "."], capture_output=True)
        subprocess.run(["git", "-C", str(cd), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "probe"], capture_output=True)
        branch = f"p-{uuid.uuid4().hex[:6]}"
        proc = self.git("push", self.repo_url, f"HEAD:refs/heads/{branch}",
                         user=user, password=token, check=False, cwd=str(cd))
        shutil.rmtree(str(cd), ignore_errors=True)
        return _classify(proc)

    def _try_clone_ssh(self, kp):
        cd = self.tmp / f"as-{uuid.uuid4().hex[:6]}"
        proc = self.ssh_git(kp, "clone", self.ssh_repo_url(), str(cd), check=False)
        shutil.rmtree(str(cd), ignore_errors=True)
        return _classify(proc)

    def _try_push_ssh(self, kp):
        cd = self.tmp / f"as-{uuid.uuid4().hex[:6]}"
        self.git("clone", self.repo_url, str(cd),
                 user=self.admin_username, password=self.admin_password, check=True)
        (cd / f"sp-{uuid.uuid4().hex[:4]}.txt").write_text("spush\n")
        subprocess.run(["git", "-C", str(cd), "add", "."], capture_output=True)
        subprocess.run(["git", "-C", str(cd), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "sprobe"], capture_output=True)
        branch = f"sp-{uuid.uuid4().hex[:6]}"
        proc = self.ssh_git(kp, "push", self.ssh_repo_url(),
                             f"HEAD:refs/heads/{branch}", check=False, cwd=str(cd))
        shutil.rmtree(str(cd), ignore_errors=True)
        return _classify(proc)

    # ================================================================== S1
    def s1_install_from_runbook(self):
        env = self.server_env(MAIN_DB, self.storage_root)

        # doctor with invalid config exits 1
        ieval = dict(env); ieval["GITHEARTH_STORAGE_ROOT"] = str(self.tmp / "nonexistent")
        p = subprocess.run([self.binary, "doctor"], env=ieval, capture_output=True, text=True)
        if p.returncode == 0: raise HarnessError("doctor invalid config should exit non-0")

        # doctor before migrate: fail schema
        p = subprocess.run([self.binary, "doctor"], env=env, capture_output=True, text=True)
        if p.returncode == 0 or "fail schema" not in (p.stdout + p.stderr):
            raise HarnessError("doctor before migrate should report fail schema")

        # serve refuses
        p = subprocess.run([self.binary, "serve"], env=env, capture_output=True, text=True)
        if p.returncode == 0: raise HarnessError("serve before migrate should refuse")

        # migrate
        subprocess.run([self.binary, "migrate"], env=env, check=True, capture_output=True, text=True)

        # bootstrap-admin
        b = subprocess.run(
            [self.binary, "bootstrap-admin", "--username", self.admin_username],
            env=env, input=self.admin_password, text=True, capture_output=True)
        if b.returncode != 0:
            raise HarnessError("bootstrap-admin failed: " + b.stderr.strip()[:300])

        # ssh-keygen (needs no DB; rerun does not overwrite)
        self.generate_ssh_host_key()
        p = subprocess.run(
            [self.binary, "ssh-keygen"],
            env={**os.environ,
                 "GITHEARTH_SSH_HOST_KEY_PATH": str(self.ssh_host_key_path),
                 "GITHEARTH_DATABASE_URL": "postgres://nobody@127.0.0.1:1/none",
                 "GITHEARTH_STORAGE_ROOT": str(self.tmp / "dummy-storage")},
            capture_output=True, text=True)
        if p.returncode == 0: raise HarnessError("ssh-keygen rerun should refuse to overwrite")

        # doctor exits 0
        p = subprocess.run([self.binary, "doctor"], env=env, capture_output=True, text=True)
        if p.returncode != 0:
            raise HarnessError(f"doctor after migrate exit non-0: {p.stdout[:200]} {p.stderr[:200]}")

        self.start_server(MAIN_DB, self.storage_root, "initial")
        if self.ssh_port is None: raise HarnessError("SSH port not found")

        # Set up admin, org, and SSH key for later scenarios
        tok = self.admin_token = self.api_session(self.admin_username, self.admin_password)
        self.admin_ssh_key = self.tmp / "admin_ssh_key"
        _, apub = self.ssh_keygen(self.admin_ssh_key)
        self.api_register_ssh_key(tok, apub)

        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "m4-org", "name": "M4 Org"}, token=tok)
        if st != 201: raise HarnessError(f"create org status {st}")
        self.org_id = org["id"]; self.org_slug = org["slug"]

    # ================================================================== S2
    def s2_auth_agreement(self):
        tok = self.admin_token
        st, repo = self.api_json(
            "POST", "/api/v1/organizations/m4-org/repositories",
            {"name": "m4-repo", "visibility": "private"}, token=tok)
        if st != 201: raise HarnessError(f"create repo status {st}")
        self.repo_id = repo["id"]; self.repo_name = "m4-repo"

        # Seed push
        w = self.main_work; w.mkdir()
        self.wgit("init", "-b", "main")
        self.wgit("config", "user.name", "M4 Test")
        self.wgit("config", "user.email", "m4@example.com")
        (w / "README.md").write_text("# M4\n")
        self.wgit("add", "-A"); self.wgit("commit", "-m", "initial")
        self.seed_sha = self.wgit("rev-parse", "HEAD").stdout.strip()
        self.git("push", self.repo_url, "main",
                 user=self.admin_username, password=self.admin_password, label="seed push")

        # Reader user
        ruser = "m4-reader"
        self.sql(MAIN_DB,
                 "INSERT INTO accounts (id, username, password_hash) "
                 "SELECT gen_random_uuid(), '" + ruser
                 + "', password_hash FROM accounts WHERE is_admin LIMIT 1;")
        rid = self.sql(MAIN_DB, "SELECT id FROM accounts WHERE username='" + ruser + "'")
        self.sql(MAIN_DB,
                 "INSERT INTO memberships (organization_id, account_id, role) "
                 "VALUES ('" + self.org_id + "', '" + rid + "', 'member');")
        aid = self.sql(MAIN_DB, "SELECT id FROM accounts WHERE is_admin LIMIT 1;")
        self.sql(MAIN_DB,
                 "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, repository_id, granted_by) "
                 "VALUES (gen_random_uuid(), 'read', 'account', '" + rid
                 + "', 'repository', '" + self.org_id + "', '" + self.repo_id + "', '" + aid + "');")

        # Non-member
        nmuser = "m4-outsider"
        self.sql(MAIN_DB,
                 "INSERT INTO accounts (id, username, password_hash) "
                 "SELECT gen_random_uuid(), '" + nmuser
                 + "', password_hash FROM accounts WHERE is_admin LIMIT 1;")

        # Suspended
        suser = "m4-suspended"
        self.sql(MAIN_DB,
                 "INSERT INTO accounts (id, username, password_hash, suspended) "
                 "SELECT gen_random_uuid(), '" + suser
                 + "', password_hash, true FROM accounts WHERE is_admin LIMIT 1;")
        sid = self.sql(MAIN_DB, "SELECT id FROM accounts WHERE username='" + suser + "'")

        # SSH keys
        rsession = self.api_session(ruser, self.admin_password)
        self.reader_ssh_key = self.tmp / "reader_ssh_key"
        _, rpub = self.ssh_keygen(self.reader_ssh_key)
        self.reader_ssh_key_id = self.api_register_ssh_key(rsession, rpub)

        nmsession = self.api_session(nmuser, self.admin_password)
        self.nonmember_ssh_key = self.tmp / "nonmember_ssh_key"
        _, nmpub = self.ssh_keygen(self.nonmember_ssh_key)
        self.nonmember_ssh_key_id = self.api_register_ssh_key(nmsession, nmpub)

        self.suspended_ssh_key = self.tmp / "suspended_ssh_key"
        _, suspub = self.ssh_keygen(self.suspended_ssh_key)
        fp = self.ssh_fingerprint(self.suspended_ssh_key)
        pubtxt = Path(suspub).read_text().strip()
        self.sql(MAIN_DB,
                 "INSERT INTO ssh_keys (id, account_id, name, algorithm, public_key, fingerprint) "
                 "VALUES (gen_random_uuid(), '" + sid
                 + "', 'm4key', 'ed25519', " + _sq(pubtxt) + ", '" + fp + "');")

        # Tokens for revocation tests
        rtk = self.api_create_token(rsession, "m4-pat-reader")
        ntk = self.api_create_token(nmsession, "m4-pat-out")
        rtoks = self.api_list_tokens(rsession)
        self.reader_token_id = rtoks[-1]["id"] if rtoks else None
        ntoks = self.api_list_tokens(nmsession)
        self.nonmember_token_id = ntoks[-1]["id"] if ntoks else None

        # Matrix
        actors = [
            ("owner", self.admin_username, self.admin_ssh_key),
            ("reader", ruser, self.reader_ssh_key),
            ("nonmember", nmuser, self.nonmember_ssh_key),
            ("suspended", suser, self.suspended_ssh_key)]

        pw = self.admin_password
        for state in ("active", "archived"):
            if state == "archived": self._archive_repo()
            for an, au, ak in actors:
                for op in ("clone", "push"):
                    hr = self._try_clone_http(au, pw) if op == "clone" else self._try_push_http(au, pw)
                    sr = self._try_clone_ssh(ak) if op == "clone" else self._try_push_ssh(ak)
                    hr_norm = "ALLOWED" if "ALLOWED" in hr else "DENIED"
                    sr_norm = "ALLOWED" if "ALLOWED" in sr else "DENIED"
                    if hr_norm != sr_norm:
                        raise HarnessError(f"{an}/{state}/{op}: HTTP={hr} SSH={sr} DISAGREE")
                    # Agreement is not enough: both transports must give the right answer.
                    want = "ALLOWED" if EXPECTED_ACCESS.get((an, state, op), False) else "DENIED"
                    if hr_norm != want:
                        raise HarnessError(f"{an}/{state}/{op}: expected {want}, HTTP={hr} SSH={sr}")
                    # A non-member must not learn that the repository exists.
                    if an == "nonmember" and not (hr == sr == "DENIED (not-found)"):
                        raise HarnessError(f"nonmember/{state}/{op}: expected not-found, HTTP={hr} SSH={sr}")
                    self.auth_matrix.append(
                        {"actor": an, "repo_state": state, "operation": op,
                         "http_result": hr, "ssh_result": sr, "agree": hr_norm == sr_norm})
            if state == "archived": self._unarchive_repo()

        # Revoked SSH key
        rs2 = self.api_session(ruser, self.admin_password)
        self.api_revoke_ssh_key(rs2, self.reader_ssh_key_id)
        r = self._try_clone_ssh(self.reader_ssh_key)
        if "ALLOWED" in r: raise HarnessError(f"revoked SSH key: {r}")
        self.auth_matrix.append(
            {"actor": "reader_revoked_key", "repo_state": "active", "operation": "clone",
             "http_result": None, "ssh_result": r, "agree": "N/A"})

        # Revoked token
        rs3 = self.api_session(ruser, self.admin_password)
        self.api_revoke_token(rs3, self.reader_token_id)
        r = self._try_clone_http(ruser, rtk)
        if "ALLOWED" in r: raise HarnessError(f"revoked token: {r}")
        self.auth_matrix.append(
            {"actor": "reader_revoked_token", "repo_state": "active", "operation": "clone",
             "http_result": r, "ssh_result": None, "agree": "N/A"})

        print(f"    authorization matrix: {len(self.auth_matrix)} cells")

    def _archive_repo(self):
        st, _ = self.api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/archive",
            token=self.admin_token)
        if st != 200: raise HarnessError(f"archive status {st}")

    def _unarchive_repo(self):
        st, _ = self.api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/unarchive",
            token=self.admin_token)
        if st != 200: raise HarnessError(f"unarchive status {st}")

    # ================================================================== S3
    def _ls_remote(self, url, env):
        """Remote refs; fails unless ls-remote works and lists main, so an absent ref proves something."""
        out = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "ls-remote", url],
            env=env, capture_output=True, text=True, timeout=120)
        if out.returncode != 0 or "refs/heads/main" not in out.stdout:
            raise HarnessError(f"ls-remote {url} failed or lacks main: rc={out.returncode} {out.stderr[:200]}")
        return out.stdout

    def s3_quota(self):
        self.stop_server()
        self.generate_ssh_host_key()
        self.start_server(MAIN_DB, self.storage_root, "quota",
                          extra={"GITHEARTH_REPOSITORY_QUOTA_BYTES": str(1 * 1024 * 1024)})
        tok = self.api_session(self.admin_username, self.admin_password)
        self.admin_token = tok

        st, qrepo = self.api_json(
            "POST", "/api/v1/organizations/m4-org/repositories",
            {"name": "quota-repo", "visibility": "private"}, token=tok)
        if st != 201: raise HarnessError(f"create quota-repo status {st}")
        qrepo_name = "quota-repo"
        qurl = f"http://127.0.0.1:{self.server.http_port}/{self.org_slug}/{qrepo_name}.git"
        qssh = self.ssh_repo_url(org=self.org_slug, repo=qrepo_name)

        qw = self.tmp / "qw"; qw.mkdir()
        subprocess.run(["git", "init", "-b", "main"], cwd=str(qw),
                       env=self.git_env(), capture_output=True, check=True)
        (qw / "seed.txt").write_text("seed\n")
        for ea in (["config", "user.name", "T"], ["config", "user.email", "t@t"],
                   ["add", "-A"], ["commit", "-m", "seed"]):
            subprocess.run(["git", "-C", str(qw)] + ea, capture_output=True)
        penv = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(qw), "push", qurl, "main"],
            env=penv, capture_output=True, text=True, check=True)

        try: self.api_register_ssh_key(tok, self.admin_ssh_key.with_suffix(".pub"))
        except HarnessError as e:
            if "already registered" not in str(e) and "409" not in str(e): raise

        # Ref-only push succeeds over HTTP
        subprocess.run(["git", "-C", str(qw), "checkout", "-b", "ref-only"], capture_output=True)
        p = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(qw), "push", qurl, "ref-only"],
            env=penv, capture_output=True, text=True, timeout=120)
        if p.returncode != 0:
            raise HarnessError(f"quota: ref-only HTTP push should succeed: {p.stderr[:400]}")

        # Ref-only push succeeds over SSH
        subprocess.run(["git", "-C", str(qw), "checkout", "-b", "ref-only-ssh"], capture_output=True)
        p = subprocess.run(
            ["git", "-C", str(qw), "push", qssh, "ref-only-ssh"],
            env=self.ssh_git_env(self.admin_ssh_key),
            capture_output=True, text=True, timeout=120)
        if p.returncode != 0:
            raise HarnessError(f"quota: ref-only SSH push should succeed: {p.stderr[:400]}")

        # Incompressible data > 1 MiB → push must be refused
        data = os.urandom(1200000)  # ~1.14 MiB random, above 1 MiB quota
        (qw / "big.bin").write_bytes(data)
        subprocess.run(["git", "-C", str(qw), "add", "big.bin"], capture_output=True)
        subprocess.run(["git", "-C", str(qw), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "big quota"], capture_output=True)

        # HTTP
        p = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(qw), "push", qurl, "HEAD:refs/heads/quota-test"],
            env=penv, capture_output=True, text=True, timeout=120)
        if p.returncode == 0:
            raise HarnessError("quota: HTTP push above limit should be refused")
        if "refs/heads/quota-test" in self._ls_remote(qurl, penv):
            raise HarnessError("quota: ref moved despite HTTP push refusal")

        # SSH
        ssp = subprocess.run(
            ["git", "-C", str(qw), "push", qssh, "HEAD:refs/heads/quota-ssh"],
            env=self.ssh_git_env(self.admin_ssh_key),
            capture_output=True, text=True, timeout=120)
        if ssp.returncode == 0:
            raise HarnessError("quota: SSH push above limit should be refused")
        if any(m in ssp.stderr for m in ("Connection refused", "Permission denied (publickey")):
            raise HarnessError(f"quota: SSH push never reached the server: {ssp.stderr[:300]}")
        if "refs/heads/quota-ssh" in self._ls_remote(qurl, penv):
            raise HarnessError("quota: ref moved despite SSH push refusal")

        shutil.rmtree(str(qw), ignore_errors=True)

    # ================================================================== S4
    def s4_backup_restore(self):
        dp = self.tmp / "m4.dump"
        with open(dp, "wb") as d:
            subprocess.run(
                ["docker", "exec", self.name, "pg_dump", "-U", "githearth", "-d", MAIN_DB, "-Fc"],
                check=True, stdout=d)
        st = self.tmp / "storage.tar"
        subprocess.run(
            ["tar", "-C", str(self.storage_root), "--exclude=run", "-cf", str(st), "."],
            check=True)
        self.stop_server()

        self.docker(["exec", self.name, "createdb", "-U", "githearth", RESTORE_DB],
                    stdout=subprocess.DEVNULL)
        with open(dp, "rb") as d:
            subprocess.run(
                ["docker", "exec", "-i", self.name, "pg_restore",
                 "-U", "githearth", "--no-owner", "-d", RESTORE_DB],
                check=True, stdin=d)
        ns = self.tmp / "storage-restore"; ns.mkdir()
        subprocess.run(["tar", "-C", str(ns), "-xf", str(st)], check=True)

        p = subprocess.run([self.cli_binary, "check", str(ns)], capture_output=True, text=True)
        if p.returncode != 0:
            raise HarnessError(f"restore: githearth check exited {p.returncode}: {p.stderr[:300]}")

        self.generate_ssh_host_key()
        self.start_server(RESTORE_DB, ns, "restored")
        rtok = self.api_session(self.admin_username, self.admin_password)
        r1 = self._try_clone_http(self.admin_username, self.admin_password)
        if "ALLOWED" not in r1:
            raise HarnessError(f"restore: owner HTTP clone denied: {r1}")

        try: self.api_register_ssh_key(rtok, self.admin_ssh_key.with_suffix(".pub"))
        except HarnessError as e:
            if "already registered" not in str(e) and "409" not in str(e): raise
        r2 = self._try_clone_ssh(self.admin_ssh_key)
        if "ALLOWED" not in r2:
            raise HarnessError(f"restore: owner SSH clone denied: {r2}")

        nmr = self._try_clone_http("m4-outsider", self.admin_password)
        if "not-found" not in nmr and "forbidden" not in nmr:
            raise HarnessError(f"restore: nonmember should be denied: {nmr}")

        if self.nonmember_ssh_key:
            nmr2 = self._try_clone_ssh(self.nonmember_ssh_key)
            if "not-found" not in nmr2:
                raise HarnessError(f"restore: nonmember SSH should be denied: {nmr2}")

    # ================================================================== S5
    def s5_upgrade_drill(self):
        self.stop_server()
        old_binary = _build_old(self)
        self._old_db = old_db = "m4old"
        self.docker(["exec", self.name, "createdb", "-U", "githearth", old_db],
                    stdout=subprocess.DEVNULL)
        old_st = self.tmp / "storage-old"; old_st.mkdir()
        ohk = old_st / "ssh_host_key"

        subprocess.run(
            [old_binary, "ssh-keygen"],
            env={**os.environ,
                 "GITHEARTH_SSH_HOST_KEY_PATH": str(ohk),
                 "GITHEARTH_DATABASE_URL": "postgres://nobody@127.0.0.1:1/none",
                 "GITHEARTH_STORAGE_ROOT": str(old_st)},
            check=True, capture_output=True, text=True)

        oenv = {**os.environ,
                "GITHEARTH_DATABASE_URL": self.db_url(old_db),
                "GITHEARTH_STORAGE_ROOT": str(old_st),
                "GITHEARTH_SECURE_COOKIES": "false",
                "GITHEARTH_RECONCILE_INTERVAL_SECONDS": "5",
                "GITHEARTH_RECONCILE_GRACE_SECONDS": "30",
                "GITHEARTH_SSH_HOST_KEY_PATH": str(ohk),
                "GITHEARTH_SSH_ENABLED": "true",
                "GITHEARTH_SSH_BIND": "127.0.0.1:0"}

        subprocess.run([old_binary, "migrate"], env=oenv, check=True, capture_output=True, text=True)
        b = subprocess.run(
            [old_binary, "bootstrap-admin", "--username", self.admin_username],
            env=oenv, input=self.admin_password, text=True, capture_output=True)
        if b.returncode != 0:
            raise HarnessError("old binary bootstrap-admin: " + b.stderr.strip()[:300])

        ohp = self.free_port()
        oenv["GITHEARTH_BIND"] = f"127.0.0.1:{ohp}"
        old_log = self.tmp / "old.log"; old_log_f = open(old_log, "wb")
        old_proc = subprocess.Popen(
            [old_binary, "serve"], env=oenv, stdout=old_log_f, stderr=old_log_f)
        self._extra_procs.append(old_proc)
        obase = f"http://127.0.0.1:{ohp}"
        _wait_http(old_proc, self, obase, "old-serve")
        osp = _read_ssh_port_from_log(old_log)

        osession = self.api_session(self.admin_username, self.admin_password, obase)
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "upgrade-org", "name": "Upgrade Org"}, token=osession, base=obase)
        if st != 201: raise HarnessError(f"old create org status {st}")
        oslug = org["slug"]

        st, repo = self.api_json(
            "POST", f"/api/v1/organizations/{oslug}/repositories",
            {"name": "upgrade-repo", "visibility": "private"}, token=osession, base=obase)
        if st != 201: raise HarnessError(f"old create repo status {st}")
        oname = repo["name"]

        ourl = f"http://127.0.0.1:{ohp}/{oslug}/{oname}.git"
        ow = self.tmp / "old-work"; ow.mkdir()
        subprocess.run(["git", "init", "-b", "main"], cwd=str(ow),
                       env=self.git_env(), capture_output=True, check=True)
        (ow / "old.txt").write_text("old binary\n")
        for ea in (["config", "user.name", "Test"], ["config", "user.email", "t@t"],
                   ["add", "-A"], ["commit", "-m", "old"]):
            subprocess.run(["git", "-C", str(ow)] + ea, capture_output=True)
        old_sha = subprocess.run(
            ["git", "-C", str(ow), "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True).stdout.strip()

        penv = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(ow), "push", ourl, "main"],
            env=penv, capture_output=True, text=True, check=True)

        st, issue = self.api_json(
            "POST", f"/api/v1/organizations/{oslug}/repositories/{oname}/issues",
            {"title": "Old Issue", "body": "created before upgrade"},
            token=osession, base=obase)
        if st != 201: raise HarnessError(f"old issue status {st}")
        inum = issue["number"]

        _kill_proc(old_proc)
        old_log_f.close()

        # ---- New binary ----
        nenv = {**os.environ,
                "GITHEARTH_DATABASE_URL": self.db_url(old_db),
                "GITHEARTH_STORAGE_ROOT": str(old_st),
                "GITHEARTH_SECURE_COOKIES": "false"}
        p = subprocess.run([self.binary, "doctor"], env=nenv, capture_output=True, text=True)
        if p.returncode == 0: raise HarnessError("new doctor should fail with pending migrations")
        if "fail schema" not in (p.stdout + p.stderr):
            raise HarnessError(f"new doctor missing 'fail schema': {(p.stdout+p.stderr)[:300]}")

        p = subprocess.run([self.binary, "serve"], env=nenv, capture_output=True, text=True)
        if p.returncode == 0: raise HarnessError("new serve should refuse before migrate")

        subprocess.run([self.binary, "migrate"], env=nenv, check=True, capture_output=True, text=True)
        p = subprocess.run([self.binary, "doctor"], env=nenv, capture_output=True, text=True)
        if p.returncode != 0:
            raise HarnessError(f"new doctor after migrate exit non-0: {(p.stdout+p.stderr)[:300]}")

        nhp = self.free_port()
        nenv.update({
            "GITHEARTH_BIND": f"127.0.0.1:{nhp}",
            "GITHEARTH_SSH_ENABLED": "true", "GITHEARTH_SSH_BIND": "127.0.0.1:0",
            "GITHEARTH_SSH_HOST_KEY_PATH": str(ohk),
            "GITHEARTH_SECURE_COOKIES": "false",
            "GITHEARTH_RECONCILE_INTERVAL_SECONDS": "5",
            "GITHEARTH_RECONCILE_GRACE_SECONDS": "30"})
        ulog = self.tmp / "upgrade.log"; ulog_f = open(ulog, "wb")
        new_proc = subprocess.Popen(
            [self.binary, "serve"], env=nenv, stdout=ulog_f, stderr=ulog_f)
        self._extra_procs.append(new_proc)
        nbase = f"http://127.0.0.1:{nhp}"
        _wait_http(new_proc, self, nbase, "upgrade")
        usp = _read_ssh_port_from_log(ulog)
        if usp is None: raise HarnessError("upgrade SSH port not found")

        nsession = self.api_session(self.admin_username, self.admin_password, nbase)
        nurl = f"http://127.0.0.1:{nhp}/{oslug}/{oname}.git"

        uc = self.tmp / "upgrade-clone"
        penv2 = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "clone", nurl, str(uc)],
            env=penv2, capture_output=True, text=True, check=True)
        csha = subprocess.run(
            ["git", "-C", str(uc), "rev-parse", "HEAD"],
            capture_output=True, text=True).stdout.strip()
        if csha != old_sha:
            raise HarnessError(f"upgrade: SHA {csha[:8]} != {old_sha[:8]}")

        st, iss = self.api_json(
            "GET", f"/api/v1/organizations/{oslug}/repositories/{oname}/issues/{inum}",
            token=nsession, base=nbase)
        if st != 200 or iss.get("title") != "Old Issue":
            raise HarnessError(f"issue not readable after upgrade: {st}")

        # New HTTP push
        (uc / "upgrade.txt").write_text("after upgrade\n")
        subprocess.run(["git", "-C", str(uc), "add", "upgrade.txt"], capture_output=True)
        subprocess.run(["git", "-C", str(uc), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "after upgrade"], capture_output=True)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(uc), "push", nurl, "HEAD:refs/heads/main"],
            env=penv2, capture_output=True, text=True, check=True)

        # New SSH push
        ukey = self.tmp / "upgrade_ssh_key"
        _, upub = self.ssh_keygen(ukey)
        self.api_register_ssh_key(nsession, upub, base=nbase)
        surl = f"ssh://git@127.0.0.1:{usp}/{oslug}/{oname}.git"
        (uc / "ssh-upgrade.txt").write_text("ssh after upgrade\n")
        subprocess.run(["git", "-C", str(uc), "add", "ssh-upgrade.txt"], capture_output=True)
        subprocess.run(["git", "-C", str(uc), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "ssh after upgrade"], capture_output=True)
        ssh_e = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                 "HOME": str(self.home), "GIT_CONFIG_NOSYSTEM": "1",
                 "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TERMINAL_PROMPT": "0",
                 "GIT_SSL_NO_VERIFY": "1", "LC_ALL": "C",
                 "GIT_SSH_COMMAND": (
                     f"ssh -i {ukey} -p {usp} -o StrictHostKeyChecking=no "
                     f"-o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes "
                     f"-o BatchMode=yes -o LogLevel=ERROR")}
        proc = subprocess.run(
            ["git", "-C", str(uc), "push", surl, "HEAD:refs/heads/main"],
            env=ssh_e, capture_output=True, text=True, timeout=120)
        if proc.returncode != 0:
            raise HarnessError(f"upgrade SSH push failed: {proc.stderr[:400]}")

        # Old binary refuses newer schema
        p = subprocess.run(
            [old_binary, "serve"],
            env={**os.environ,
                 "GITHEARTH_DATABASE_URL": self.db_url(old_db),
                 "GITHEARTH_STORAGE_ROOT": str(old_st)},
            capture_output=True, text=True)
        if p.returncode == 0:
            raise HarnessError("old binary should refuse newer schema")

        _kill_proc(new_proc)
        ulog_f.close()

    # ================================================================== S6
    def s6_restart_jobs(self):
        self.stop_server()
        if self._old_db:
            self.docker(["exec", self.name, "dropdb", "-U", "githearth", "--if-exists", self._old_db],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self._old_db = None
        self.generate_ssh_host_key()
        self.start_server(MAIN_DB, self.storage_root, "final")
        env = self.server_env(MAIN_DB, self.storage_root, ssh_enabled=False)
        p = subprocess.run([self.binary, "doctor"], env=env, capture_output=True, text=True)
        if p.returncode != 0:
            raise HarnessError(f"final doctor exit non-0: {(p.stdout+p.stderr)[:500]}")
        if "fail jobs" in (p.stdout + p.stderr).lower():
            raise HarnessError("doctor jobs check shows failure")

    # ================================================================== S7
    def s7_container_install(self):
        self.stop_server()
        # Build the Docker image
        img = "githearth-m4-test:" + uuid.uuid4().hex[:8]
        subprocess.run(
            ["docker", "build", "-t", img, "-f", str(ROOT / "Dockerfile"), str(ROOT)],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600)

        net = "githearth-m4-net-" + uuid.uuid4().hex[:8]
        self._s7_net = net
        subprocess.run(["docker", "network", "create", net], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        pg_name = "githearth-m4-pg-" + uuid.uuid4().hex[:8]
        vol_name = "githearth-m4-repos-" + uuid.uuid4().hex[:8]
        self._s7_pg = pg_name
        self._s7_vol = vol_name
        self._s7_img = img

        subprocess.run([
            "docker", "run", "--detach", "--rm", "--name", pg_name,
            "--network", net,
            "-e", "POSTGRES_DB=githearth",
            "-e", "POSTGRES_USER=githearth",
            "-e", "POSTGRES_PASSWORD=" + self.db_password,
            IMAGE,
        ], check=True, stdout=subprocess.DEVNULL)
        for _ in range(120):
            if subprocess.run(
                ["docker", "exec", pg_name, "pg_isready", "-h", "127.0.0.1",
                 "-U", "githearth", "-d", "githearth"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            ).returncode == 0:
                break
            time.sleep(0.5)
        else:
            raise HarnessError("s7 postgres startup timed out")

        db_url = "postgres://githearth:" + self.db_password + "@" + pg_name + ":5432/githearth"
        # migrate
        subprocess.run([
            "docker", "run", "--rm", "--network", net,
            "-v", vol_name + ":/var/lib/githearth",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            img, "migrate",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=120)

        # bootstrap-admin
        subprocess.run([
            "docker", "run", "--rm", "--network", net, "-i",
            "-v", vol_name + ":/var/lib/githearth",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            img, "bootstrap-admin", "--username", self.admin_username,
        ], input=self.admin_password, text=True, check=True,
           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)

        # The server's own ssh-keygen; the image sets GITHEARTH_SSH_HOST_KEY_PATH.
        subprocess.run([
            "docker", "run", "--rm",
            "-v", vol_name + ":/var/lib/githearth",
            # Config validation needs a database URL even though ssh-keygen never connects.
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            img, "ssh-keygen",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)

        # serve in background
        hp = self.free_port()
        sp = self.free_port()
        srv_name = "githearth-m4-srv-" + uuid.uuid4().hex[:8]
        self._s7_srv = srv_name
        subprocess.run([
            "docker", "run", "--detach", "--name", srv_name,  # no --rm: keep logs for failures
            "--network", net,
            "-v", vol_name + ":/var/lib/githearth",
            "-p", f"127.0.0.1:{hp}:8080",
            "-p", f"127.0.0.1:{sp}:2222",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            "-e", "GITHEARTH_SSH_ENABLED=true",
            "-e", "GITHEARTH_SSH_HOST_KEY_PATH=/var/lib/githearth/ssh_host_key",
            "-e", "GITHEARTH_BIND=0.0.0.0:8080",
            "-e", "GITHEARTH_SSH_BIND=0.0.0.0:2222",
            "-e", "GITHEARTH_SECURE_COOKIES=false",
            img, "serve",
        ], check=True, stdout=subprocess.DEVNULL, timeout=30)

        base = f"http://127.0.0.1:{hp}"
        # Wait ready
        for _ in range(300):
            try:
                st, _ = self.api_json("GET", "/health/ready", base=base)
                if st == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            if subprocess.run(
                ["docker", "inspect", srv_name, "--format", "{{.State.Status}}"],
                capture_output=True, text=True
            ).stdout.strip() != "running":
                raise HarnessError("s7 server stopped during startup")
            time.sleep(0.1)
        else:
            raise HarnessError("s7 server readiness timed out")

        # Wait for SSH to be reachable
        for _ in range(100):
            try:
                s = socket.create_connection(("127.0.0.1", sp), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise HarnessError(f"s7 SSH port {sp} never became reachable")

        # login, create org, create private repo
        tok = self.api_session(self.admin_username, self.admin_password, base=base)
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "s7-org", "name": "S7 Org"}, token=tok, base=base)
        if st != 201:
            raise HarnessError(f"s7 create org status {st}")
        org_slug = org["slug"]
        st, repo = self.api_json(
            "POST", f"/api/v1/organizations/{org_slug}/repositories",
            {"name": "s7-repo", "visibility": "private"}, token=tok, base=base)
        if st != 201:
            raise HarnessError(f"s7 create repo status {st}")
        repo_name = repo["name"]
        http_url = f"http://127.0.0.1:{hp}/{org_slug}/{repo_name}.git"

        # Push over HTTP
        w = self.tmp / "s7-work"; w.mkdir()
        subprocess.run(["git", "init", "-b", "main"], cwd=str(w),
                       env=self.git_env(), capture_output=True, check=True)
        (w / "README.md").write_text("# S7\n")
        subprocess.run(["git", "-C", str(w), "config", "user.name", "S7"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "config", "user.email", "s7@t"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "add", "-A"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "commit", "-m", "init"], capture_output=True)
        penv = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", http_url, "main"],
            env=penv, capture_output=True, text=True, check=True, timeout=120)

        # Register SSH key
        skey = self.tmp / "s7_ssh_key"
        _, spub = self.ssh_keygen(skey)
        self.api_register_ssh_key(tok, spub, base=base)

        # Push over SSH
        s7_ssh = f"ssh://git@127.0.0.1:{sp}/{org_slug}/{repo_name}.git"
        subprocess.run(["git", "-C", str(w), "-c", "user.name=S7", "-c", "user.email=s7@t",
                         "commit", "--allow-empty", "-m", "ssh"], capture_output=True)
        ssh_e = self.ssh_git_env(skey)
        sproc = subprocess.run(
            ["git", "-C", str(w), "push", s7_ssh, "main"],
            env=ssh_e, capture_output=True, text=True, timeout=120)
        if sproc.returncode != 0:
            raise HarnessError(f"s7 SSH push failed: {sproc.stderr.strip()[:400]}\n"
                               + _container_tail(srv_name))

        # Clone over HTTP
        ch = self.tmp / "s7-clone-http"; ch.mkdir()
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "clone", http_url, str(ch)],
            env=penv, capture_output=True, text=True, check=True, timeout=120)
        http_refs = subprocess.run(
            ["git", "-C", str(ch), "show-ref", "--heads"],
            capture_output=True, text=True, check=True).stdout.strip()

        # Clone over SSH
        cs = self.tmp / "s7-clone-ssh"; cs.mkdir()
        subprocess.run(
            ["git", "clone", s7_ssh, str(cs)],
            env=ssh_e, capture_output=True, text=True, check=True, timeout=120)
        ssh_refs = subprocess.run(
            ["git", "-C", str(cs), "show-ref", "--heads"],
            capture_output=True, text=True, check=True).stdout.strip()

        if http_refs != ssh_refs:
            raise HarnessError(f"s7: HTTP and SSH refs differ\nHTTP:{http_refs}\nSSH:{ssh_refs}")

        # doctor inside container
        p = subprocess.run(
            ["docker", "exec", "-e", "GITHEARTH_DATABASE_URL=" + db_url,
             srv_name, "githearth-server", "doctor"],
            capture_output=True, text=True, timeout=30)
        if p.returncode != 0:
            raise HarnessError(f"s7: doctor exited {p.returncode}: {(p.stdout+p.stderr)[:300]}")

        # Restart
        subprocess.run(["docker", "stop", srv_name], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        subprocess.run(["docker", "rm", "--volumes", srv_name], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)

        srv_name2 = "githearth-m4-srv2-" + uuid.uuid4().hex[:8]
        self._s7_srv2 = srv_name2
        subprocess.run([
            "docker", "run", "--detach", "--rm", "--name", srv_name2,
            "--network", net,
            "-v", vol_name + ":/var/lib/githearth",
            "-p", f"127.0.0.1:{hp}:8080",
            "-p", f"127.0.0.1:{sp}:2222",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            "-e", "GITHEARTH_SSH_ENABLED=true",
            "-e", "GITHEARTH_SSH_HOST_KEY_PATH=/var/lib/githearth/ssh_host_key",
            "-e", "GITHEARTH_BIND=0.0.0.0:8080",
            "-e", "GITHEARTH_SSH_BIND=0.0.0.0:2222",
            "-e", "GITHEARTH_SECURE_COOKIES=false",
            img, "serve",
        ], check=True, stdout=subprocess.DEVNULL, timeout=30)

        # Wait ready
        base2 = f"http://127.0.0.1:{hp}"
        for _ in range(300):
            try:
                st, _ = self.api_json("GET", "/health/ready", base=base2)
                if st == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.1)
        else:
            raise HarnessError("s7 restarted server readiness timed out")

        # Clone after restart still works
        cr = self.tmp / "s7-clone-restart"; cr.mkdir()
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "clone", http_url, str(cr)],
            env=penv, capture_output=True, text=True, check=True, timeout=120)
        rest_refs = subprocess.run(
            ["git", "-C", str(cr), "show-ref", "--heads"],
            capture_output=True, text=True, check=True).stdout.strip()
        if rest_refs != http_refs:
            raise HarnessError("s7: restart clone refs mismatch")

        shutil.rmtree(str(w), ignore_errors=True)
        shutil.rmtree(str(ch), ignore_errors=True)
        shutil.rmtree(str(cs), ignore_errors=True)
        shutil.rmtree(str(cr), ignore_errors=True)

    # ================================================================== S8
    def s8_disk_full(self):
        self.stop_server()
        img = getattr(self, "_s7_img", None)
        if not img:
            img = "githearth-m4-test:" + uuid.uuid4().hex[:8]
            subprocess.run(
                ["docker", "build", "-t", img, "-f", str(ROOT / "Dockerfile"), str(ROOT)],
                check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600)
            self._s7_img = img

        pg_name = "githearth-m4-pgdf-" + uuid.uuid4().hex[:8]
        srv_name = "githearth-m4-srvdf-" + uuid.uuid4().hex[:8]
        vol_name = "githearth-m4-dfvol-" + uuid.uuid4().hex[:8]
        self._s8_pg = pg_name
        self._s8_srv = srv_name
        self._s8_vol = vol_name

        net = getattr(self, "_s7_net", None)
        if net is None:
            net = "githearth-m4-net-" + uuid.uuid4().hex[:8]
            subprocess.run(["docker", "network", "create", net], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self._s8_net = net
        else:
            self._s8_net = None

        subprocess.run([
            "docker", "run", "--detach", "--rm", "--name", pg_name,
            "--network", net,
            "-e", "POSTGRES_DB=githearth",
            "-e", "POSTGRES_USER=githearth",
            "-e", "POSTGRES_PASSWORD=" + self.db_password,
            IMAGE,
        ], check=True, stdout=subprocess.DEVNULL)
        for _ in range(120):
            if subprocess.run(
                ["docker", "exec", pg_name, "pg_isready", "-h", "127.0.0.1",
                 "-U", "githearth", "-d", "githearth"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            ).returncode == 0:
                break
            time.sleep(0.5)
        else:
            raise HarnessError("s8 postgres startup timed out")

        hp = self.free_port()
        sp = self.free_port()
        db_url = "postgres://githearth:" + self.db_password + "@" + pg_name + ":5432/githearth"

        # migrate and bootstrap-admin need only the database. Storage is a small tmpfs
        # inside the server container: never fill a docker volume, which lives on the host disk.
        subprocess.run([
            "docker", "run", "--rm", "--network", net,
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            img, "migrate",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        subprocess.run([
            "docker", "run", "--rm", "--network", net, "-i",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            img, "bootstrap-admin", "--username", self.admin_username,
        ], input=self.admin_password, text=True,
           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        # Start serve
        subprocess.run([
            "docker", "run", "--detach", "--rm", "--name", srv_name,
            "--network", net,
            "--tmpfs", "/var/lib/githearth:size=32m,uid=10001,gid=10001,mode=0700",
            "--entrypoint", "sh",
            "-p", f"127.0.0.1:{hp}:8080",
            "-p", f"127.0.0.1:{sp}:2222",
            "-e", "GITHEARTH_DATABASE_URL=" + db_url,
            "-e", "GITHEARTH_SSH_ENABLED=true",
            "-e", "GITHEARTH_BIND=0.0.0.0:8080",
            "-e", "GITHEARTH_SSH_BIND=0.0.0.0:2222",
            "-e", "GITHEARTH_RECONCILE_INTERVAL_SECONDS=5",
            "-e", "GITHEARTH_RECONCILE_GRACE_SECONDS=30",
            "-e", "GITHEARTH_SECURE_COOKIES=false",
            img, "-c", "githearth-server ssh-keygen && exec githearth-server serve",
        ], check=True, stdout=subprocess.DEVNULL, timeout=30)

        base = f"http://127.0.0.1:{hp}"
        for _ in range(300):
            try:
                st, _ = self.api_json("GET", "/health/ready", base=base)
                if st == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.1)
        else:
            raise HarnessError("s8 server readiness timed out")

        tok = self.api_session(self.admin_username, self.admin_password, base=base)
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "s8-org", "name": "S8 Org"}, token=tok, base=base)
        if st != 201:
            raise HarnessError(f"s8 create org status {st}")
        org_slug = org["slug"]
        st, repo = self.api_json(
            "POST", f"/api/v1/organizations/{org_slug}/repositories",
            {"name": "s8-repo", "visibility": "private"}, token=tok, base=base)
        if st != 201:
            raise HarnessError(f"s8 create repo status {st}")
        repo_name = repo["name"]
        http_url = f"http://127.0.0.1:{hp}/{org_slug}/{repo_name}.git"
        s8_ssh = f"ssh://git@127.0.0.1:{sp}/{org_slug}/{repo_name}.git"

        # Register SSH key
        skey = self.tmp / "s8_ssh_key"
        _, spub = self.ssh_keygen(skey)
        self.api_register_ssh_key(tok, spub, base=base)

        # Seed push over HTTP
        w = self.tmp / "s8-work"; w.mkdir()
        subprocess.run(["git", "init", "-b", "main"], cwd=str(w),
                       env=self.git_env(), capture_output=True, check=True)
        (w / "seed.txt").write_text("s8\n")
        subprocess.run(["git", "-C", str(w), "config", "user.name", "S8"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "config", "user.email", "s8@t"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "add", "-A"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "commit", "-m", "seed"], capture_output=True)
        penv = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", http_url, "main"],
            env=penv, capture_output=True, text=True, check=True, timeout=120)

        # Remember pre-fill refs
        pre_ref = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "ls-remote", http_url],
            env=penv, capture_output=True, text=True, check=True).stdout.strip()

        # Fill the volume to near-full (leave ~100KB free)
        subprocess.run(
            ["docker", "exec", srv_name, "sh", "-c",
             # Guard: refuse to fill anything but the small tmpfs (64 MiB max).
             "total=$(df --output=size /var/lib/githearth | tail -1); "
             "[ \"$total\" -le 65536 ] || { echo refusing to fill $total KiB >&2; exit 3; }; "
             "avail=$(df --output=avail /var/lib/githearth | tail -1); "
             "target=$((avail > 100 ? avail - 100 : 0)); "
             "dd if=/dev/zero of=/var/lib/githearth/filler bs=1K count=$target 2>/dev/null"],
            check=True, capture_output=True, text=True, timeout=300)

        # Push large file over HTTP — must fail
        data = os.urandom(5 * 1024 * 1024)  # 5 MiB
        (w / "big.bin").write_bytes(data)
        subprocess.run(["git", "-C", str(w), "add", "big.bin"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "big disk"], capture_output=True)
        hp_proc = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", http_url, "HEAD:refs/heads/big-http"],
            env=penv, capture_output=True, text=True, timeout=120)
        if hp_proc.returncode == 0:
            raise HarnessError(f"s8: HTTP push on full disk should fail: rc=0, stderr={hp_proc.stderr[:200]}")
        post_ref = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "ls-remote", http_url],
            env=penv, capture_output=True, text=True, check=True).stdout.strip()
        if pre_ref != post_ref:
            raise HarnessError("s8: ref moved during HTTP disk-full push")

        # Push large file over SSH — must fail
        (w / "big2.bin").write_bytes(data)
        subprocess.run(["git", "-C", str(w), "add", "big2.bin"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "big disk ssh"], capture_output=True)
        ssh_e = self.ssh_git_env(skey)
        ss_proc = subprocess.run(
            ["git", "-C", str(w), "push", s8_ssh, "HEAD:refs/heads/big-ssh"],
            env=ssh_e, capture_output=True, text=True, timeout=120)
        if ss_proc.returncode == 0:
            raise HarnessError("s8: SSH push on full disk should fail")
        # The refusal must come from the server, not from never reaching it.
        if any(m in ss_proc.stderr for m in ("Connection refused", "Permission denied (publickey")):
            raise HarnessError(f"s8: SSH push never reached the server: {ss_proc.stderr[:300]}")
        post_ref2 = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "ls-remote", http_url],
            env=penv, capture_output=True, text=True, check=True).stdout.strip()
        if pre_ref != post_ref2:
            raise HarnessError("s8: ref moved during SSH disk-full push")

        # Server still alive
        st, _ = self.api_json("GET", "/health/ready", base=base)
        if st != 200:
            raise HarnessError(f"s8: server not healthy after disk full: {st}")

        # doctor reports disk warn/fail
        p = subprocess.run(
            ["docker", "exec", "-e", "GITHEARTH_DATABASE_URL=" + db_url,
             srv_name, "githearth-server", "doctor"],
            capture_output=True, text=True, timeout=30)
        out = p.stdout + p.stderr
        if "ok   disk" in out:
            raise HarnessError(f"s8: doctor disk check should warn/fail, not ok: {out[:300]}")

        # Integrity check: copy storage out and run CLI check
        stg = self.tmp / "s8-storage-copy"; stg.mkdir()
        # docker cp cannot see into a tmpfs mount; stream it out with tar instead.
        tar_out = subprocess.run(
            ["docker", "exec", srv_name, "tar", "-C", "/var/lib/githearth", "-cf", "-", "."],
            check=True, capture_output=True, timeout=60).stdout
        subprocess.run(["tar", "-C", str(stg), "-xf", "-"], input=tar_out, check=True)
        p = subprocess.run([self.cli_binary, "check", str(stg)], capture_output=True, text=True)
        if p.returncode != 0:
            raise HarnessError(f"s8: githearth check exited {p.returncode}: {p.stderr[:300]}")

        # Delete filler, push succeeds
        subprocess.run(
            ["docker", "exec", srv_name, "rm", "/var/lib/githearth/filler"],
            check=True, capture_output=True, text=True, timeout=10)

        # Small push over HTTP
        (w / "recovered.txt").write_text("recovered\n")
        subprocess.run(["git", "-C", str(w), "add", "recovered.txt"], capture_output=True)
        subprocess.run(["git", "-C", str(w), "-c", "user.name=T", "-c", "user.email=t@t",
                         "commit", "-m", "recovered"], capture_output=True)
        r_proc = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", http_url, "HEAD:refs/heads/recovered"],
            env=penv, capture_output=True, text=True, timeout=120)
        if r_proc.returncode != 0:
            raise HarnessError(f"s8: push after filler removal failed: {r_proc.stderr[:300]}")

        # Clone and check refs
        cf = self.tmp / "s8-clone-final"; cf.mkdir()
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}", "clone", http_url, str(cf)],
            env=penv, capture_output=True, text=True, check=True, timeout=120)
        # A fresh clone has only main as a local branch; recovered is a remote-tracking ref.
        final_refs = subprocess.run(
            ["git", "-C", str(cf), "show-ref"],
            capture_output=True, text=True, check=True).stdout.strip()
        if "refs/remotes/origin/recovered" not in final_refs:
            raise HarnessError("s8: recovered ref not in clone")

        shutil.rmtree(str(w), ignore_errors=True)
        shutil.rmtree(str(stg), ignore_errors=True)
        shutil.rmtree(str(cf), ignore_errors=True)

    # ================================================================== S9
    def s9_uninstall_drill(self):
        self.stop_server()
        if self._old_db:
            self.docker(["exec", self.name, "dropdb", "-U", "githearth", "--if-exists", self._old_db],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self._old_db = None
        self.generate_ssh_host_key()
        self.start_server(MAIN_DB, self.storage_root, "pre-s9")
        tok = self.api_session(self.admin_username, self.admin_password)
        st, raw = self.api_json(
            "GET", f"/api/v1/organizations/{self.org_slug}/repositories", token=tok)
        if st != 200:
            raise HarnessError(f"s9: list repos status {st}")
        repo_entries = raw if isinstance(raw, list) else raw.get("items", [])
        st, issue = self.api_json(
            "POST",
            f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues",
            {"title": "S9 Uninstall Test", "body": "created before uninstall drill"},
            token=tok)
        if st != 201:
            raise HarnessError(f"s9: create issue status {st}")
        self._s9_issue_number = issue["number"]
        mirror_dir = self.tmp / "mirrors"; mirror_dir.mkdir()
        mir_refs = {}
        for r in repo_entries:
            rname = r["name"]
            md = mirror_dir / f"{rname}.git"
            self.git("clone", "--mirror", self._repo_http_url(rname), str(md),
                     user=self.admin_username, password=self.admin_password, label=f"s9-mirror-{rname}")
            refs_out = subprocess.run(
                ["git", "-C", str(md), "ls-remote", "--heads", "--tags", "."],
                capture_output=True, text=True, check=True).stdout.strip()
            mir_refs[rname] = refs_out
        hp_before = self.server.http_port
        sp_before = self.ssh_port
        self.stop_server()
        # keep-data variant: delete binary copy and config, then reinstall
        config_env = self.server_env(MAIN_DB, self.storage_root)
        binary_copy = self.tmp / "githearth-server-copy"
        shutil.copy2(self.binary, binary_copy)
        self.binary = str(binary_copy)
        subprocess.run([self.binary, "migrate"], env=config_env,
                       check=True, capture_output=True, text=True)
        subprocess.run([self.binary, "doctor"], env=config_env,
                       check=True, capture_output=True, text=True)
        self.generate_ssh_host_key()
        self.start_server(MAIN_DB, self.storage_root, "keep-data-reinstall")
        for rname, expected_refs in mir_refs.items():
            actual = self.git("ls-remote", "--heads", "--tags", self._repo_http_url(rname),
                              user=self.admin_username, password=self.admin_password,
                              label=f"s9-ls-{rname}").stdout.strip()
            if not actual:
                raise HarnessError(f"s9 keep-data: ls-remote empty for {rname}")
            actual_refs = "\n".join(sorted(actual.splitlines()))
            expected_sorted = "\n".join(sorted(expected_refs.splitlines()))
            if actual_refs != expected_sorted:
                raise HarnessError(f"s9 keep-data: refs mismatch for {rname}\nexpected:{expected_refs[:200]}\nactual:{actual[:200]}")
        st, iss = self.api_json(
            "GET",
            f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues/{self._s9_issue_number}",
            token=self.api_session(self.admin_username, self.admin_password))
        if st != 200 or iss.get("title") != "S9 Uninstall Test":
            raise HarnessError(f"s9: keep-data issues not readable: {st}")
        self.stop_server()
        # full removal
        for rname, expected_refs in mir_refs.items():
            md = mirror_dir / f"{rname}.git"
            refs_now = subprocess.run(
                ["git", "-C", str(md), "ls-remote", "--heads", "--tags", "."],
                capture_output=True, text=True, check=True).stdout.strip()
            if refs_now != expected_refs:
                raise HarnessError(f"s9: mirror refs changed after keep-data for {rname}")
            fresh = self.tmp / f"s9-fresh-{uuid.uuid4().hex[:6]}"
            subprocess.run(
                ["git", "clone", str(md), str(fresh)],
                capture_output=True, text=True, check=True)
            fresh_refs = subprocess.run(
                ["git", "-C", str(fresh), "ls-remote", "--heads", "--tags", "origin"],
                capture_output=True, text=True, check=True).stdout.strip()
            if fresh_refs != expected_refs:
                raise HarnessError(f"s9: fresh mirror clone refs mismatch for {rname}")
            shutil.rmtree(str(fresh), ignore_errors=True)
        shutil.rmtree(str(self.storage_root), ignore_errors=True)
        self.storage_root.mkdir()
        remaining = list(self.storage_root.iterdir())
        if remaining:
            raise HarnessError(f"s9: storage root not empty: {[p.name for p in remaining]}")
        self.docker(["exec", self.name, "dropdb", "-U", "githearth", MAIN_DB],
                    capture_output=True, text=True)
        db_gone = subprocess.run(
            ["docker", "exec", self.name, "psql",
             "-U", "githearth", "-d", "template1", "-t", "-A", "-v", "ON_ERROR_STOP=1"],
            input="SELECT 1 FROM pg_database WHERE datname='" + MAIN_DB + "'",
            text=True, capture_output=True).stdout.strip()
        if db_gone.strip():
            raise HarnessError(f"s9: database {MAIN_DB} still present after drop")
        try:
            s = socket.create_connection(("127.0.0.1", hp_before), timeout=1)
            s.close()
            raise HarnessError("s9: HTTP port still accepting after full removal")
        except (ConnectionRefusedError, OSError):
            pass
        if sp_before is not None:
            try:
                s = socket.create_connection(("127.0.0.1", sp_before), timeout=1)
                s.close()
                raise HarnessError("s9: SSH port still accepting after full removal")
            except (ConnectionRefusedError, OSError):
                pass
        server_procs = subprocess.run(
            ["pgrep", "-f", "githearth-server serve"],
            capture_output=True, text=True).stdout.strip()
        if server_procs:
            leftover = []
            for pid_str in server_procs.splitlines():
                try:
                    exe = os.readlink(f"/proc/{pid_str}/exe")
                except OSError:
                    exe = ""
                if self.binary in exe or "githearth-server" in exe:
                    ns_check = subprocess.run(
                        ["cat", f"/proc/{pid_str}/cgroup"],
                        capture_output=True, text=True).stdout.strip()
                    if "/docker/" not in ns_check:
                        leftover.append(pid_str)
            if leftover:
                raise HarnessError(f"s9: server processes still running: {','.join(leftover)}")

    def _repo_http_url(self, repo_name):
        return f"http://127.0.0.1:{self.server.http_port}/{self.org_slug}/{repo_name}.git"

    # ================================================================== runner

    def run(self):
        signal.signal(signal.SIGINT, lambda *_a: sys.exit(1))
        signal.signal(signal.SIGTERM, lambda *_a: sys.exit(1))
        try:
            self.start_db()
            self.scenario("s1_install_from_runbook", self.s1_install_from_runbook)
            self.scenario("s2_auth_agreement", self.s2_auth_agreement)
            self.scenario("s3_quota", self.s3_quota)
            self.scenario("s4_backup_restore", self.s4_backup_restore)
            self.scenario("s5_upgrade_drill", self.s5_upgrade_drill)
            self.scenario("s6_restart_jobs", self.s6_restart_jobs)
            self.scenario("s7_container_install", self.s7_container_install)
            self.scenario("s8_disk_full", self.s8_disk_full)
            self.scenario("s9_uninstall_drill", self.s9_uninstall_drill)
        finally:
            self.cleanup()
        self.write_report()
        ps = sum(1 for s in self.scenarios if s["result"] == "PASS")
        print(f"\nM4 verification: {ps}/{len(self.scenarios)} scenarios passed")
        print(f"report: {self.report_path}")
        return 0 if ps == len(self.scenarios) else 1

    def write_report(self):
        info = {}
        info["git_version"] = subprocess.run(
            ["git", "--version"], capture_output=True, text=True).stdout.strip()
        info["source_revision"] = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
        info["os"] = platform.platform() + " " + platform.machine()
        info["upgrade_from_rev"] = self._from_rev
        report = {"environment": info, "scenarios": self.scenarios,
                  "git_operations": self.git_ops, "server_lifetimes": self.lifetimes,
                  "authorization_matrix": self.auth_matrix, "measurements": {}}
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)

    def cleanup(self):
        # Kill all tracked subprocesses first
        for proc in self._extra_procs:
            _kill_proc(proc)
        self._extra_procs.clear()
        super().cleanup()
        if self._old_worktree and Path(self._old_worktree).exists():
            subprocess.run(
                ["git", "worktree", "remove", "--force", self._old_worktree],
                cwd=ROOT, capture_output=True)
        if self._old_target_dir and Path(self._old_target_dir).exists():
            shutil.rmtree(self._old_target_dir, ignore_errors=True)
        # s7/s8 docker resources: containers (with anonymous volumes) first, since a
        # network or image in use cannot be removed; then volumes, networks, the image.
        steps = [
            (("_s7_pg", "_s7_srv", "_s7_srv2", "_s8_pg", "_s8_srv"), ["rm", "--force", "--volumes"]),
            (("_s7_vol",), ["volume", "rm", "--force"]),
            (("_s7_net", "_s8_net"), ["network", "rm"]),
            (("_s7_img",), ["rmi", "--force"]),
        ]
        for names, cmd in steps:
            for name in names:
                val = getattr(self, name, None)
                if val:
                    subprocess.run(["docker"] + cmd + [val],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


# ---- module-level helpers (not on the class, shared by upgrade) -----------

def _build_cli():
    subprocess.run(["cargo", "build", "-p", "githearth-cli", "--release"],
                   cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    td = os.environ.get("CARGO_TARGET_DIR")
    base = Path(td) if td else (ROOT / "target")
    b = base / "release" / "githearth"
    if not b.exists(): raise HarnessError("CLI binary not found after build")
    return str(b)


def _build_old(harness):
    wd = harness.tmp / "old-worktree"; harness._old_worktree = str(wd)
    wt = harness.tmp / "old-target"; harness._old_target_dir = str(wt)
    subprocess.run(
        ["git", "worktree", "add", str(wd), harness._from_rev],
        cwd=ROOT, check=True, capture_output=True, timeout=60)
    subprocess.run(
        ["cargo", "build", "-p", "githearth-server", "--release"],
        cwd=str(wd),
        env={**os.environ, "CARGO_BUILD_JOBS": "4", "CARGO_TARGET_DIR": str(wt)},
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=900)
    ob = Path(wt) / "release" / "githearth-server"
    if not ob.exists(): raise HarnessError("old binary not found after build")
    return str(ob)


def _wait_ssh_log(harness, proc, phase):
    cur = len(_log(harness).splitlines())
    dl = time.time() + 30
    while time.time() < dl:
        if proc.poll() is not None:
            raise HarnessError(f"{phase} server exited before SSH log line")
        txt = _log(harness); lines = txt.splitlines()
        for ln in lines[cur:]:
            try: rec = json.loads(ln)
            except (json.JSONDecodeError, ValueError): continue
            if rec.get("fields", {}).get("message", "") == "SSH server listening":
                return
        cur = len(lines); time.sleep(0.1)
    raise HarnessError(f"{phase}: SSH listening log line not seen within 30s")


def _last_ssh_port(harness):
    result = None
    for ln in _log(harness).splitlines():
        try: rec = json.loads(ln)
        except (json.JSONDecodeError, ValueError): continue
        if rec.get("fields", {}).get("message", "") == "SSH server listening":
            b = rec.get("fields", {}).get("bind", "")
            m = re.search(r"(\d+\.\d+\.\d+\.\d+):(\d+)", str(b))
            if m: result = int(m.group(2))
    return result


def _log(harness):
    try: return harness.log_path.read_text(errors="replace")
    except: return ""


def _read_ssh_port_from_log(log_path):
    for ln in log_path.read_text(errors="replace").splitlines():
        try: rec = json.loads(ln)
        except (json.JSONDecodeError, ValueError): continue
        if rec.get("fields", {}).get("message", "") == "SSH server listening":
            b = rec.get("fields", {}).get("bind", "")
            m = re.search(r"(\d+\.\d+\.\d+\.\d+):(\d+)", str(b))
            if m: return int(m.group(2))
    return None


def _wait_http(proc, harness, base, label, timeout=120):
    for _ in range(timeout * 10):
        if proc.poll() is not None:
            raise HarnessError(f"{label} server exited during startup")
        try:
            st, _, _ = harness.api("GET", "/health/ready", base=base)
            if st == 200: return
        except (OSError, urllib.error.URLError): pass
        time.sleep(0.1)
    raise HarnessError(f"{label} server readiness timed out")


def _container_tail(name):
    """Container state and last log lines, for a failure message."""
    st = subprocess.run(["docker", "inspect", name, "--format", "{{.State.Status}}"],
                        capture_output=True, text=True).stdout.strip()
    logs = subprocess.run(["docker", "logs", "--tail", "15", name],
                          capture_output=True, text=True)
    return f"container {name} {st or 'gone'}:\n{(logs.stdout + logs.stderr)[-2500:]}"


def _kill_proc(proc):
    if proc is None or proc.poll() is not None: return
    proc.terminate()
    try: proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        try: proc.wait(timeout=5)
        except subprocess.TimeoutExpired: pass


def _classify(proc):
    if proc.returncode == 0: return "ALLOWED"
    lo = proc.stderr.lower()
    if "not found" in lo or "not-found" in lo: return "DENIED (not-found)"
    if "forbidden" in lo: return "DENIED (forbidden)"
    if "permission" in lo: return "DENIED (permission)"
    if "authentication" in lo: return "DENIED (auth)"
    return f"DENIED ({proc.stderr.strip()[:80]})"


def _sq(s):
    return "'" + s.replace("'", "''") + "'"


def main():
    p = argparse.ArgumentParser(description="Verify M4 qualification end to end.")
    p.add_argument("--report", help="write measurements JSON to this path")
    a = p.parse_args()
    rp = a.report
    if rp is None:
        fd, rp = tempfile.mkstemp(prefix="m4-report-", suffix=".json"); os.close(fd)
    h = M4Harness(rp); sys.exit(h.run())


if __name__ == "__main__":
    main()
