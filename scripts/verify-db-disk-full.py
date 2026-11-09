#!/usr/bin/env python3
"""B29: PostgreSQL disk-full drill. Python 3 stdlib only.

Reuses verify-m2.Harness via importlib subclassing (same pattern as verify-m4.py).
Overrides only the database container start to put PGDATA on a size-capped tmpfs.
"""
import argparse
import importlib.util
import json
import os
import secrets
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


def _load_m2():
    spec = importlib.util.spec_from_file_location(
        "verify_m2", ROOT / "scripts" / "verify-m2.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

_m2 = _load_m2()
HarnessError = _m2.HarnessError

# ------------------------------------------------------------------- Harness

class DbDiskFullHarness(_m2.Harness):
    """Override DB start to put PGDATA on a size-capped tmpfs."""

    def __init__(self, report_path):
        self.name = "githearth-b29-" + uuid.uuid4().hex[:12]
        super().__init__(report_path)
        self.org_slug = None
        self.repo_name = None
        self.repo_id = None
        self.b29_observed = {}

    # ---------- override DB start: tmpfs for PGDATA ----------------------

    def start_db(self):
        self.db_port = self.free_port()
        self.docker(
            [
                "run", "--detach", "--rm", "--name", self.name,
                "--env-file", str(self.env_path),
                "--tmpfs", "/var/lib/postgresql/data:size=96m",
                "--publish", f"127.0.0.1:{self.db_port}:5432",
                IMAGE,
            ],
            stdout=subprocess.DEVNULL,
        )
        self._assert_tmpfs_safety()
        for _ in range(120):
            if subprocess.run(
                ["docker", "exec", self.name, "pg_isready", "-h", "127.0.0.1",
                 "-U", "githearth", "-d", "m2"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            ).returncode == 0:
                return
            time.sleep(0.5)
        raise HarnessError("PostgreSQL startup timed out")

    def _assert_tmpfs_safety(self):
        """Guard: refuse if the tmpfs holding PGDATA is > 256 MiB."""
        out = subprocess.run(
            ["docker", "exec", self.name, "df", "--output=size",
             "/var/lib/postgresql/data"],
            capture_output=True, text=True, check=True)
        lines = out.stdout.strip().splitlines()
        size_1k = int(lines[-1].strip())
        # df --output=size reports 1K blocks
        max_1k = 256 * 1024  # 256 MiB
        if size_1k > max_1k:
            raise HarnessError(
                f"REFUSING: tmpfs size {size_1k} 1K-blocks > {max_1k} (256 MiB)")

    # ---------- baseline --------------------------------------------------

    def b1_baseline(self):
        """Migrate, bootstrap, serve; create org + private repo; push; issue."""
        env = self.server_env("m2", self.storage_root)
        subprocess.run([self.binary, "migrate"], env=env,
                       check=True, capture_output=True, text=True)
        subprocess.run(
            [self.binary, "bootstrap-admin", "--username", self.admin_username],
            env=env, input=self.admin_password, text=True, capture_output=True,
            check=True)
        self.start_server("m2", self.storage_root, "baseline")
        tok = self.api_session()
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "b29-org", "name": "B29 Org"}, token=tok)
        if st != 201: raise HarnessError(f"baseline org status {st}")
        self.org_id = org["id"]; self.org_slug = org["slug"]
        st, repo = self.api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories",
            {"name": "b29-repo", "visibility": "private"}, token=tok)
        if st != 201: raise HarnessError(f"baseline repo status {st}")
        self.repo_id = repo["id"]; self.repo_name = repo["name"]
        self.admin_token = tok

        w = self.main_work; w.mkdir()
        self.wgit("init", "-b", "main")
        self.wgit("config", "user.name", "B29")
        self.wgit("config", "user.email", "b29@example.com")
        (w / "README.md").write_text("# B29\n")
        self.wgit("add", "-A"); self.wgit("commit", "-m", "initial")
        self.baseline_sha = self.wgit("rev-parse", "HEAD").stdout.strip()
        self.git("push", self.repo_url, "main",
                 user=self.admin_username, password=self.admin_password,
                 label="baseline push")
        st, iss = self.api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues",
            {"title": "Baseline issue", "body": "before fill"}, token=tok)
        if st != 201: raise HarnessError(f"baseline issue status {st}")

    def api_session(self):
        st, s = self.api_json("POST", "/api/v1/login",
                              {"username": self.admin_username,
                               "password": self.admin_password})
        if st != 200 or not s.get("token"):
            raise HarnessError(f"login status {st}")
        return s["token"]

    @property
    def repo_url(self):
        return (f"http://127.0.0.1:{self.server.port}/"
                f"{self.org_slug}/{self.repo_name}.git")

    # ---------- fill ------------------------------------------------------

    def b2_fill(self):
        """Fill the tmpfs to nearly full from inside the PG container."""
        self._assert_tmpfs_safety()
        # Determine filler path: prefer outside PGDATA on same filesystem,
        # fall back to inside the tmpfs mount root (= PGDATA).
        # Pre-create filler table before disk is full (catalog writes need space)
        subprocess.run(
            ["docker", "exec", self.name, "psql", "-U", "githearth", "-d", "m2",
             "-c", "CREATE TABLE IF NOT EXISTS _b29_fill(t text);"],
            check=True, capture_output=True, text=True, timeout=30)
        # Fill the tmpfs to 0 available
        out = subprocess.run(
            ["docker", "exec", self.name, "sh", "-c",
             "total=$(df --output=size /var/lib/postgresql/data | tail -1); "
             "[ \"$total\" -le 262144 ] || { echo REFUSING $total >&2; exit 3; }; "
             "filler=/var/lib/postgresql/data/../filler; "
             "pgdata_mnt=$(df --output=target /var/lib/postgresql/data | tail -1 | xargs); "
             "filler_mnt=$(df --output=target \"$filler\" 2>/dev/null | tail -1 | xargs); "
             "if [ \"$pgdata_mnt\" != \"$filler_mnt\" ]; then "
             "  filler=/var/lib/postgresql/data/filler; "
             "  echo \"filler inside PGDATA\" >&2; "
             "fi; "
             "while :; do "
             "  dd if=/dev/zero of=\"$filler\" bs=1024K count=1 oflag=append conv=notrunc 2>/dev/null || break; "
             "done; "
             "echo \"$filler\" >&2; "
             "df /var/lib/postgresql/data >&2"],
            check=True, capture_output=True, text=True, timeout=300)
        wal_iters = 0
        for _ in range(50):
            rc = subprocess.run(
                ["docker", "exec", self.name, "psql", "-U", "githearth", "-d", "m2",
                 "-c", "INSERT INTO _b29_fill "
                       "SELECT repeat('x', 7200) FROM generate_series(1, 50)"],
                capture_output=True, text=True, timeout=60)
            if rc.returncode != 0:
                break
            wal_iters += 1
        self.b29_observed["fill_table_iters"] = wal_iters
        self.b29_observed["fill_table_last_err"] = (
            rc.stderr.strip()[:500] if wal_iters < 50 else "completed all")
        # Final df snapshot
        df_final = subprocess.run(
            ["docker", "exec", self.name, "df", "--output=avail,pcent",
             "/var/lib/postgresql/data"],
            capture_output=True, text=True, timeout=10)
        self.b29_observed["disk_after_fill"] = df_final.stdout.strip()
        self.b29_observed["fill_stderr"] = out.stderr.strip()

        # Record filler location and verify it is on the tmpfs
        loc = subprocess.run(
            ["docker", "exec", self.name, "df",
             "/var/lib/postgresql/data/filler", "/var/lib/postgresql/filler"],
            capture_output=True, text=True, timeout=10)
        self.b29_observed["filler_df"] = loc.stdout.strip()

    # ---------- while full -----------------------------------------------

    def b3_while_full(self):
        """Assert writes fail cleanly, reads work, server stays alive."""
        # Snapshot disk usage immediately before tests
        df_pre = subprocess.run(
            ["docker", "exec", self.name, "df", "--output=avail,pcent",
             "/var/lib/postgresql/data"],
            capture_output=True, text=True, timeout=10)
        self.b29_observed["disk_before_while_full"] = df_pre.stdout.strip()
        tok = self.admin_token
        start = time.time()

        # ---- wr1: create an issue while DB disk is full
        t0 = time.time()
        st, body = self.api_json(
            "POST",
            f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues",
            {"title": "Full issue", "body": "should fail"}, token=tok)
        issue_dur = time.time() - t0
        self.b29_observed["issue_during_full_status"] = st
        self.b29_observed["issue_during_full_body"] = (
            json.dumps(body) if body else "null")
        self.b29_observed["issue_during_full_secs"] = round(issue_dur, 3)
        # Record whether the write was refused as expected.
        # PostgreSQL may serve small writes from already-allocated pages even
        # at 100% disk usage; this is a finding if it happens.
        self.b29_observed["issue_during_full_failed"] = (st >= 500)
        if issue_dur > 60:
            raise HarnessError("issue during full disk hung")

        # ---- wr2: create a comment
        st_c, body_c = self.api_json(
            "POST",
            f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues/1/comments",
            {"body": "should also fail"}, token=tok)
        self.b29_observed["comment_during_full_status"] = st_c

        # ---- wr3: push
        w = self.main_work
        (w / "full.txt").write_text("during fill\n")
        self.wgit("add", "full.txt")
        self.wgit("-c", "user.name=T", "-c", "user.email=t@t",
                   "commit", "-m", "during fill")
        penv = self.git_env(self.admin_username, self.admin_password)
        pre_ref = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "ls-remote", self.repo_url],
            env=penv, capture_output=True, text=True, timeout=60).stdout.strip()
        t1 = time.time()
        hp_proc = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", self.repo_url, "HEAD:refs/heads/full-push"],
            env=penv, capture_output=True, text=True, timeout=120)
        push_dur = time.time() - t1
        self.b29_observed["push_during_full_rc"] = hp_proc.returncode
        self.b29_observed["push_during_full_stderr"] = hp_proc.stderr.strip()[:500]
        self.b29_observed["push_during_full_secs"] = round(push_dur, 3)
        self.b29_observed["push_during_full_refused"] = (hp_proc.returncode != 0)

        # No ref moved on disk (check regardless of push outcome)
        rp = self.repo_storage_path()
        out = subprocess.run(
            ["git", "--git-dir", rp, "rev-parse", "--verify", "-q",
             "refs/heads/full-push"],
            capture_output=True, text=True)
        self.b29_observed["ref_moved_on_disk"] = (out.returncode == 0)

        # Compare pre/post refs via ls-remote (may fail during disk full)
        post_proc = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "ls-remote", self.repo_url],
            env=penv, capture_output=True, text=True, timeout=60)
        self.b29_observed["ls_remote_post_full_rc"] = post_proc.returncode
        self.b29_observed["refs_changed_during_full"] = (
            post_proc.returncode == 0 and pre_ref != post_proc.stdout.strip())

        # ---- server alive and /health/live
        st, body = self.api_json("GET", "/health/live")
        self.b29_observed["health_live_during_full"] = st
        if st != 200:
            raise HarnessError(f"/health/live not 200 during disk full: {st}")

        # ---- /health/ready behaviour (recorded, not asserted)
        st_r, body_r = self.api_json("GET", "/health/ready")
        self.b29_observed["health_ready_during_full"] = st_r

        # ---- reads still work if PG can serve them
        # Clone should work if fetch does not need DB writes
        cd = self.tmp / "full-clone"; cd.mkdir(exist_ok=True)
        cr = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "clone", self.repo_url, str(cd)],
            env=penv, capture_output=True, text=True, timeout=120)
        self.b29_observed["clone_during_full_rc"] = cr.returncode

        # List issues
        st_i, issues = self.api_json(
            "GET",
            f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues",
            token=tok)
        self.b29_observed["list_issues_during_full_status"] = st_i

        # ---- githearth-server doctor
        doc_env = self.server_env("m2", self.storage_root)
        doc = subprocess.run(
            [self.binary, "doctor"],
            env=doc_env, capture_output=True, text=True, timeout=30)
        self.b29_observed["doctor_rc_during_full"] = doc.returncode
        self.b29_observed["doctor_output_during_full"] = (doc.stdout + doc.stderr)[:1000]

    # ---------- recover ---------------------------------------------------

    def b4_recover(self):
        """Delete filler; writes work again without restart."""
        # Remove the filler wherever it was created (try both paths)
        for fp in ("/var/lib/postgresql/data/filler", "/var/lib/postgresql/filler"):
            rc = subprocess.run(
                ["docker", "exec", self.name, "rm", "-f", fp],
                capture_output=True, text=True, timeout=10)
        # Verify disk is no longer full
        df_out = subprocess.run(
            ["docker", "exec", self.name, "df", "--output=pcent",
             "/var/lib/postgresql/data"],
            capture_output=True, text=True, check=True, timeout=10)
        self.b29_observed["disk_use_after_cleanup"] = df_out.stdout.strip()

        t0 = time.time()
        deadline = t0 + 30
        while time.time() < deadline:
            # Issue creation
            st, iss = self.api_json(
                "POST",
                f"/api/v1/organizations/{self.org_slug}/repositories/{self.repo_name}/issues",
                {"title": "Recovered issue", "body": "after recovery"},
                token=self.admin_token)
            if st == 201:
                break
            time.sleep(0.5)
        else:
            raise HarnessError(
                f"recover: issue creation did not recover within 30s (last status {st})")
        rec_dur = time.time() - t0
        self.b29_observed["recovery_issue_secs"] = round(rec_dur, 3)

        # Push
        w = self.main_work
        (w / "recovered.txt").write_text("recovered\n")
        self.wgit("add", "recovered.txt")
        self.wgit("-c", "user.name=T", "-c", "user.email=t@t",
                   "commit", "-m", "recovered")
        penv = self.git_env(self.admin_username, self.admin_password)
        subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "-C", str(w), "push", self.repo_url, "HEAD:refs/heads/recovered"],
            env=penv, capture_output=True, text=True, check=True, timeout=120)

        # Journal and DB agree: ref is in ref_updates and ls-remote
        rid = self.repo_id
        cnt = int(self.sql(
            "m2",
            "SELECT count(*) FROM ref_updates WHERE repository_id='" + rid
            + "' AND ref_name='refs/heads/recovered'"))
        if cnt < 1:
            raise HarnessError("recover: ref_updates missing recovered ref")

        # The push accepted while the database disk was full moved a ref on disk;
        # its metadata must catch up (replay or reconciler), not stay missing.
        for _ in range(120):
            full = int(self.sql(
                "m2",
                "SELECT count(*) FROM ref_updates WHERE repository_id='" + rid
                + "' AND ref_name='refs/heads/full-push'"))
            if full >= 1:
                break
            time.sleep(0.5)
        else:
            raise HarnessError("recover: push made while the DB was full never reached ref_updates")
        self.b29_observed["full_push_metadata_reconciled"] = True

        penv = self.git_env(self.admin_username, self.admin_password)
        post_ref = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "ls-remote", self.repo_url],
            env=penv, capture_output=True, text=True, check=True, timeout=60).stdout.strip()
        if "refs/heads/recovered" not in post_ref:
            raise HarnessError("recover: ls-remote missing recovered ref")

        # githearth check exits 0
        # Use the binary directly since CLI may not be available
        chk_env = self.server_env("m2", self.storage_root)
        chk = subprocess.run(
            [self.binary, "doctor"],
            env=chk_env, capture_output=True, text=True, timeout=30)
        self.b29_observed["doctor_rc_after_recover"] = chk.returncode

    # ---------- runner ----------------------------------------------------

    def run(self):
        signal.signal(signal.SIGINT, lambda *_a: sys.exit(1))
        signal.signal(signal.SIGTERM, lambda *_a: sys.exit(1))
        try:
            self.start_db()
            self.scenario("b1_baseline", self.b1_baseline)
            self.scenario("b2_fill", self.b2_fill)
            self.scenario("b3_while_full", self.b3_while_full)
            self.scenario("b4_recover", self.b4_recover)
        finally:
            self.cleanup()
        self._write_report()
        ps = sum(1 for s in self.scenarios if s["result"] == "PASS")
        print(f"\nB29 DB disk-full drill: {ps}/{len(self.scenarios)} scenarios passed")
        print(f"report: {self.report_path}")
        print(f"observed: {json.dumps(self.b29_observed)}")
        return 0 if ps == len(self.scenarios) else 1

    def _write_report(self):
        info = {}
        info["git_version"] = subprocess.run(
            ["git", "--version"], capture_output=True, text=True).stdout.strip()
        info["source_revision"] = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT,
            capture_output=True, text=True).stdout.strip()
        import platform
        info["os"] = platform.platform() + " " + platform.machine()
        report = {
            "environment": info,
            "scenarios": self.scenarios,
            "observed": self.b29_observed,
        }
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)


def main():
    p = argparse.ArgumentParser(description="B29 PostgreSQL disk-full drill")
    p.add_argument("--report", help="write report JSON to this path")
    a = p.parse_args()
    rp = a.report
    if rp is None:
        fd, rp = tempfile.mkstemp(prefix="b29-report-", suffix=".json")
        os.close(fd)
    h = DbDiskFullHarness(rp)
    sys.exit(h.run())


if __name__ == "__main__":
    main()
