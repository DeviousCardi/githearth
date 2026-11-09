#!/usr/bin/env python3
"""B28 off-host backup drill: subclass verify-m2.Harness, start a disposable
backup host container, follow the runbook literally, simulate disaster, restore
from the off-host copy, and run a tamper check.

Python 3 stdlib only. Reuses verify-m2.py via importlib (as verify-m4.py does).
"""
import argparse
import importlib.util
import json
import os
import platform
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BACKUP_HOST_IMAGE = "githearth-backup-host:drill"
BACKUP_HOST_DOCKERFILE = """FROM debian:bookworm-slim
RUN apt-get update -qq \\
 && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends openssh-server rsync \\
 && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd
"""
BACKUP_HOSTNAME = "backup-host"
BACKUP_USER = "ghbackup"  # Debian already has a system user named backup
BACKUP_REMOTE_DIR = "/srv/backups/githearth"
BACKUP_SSH_PORT = None


class HarnessError(Exception):
    pass


def _load_m2():
    spec = importlib.util.spec_from_file_location(
        "verify_m2", ROOT / "scripts" / "verify-m2.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_m2 = _load_m2()


class B28Harness(_m2.Harness):
    def __init__(self, report_path):
        self.name = "githearth-b28-" + secrets.token_hex(6)
        super().__init__(report_path)
        self.backup_container = None
        self.backup_key = None
        self.backup_port = None
        self.backup_known_hosts = None
        self._extra_containers = []
        self._dump_path = None
        self._storage_tgz = None
        self._sums_path = None

    # ---- backup host infra ------------------------------------------------

    def _backup_ssh_opts(self):
        return [
            "-o", "StrictHostKeyChecking=yes",
            "-o", "UserKnownHostsFile=" + str(self.backup_known_hosts),
            "-o", "IdentitiesOnly=yes",
            "-o", "BatchMode=yes",
            "-o", "LogLevel=ERROR",
        ]

    def _backup_scp(self, src, remote_dst):
        subprocess.run(
            ["scp", "-P", str(self.backup_port), "-i", str(self.backup_key),
             "-o", "StrictHostKeyChecking=yes",
             "-o", "UserKnownHostsFile=" + str(self.backup_known_hosts),
             "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR",
             str(src),
             f"{BACKUP_USER}@127.0.0.1:{remote_dst}"],
            check=True, capture_output=True, text=True, timeout=120)

    def _backup_rsync(self, srcs, remote_dir):
        args = ["rsync", "-av",
                "-e", f"ssh -i {self.backup_key} -p {self.backup_port} -o StrictHostKeyChecking=yes -o UserKnownHostsFile={self.backup_known_hosts} -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR"]
        args.extend([str(s) for s in srcs])
        args.append(f"{BACKUP_USER}@127.0.0.1:{remote_dir}/")
        subprocess.run(args, check=True, capture_output=True, text=True, timeout=120)

    def _backup_ssh(self, command, check=True):
        return subprocess.run(
            ["ssh"] + self._backup_ssh_opts()
            + ["-p", str(self.backup_port), "-i", str(self.backup_key),
               f"{BACKUP_USER}@127.0.0.1", command],
            check=check, capture_output=True, text=True, timeout=60)

    def start_backup_host(self):
        bk = self.backup_key = self.tmp / "backup_ed25519"
        self.ssh_keygen(bk)
        pubkey = bk.with_suffix(".pub").read_text().strip()

        self.backup_known_hosts = self.tmp / "backup_known_hosts"

        self.backup_port = self.free_port()
        container_name = "githearth-bh-" + secrets.token_hex(6)
        self._extra_containers.append(container_name)

        # A small image with sshd and rsync, built once and then cached by Docker.
        # /run/sshd must exist or Debian's sshd exits at start.
        subprocess.run(
            ["docker", "build", "-q", "-t", BACKUP_HOST_IMAGE, "-"],
            input=BACKUP_HOST_DOCKERFILE, text=True, check=True,
            stdout=subprocess.DEVNULL, timeout=600)
        authorized = self.tmp / "backup_authorized_keys"
        authorized.write_text(pubkey + "\n")
        authorized.chmod(0o644)
        setup = (
            f"useradd -m -s /bin/bash {BACKUP_USER} && "
            f"install -d -o {BACKUP_USER} -m 700 /home/{BACKUP_USER}/.ssh {BACKUP_REMOTE_DIR} && "
            f"install -o {BACKUP_USER} -m 600 /keys/authorized_keys /home/{BACKUP_USER}/.ssh/ && "
            "ssh-keygen -A && "
            f"exec /usr/sbin/sshd -D -e -o ListenAddress=127.0.0.1:{self.backup_port} "
            "-o PasswordAuthentication=no -o PermitRootLogin=no"
        )
        # No --rm: if sshd dies, its logs explain why; cleanup removes the container.
        subprocess.run(
            ["docker", "run", "--detach", "--network", "host",
             "--name", container_name,
             "-v", f"{authorized}:/keys/authorized_keys:ro",
             BACKUP_HOST_IMAGE, "bash", "-c", setup],
            check=True, stdout=subprocess.DEVNULL)
        self.backup_container = container_name

        # Wait for sshd; fail at once (with its logs) if the container stops.
        for _ in range(60):
            state = subprocess.run(
                ["docker", "inspect", "-f", "{{.State.Running}}", container_name],
                capture_output=True, text=True).stdout.strip()
            if state != "true":
                logs = subprocess.run(["docker", "logs", "--tail", "20", container_name],
                                      capture_output=True, text=True)
                raise HarnessError(f"backup host exited: {(logs.stdout + logs.stderr)[-1500:]}")
            r = subprocess.run(
                ["ssh-keyscan", "-p", str(self.backup_port), "127.0.0.1"],
                capture_output=True, text=True, timeout=10)
            if r.returncode == 0 and r.stdout.strip():
                self.backup_known_hosts.write_text(r.stdout)
                break
            time.sleep(0.5)
        else:
            raise HarnessError("backup host sshd not reachable within 30 s")

        subprocess.run(
            ["ssh-keygen", "-H", "-f", str(self.backup_known_hosts)],
            check=True, capture_output=True, text=True, timeout=15)

        for _ in range(30):
            try:
                self._backup_ssh("echo ready")
                return
            except subprocess.CalledProcessError:
                time.sleep(1)
        raise HarnessError("backup host SSH not ready")

    def cleanup_backup_host(self):
        if self.backup_container:
            subprocess.run(
                ["docker", "rm", "--force", "--volumes", self.backup_container],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.backup_container = None
        for c in list(self._extra_containers):
            subprocess.run(
                ["docker", "rm", "--force", "--volumes", c],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self._extra_containers.clear()

    # ---- SSH keygen helper (stdlib) ---------------------------------------

    def ssh_keygen(self, priv_path):
        subprocess.run(
            ["ssh-keygen", "-t", "ed25519", "-f", str(priv_path),
             "-N", "", "-C", "b28-backup", "-q"],
            check=True, capture_output=True, timeout=15)
        return priv_path

    # ---- scenario ---------------------------------------------------------

    def s1_fresh_install(self):
        """Migrate, bootstrap-admin, start server, create org + private repo."""
        env = self.server_env("b28", self.storage_root)
        subprocess.run(
            [self.binary, "migrate"], env=env, check=True,
            capture_output=True, text=True)
        self.admin_password = secrets.token_urlsafe(32)
        subprocess.run(
            [self.binary, "bootstrap-admin", "--username", "b28-admin"],
            env=env, input=self.admin_password, text=True, capture_output=True,
            check=True)
        self.admin_username = "b28-admin"
        self.start_server("b28", self.storage_root, "initial")
        st, sess = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.admin_username, "password": self.admin_password})
        if st != 200:
            raise HarnessError(f"login status {st}")
        self.admin_token = sess["token"]
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": "b28-org", "name": "B28 Org"}, token=self.admin_token)
        if st != 201:
            raise HarnessError(f"create org status {st}")
        self.org_id = org["id"]
        self.org_slug = org["slug"]
        st, repo = self.api_json(
            "POST", "/api/v1/organizations/b28-org/repositories",
            {"name": "b28-repo", "visibility": "private"}, token=self.admin_token)
        if st != 201:
            raise HarnessError(f"create repo status {st}")
        self.repo_id = repo["id"]
        self.repo_name = repo["name"]

    @property
    def repo_url(self):
        return f"http://127.0.0.1:{self.server.port}/{self.org_slug}/{self.repo_name}.git"

    def s2_push_and_issue(self):
        """Push commits (a few MiB) and create an issue."""
        w = self.main_work
        w.mkdir()
        self.wgit("init", "-b", "main")
        self.wgit("config", "user.name", "B28")
        self.wgit("config", "user.email", "b28@example.com")
        for i in range(10):
            (w / f"file{i:02d}.txt").write_text("x" * 200_000)  # ~200 KiB each
        self.wgit("add", "-A")
        self.wgit("commit", "-m", "initial: ~2 MiB of files")
        self.sha_main = self.wgit("rev-parse", "HEAD").stdout.strip()
        # cwd matters: without it git runs in the harness's directory (this source tree).
        self.git("push", self.repo_url, "main", cwd=str(w),
                 user=self.admin_username, password=self.admin_password,
                 label="seed push")
        # Wait for reconciler to apply
        repo_path = (f"/api/v1/organizations/{self.org_slug}"
                     f"/repositories/{self.repo_name}")
        for _ in range(200):
            st, refs = self.api_json("GET", repo_path + "/refs",
                                     token=self.admin_token)
            if st == 200 and refs.get("branches"):
                break
            time.sleep(0.1)
        st, issue = self.api_json(
            "POST", repo_path + "/issues",
            {"title": "Test issue for off-host restore", "body": "verify me"},
            token=self.admin_token)
        if st != 201:
            raise HarnessError(f"issue create status {st}")
        self.issue_number = issue["number"]

    def s3_offhost_backup(self):
        """Follow the runbook: dump, tar, sums, rsync, verify manifest remote."""
        self._dump_path = self.tmp / "b28.dump"
        with open(self._dump_path, "wb") as d:
            subprocess.run(
                ["docker", "exec", self.name, "pg_dump", "-U", "githearth",
                 "-d", "b28", "-Fc"],
                check=True, stdout=d, timeout=60)
        self._storage_tgz = self.tmp / "storage.tgz"
        subprocess.run(
            ["tar", "-C", str(self.storage_root),
             "--exclude=./run", "-czf", str(self._storage_tgz), "."],
            check=True, timeout=60)
        self._sums_path = self.tmp / "SHA256SUMS"
        subprocess.run(
            ["bash", "-c",
             f"cd {shlex.quote(str(self.tmp))} && "
             f"sha256sum b28.dump storage.tgz > SHA256SUMS"],
            check=True, capture_output=True, text=True, timeout=30)
        self._backup_rsync(
            [self._dump_path, self._storage_tgz, self._sums_path],
            BACKUP_REMOTE_DIR)
        result = self._backup_ssh(
            f"cd {BACKUP_REMOTE_DIR} && sha256sum -c SHA256SUMS")
        if "FAILED" in result.stdout:
            raise HarnessError(f"remote sha256sum failed: {result.stdout}")

    def s4_disaster_and_restore(self):
        """Stop server, drop DB, delete storage, fetch + verify + restore."""
        self.stop_server()
        subprocess.run(
            ["docker", "exec", self.name, "dropdb", "-U", "githearth",
             "--if-exists", "b28"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=30)
        shutil.rmtree(str(self.storage_root))

        restore_dir = self.tmp / "restore-fetched"
        restore_dir.mkdir()
        subprocess.run(
            ["rsync", "-av",
             "-e", f"ssh -i {self.backup_key} -p {self.backup_port} -o StrictHostKeyChecking=yes -o UserKnownHostsFile={self.backup_known_hosts} -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR",
             f"{BACKUP_USER}@127.0.0.1:{BACKUP_REMOTE_DIR}/",
             str(restore_dir) + "/"],
            check=True, capture_output=True, text=True, timeout=120)
        r = subprocess.run(
            ["bash", "-c",
             f"cd {shlex.quote(str(restore_dir))} && sha256sum -c SHA256SUMS"],
            capture_output=True, text=True, timeout=30)
        if r.returncode != 0 or "FAILED" in r.stdout:
            raise HarnessError(f"local manifest verify failed: {r.stdout}")

        new_db = "b28restore"
        subprocess.run(
            ["docker", "exec", self.name, "createdb", "-U", "githearth",
             new_db],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=30)
        with open(restore_dir / "b28.dump", "rb") as d:
            subprocess.run(
                ["docker", "exec", "-i", self.name, "pg_restore",
                 "-U", "githearth", "--no-owner", "-d", new_db],
                check=True, stdin=d, timeout=60)

        new_storage = self.tmp / "storage-restored"
        new_storage.mkdir()
        subprocess.run(
            ["tar", "-C", str(new_storage), "-xzf",
             str(restore_dir / "storage.tgz")],
            check=True, timeout=60)

        cli = str(self.tmp / "githearth")
        subprocess.run(
            ["cargo", "build", "-p", "githearth-cli", "--release"],
            cwd=ROOT, check=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600)
        td = os.environ.get("CARGO_TARGET_DIR")
        base = Path(td) if td else (ROOT / "target")
        built = base / "release" / "githearth"
        shutil.copy(str(built), cli)
        p = subprocess.run([cli, "check", str(new_storage)],
                           capture_output=True, text=True, timeout=120)
        if p.returncode != 0:
            raise HarnessError(f"check exited {p.returncode}: {p.stderr[:300]}")

        self.start_server(new_db, new_storage, "restored")

        st, sess = self.api_json(
            "POST", "/api/v1/login",
            {"username": self.admin_username, "password": self.admin_password})
        if st != 200:
            raise HarnessError(f"restore login status {st}")
        rtok = sess["token"]

        clone_dir = self.tmp / "restore-clone"
        self.git("clone", self.repo_url, str(clone_dir),
                 user=self.admin_username, password=self.admin_password,
                 label="restore clone")
        sha = subprocess.run(
            ["git", "-C", str(clone_dir), "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True).stdout.strip()
        if sha != self.sha_main:
            raise HarnessError(
                f"clone HEAD {sha[:8]} != original {self.sha_main[:8]}")

        repo_path = (f"/api/v1/organizations/{self.org_slug}"
                     f"/repositories/{self.repo_name}")
        st, issue = self.api_json(
            "GET", f"{repo_path}/issues/{self.issue_number}", token=rtok)
        if st != 200 or (issue.get("title") or "") != "Test issue for off-host restore":
            raise HarnessError(f"issue not readable after restore: {st}")

    def s5_tamper_check(self):
        """Flip one byte in the remote tarball and assert sha256sum fails."""
        # Overwrite 8 bytes in place (the backup host has coreutils, not Python).
        self._backup_ssh(
            f"printf TAMPERED | dd of={BACKUP_REMOTE_DIR}/storage.tgz bs=1 seek=64 "
            f"conv=notrunc status=none")
        r = self._backup_ssh(
            f"cd {BACKUP_REMOTE_DIR} && sha256sum -c SHA256SUMS",
            check=False)
        if r.returncode == 0:
            raise HarnessError("sha256sum should fail after tampering")

    # ---- run ---------------------------------------------------------------

    def scenario(self, name, fn):
        start = time.time()
        try:
            fn()
            result = "PASS"
            detail = ""
        except HarnessError as e:
            result = "FAIL"
            detail = str(e)
        except Exception as e:
            result = "FAIL"
            detail = f"{type(e).__name__}: {e}"
        self.scenarios.append({
            "name": name, "result": result,
            "wall_seconds": round(time.time() - start, 3)})
        print(f"{result}: {name}")
        if detail:
            print(f"    {detail}")
        if result == "FAIL":
            self.failures.append(name)

    def run(self):
        signal.signal(signal.SIGINT, lambda *_a: sys.exit(1))
        signal.signal(signal.SIGTERM, lambda *_a: sys.exit(1))
        try:
            self.start_db()
            self.scenario("fresh_install", self.s1_fresh_install)
            self.scenario("push_and_issue", self.s2_push_and_issue)
            self.start_backup_host()
            self.scenario("offhost_backup", self.s3_offhost_backup)
            self.scenario("disaster_and_restore", self.s4_disaster_and_restore)
            self.scenario("tamper_check", self.s5_tamper_check)
        finally:
            self.cleanup_backup_host()
            self.cleanup()
        self.write_report()
        ps = sum(1 for s in self.scenarios if s["result"] == "PASS")
        print(f"\nOff-host backup: {ps}/{len(self.scenarios)} scenarios passed")
        print(f"report: {self.report_path}")
        return 0 if ps == len(self.scenarios) else 1

    def write_report(self):
        info = {}
        info["git_version"] = subprocess.run(
            ["git", "--version"], capture_output=True, text=True).stdout.strip()
        info["source_revision"] = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT,
            capture_output=True, text=True).stdout.strip()
        info["os"] = platform.platform() + " " + platform.machine()
        report = {
            "environment": info, "scenarios": self.scenarios,
            "git_operations": self.git_ops, "server_lifetimes": self.lifetimes,
            "measurements": {}}
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)

    def cleanup(self):
        super().cleanup()

    # Override: M2 uses MAIN_DB = "m2" but we need "b28"
    def start_db(self):
        self.db_port = self.free_port()
        self.env_path.write_text(
            f"POSTGRES_USER=githearth\n"
            f"POSTGRES_DB=b28\n"
            f"POSTGRES_PASSWORD={self.db_password}\n")
        self.env_path.chmod(0o600)
        self.docker(
            ["run", "--detach", "--rm", "--name", self.name,
             "--env-file", str(self.env_path),
             "--publish", f"127.0.0.1:{self.db_port}:5432",
             _m2.IMAGE],
            stdout=subprocess.DEVNULL)
        for _ in range(120):
            if subprocess.run(
                ["docker", "exec", self.name, "pg_isready", "-h", "127.0.0.1",
                 "-U", "githearth", "-d", "b28"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            ).returncode == 0:
                return
            time.sleep(0.5)
        raise HarnessError("PostgreSQL startup timed out")

    def db_url(self, db):
        return f"postgres://githearth:{self.db_password}@127.0.0.1:{self.db_port}/{db}"

    def sql(self, db, query):
        out = subprocess.run(
            ["docker", "exec", "-i", self.name, "psql",
             "-U", "githearth", "-d", db, "-t", "-A",
             "-v", "ON_ERROR_STOP=1"],
            input=query, text=True, capture_output=True, timeout=30)
        if out.returncode != 0:
            raise HarnessError(
                f"SQL failed ({out.returncode}): {out.stderr.strip()[:200]}")
        return out.stdout.strip()


def main():
    p = argparse.ArgumentParser(description="Off-host backup drill (B28)")
    p.add_argument("--report", help="write measurements JSON to this path")
    a = p.parse_args()
    rp = a.report
    if rp is None:
        fd, rp = __import__("tempfile").mkstemp(
            prefix="b28-report-", suffix=".json")
        os.close(fd)
    h = B28Harness(rp)
    sys.exit(h.run())


if __name__ == "__main__":
    import shlex
    main()
