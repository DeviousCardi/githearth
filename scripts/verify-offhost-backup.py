#!/usr/bin/env python3
"""B28 off-host backup drill, re-pointed at `githearth-server backup`/`restore`.

Subclass verify-m2.Harness, start a disposable backup host container, produce an
encrypted backup behind a proved fence, assert its printed and manifest-recorded
fence plus its encryption metadata, copy the single archive off-host, exercise
the four restore refusals, simulate disaster, restore from the off-host copy and
verify the restored installation.

Python 3 stdlib only. Reuses verify-m2.py via importlib (as verify-m4.py does).
"""
import argparse
import hashlib
import importlib.util
import json
import os
import platform
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BACKUP_HOST_IMAGE = "githearth-backup-host:drill"
BACKUP_HOST_DOCKERFILE = """FROM debian:bookworm-slim
RUN apt-get update -qq \\
 && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends openssh-server rsync coreutils \\
 && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd
"""
BACKUP_HOSTNAME = "backup-host"
BACKUP_USER = "ghbackup"  # Debian already has a system user named backup
BACKUP_REMOTE_DIR = "/srv/backups/githearth"
MAIN_DB = "b28"
RESTORE_DB = "b28restore"
ARCHIVE_MAGIC = b"GITHEARTH-BACKUP1\n"


class HarnessError(Exception):
    pass


def _load_m2():
    spec = importlib.util.spec_from_file_location(
        "verify_m2", ROOT / "scripts" / "verify-m2.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_m2 = _load_m2()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


class B28Harness(_m2.Harness):
    def __init__(self, report_path):
        self.name = "githearth-b28-" + secrets.token_hex(6)
        self.backup_container = None
        self.backup_key = None
        self.backup_port = None
        self.backup_known_hosts = None
        self.server_ssh_port = None
        self.server_host_key = None
        self.http_port = None
        self._extra_containers = []
        self._inspector = None
        super().__init__(report_path)
        self._use_short_root()

    # ---- temporary directory ---------------------------------------------

    def _use_short_root(self):
        """Give the drill a short working directory on macOS.

        macOS puts $TMPDIR under /var/folders/.../T/ (70 bytes here), so the
        restored storage root becomes 87 bytes. `restore` rewrites the
        configuration it published and validates it, and the hook socket is
        <storage_root>/run/hook-XXXXXXXX.sock: 110 bytes, over the product's
        107-byte Unix-socket limit, so restore correctly refuses. A short
        $TMPDIR, or /tmp, leaves plenty of room.

        On every other platform the standard m2-* directory under the system
        temp root is kept.
        """
        if platform.system() != "Darwin":
            return
        tag = hashlib.sha256(str(self.tmp).encode()).hexdigest()[:12]
        # Keep the hook socket path inside the 107-byte Unix limit: the
        # socket is <parent>/<work>/storage-restored/run/hook-XXXXXXXX.sock.
        limit = 107 - (1 + 20 + len("/storage-restored") + 23)
        temp = Path(tempfile.gettempdir()).resolve()
        parents = [p for p in (temp, Path("/tmp")) if p.is_dir()]
        parent = next((p for p in parents if len(str(p)) <= limit),
                      min(parents, key=lambda p: len(str(p))))
        holding = parent / f"gh-drill-{tag}"
        shutil.move(str(self.tmp), str(holding))
        self.tmp = holding
        self.folder.cleanup()
        self.folder = tempfile.TemporaryDirectory(prefix="", dir=str(parent))
        # The drill runs directly in the new short directory; copy its contents
        # across and drop the holding directory.
        for child in sorted(holding.iterdir()):
            shutil.move(str(child), str(Path(self.folder.name) / child.name))
        holding.rmdir()
        self._repoint(Path(self.folder.name))

    def _repoint(self, tmp_root):
        """Recompute every derived path for the drill's working directory."""
        self.tmp = tmp_root
        self.env_path = tmp_root / "db.env"
        self.log_path = tmp_root / "app.log"
        if getattr(self, "log_file", None) is not None:
            self.log_file.close()
        self.log_file = open(self.log_path, "wb")
        self.dump_path = tmp_root / "m2.dump"
        self.storage_tar = tmp_root / "storage.tar"
        self.home = tmp_root / "home"
        self.home.mkdir(exist_ok=True)
        self.cred_helper = tmp_root / "git-credential-m2"
        self.cred_helper.write_text(
            "#!/bin/sh\n"
            'printf "username=%s\\npassword=%s\\n\\n" "$GITHEARTH_HTTP_USER" "$GITHEARTH_HTTP_PASS"\n'
        )
        self.cred_helper.chmod(0o755)
        self.storage_root = tmp_root / "storage"
        self.main_work = tmp_root / "work"
        self.clone = tmp_root / "clone"
        self.env_path.write_text(
            "POSTGRES_USER=githearth\n"
            f"POSTGRES_DB={MAIN_DB}\n"
            f"POSTGRES_PASSWORD={self.db_password}\n"
        )
        self.env_path.chmod(0o600)

    # ---- server configuration --------------------------------------------

    def server_env(self, db, storage_root):
        """M2's environment plus the SSH listener the backup fence probes."""
        env = super().server_env(db, storage_root)
        if self.server_host_key is not None:
            env["GITHEARTH_SSH_ENABLED"] = "true"
            env["GITHEARTH_SSH_BIND"] = f"127.0.0.1:{self.server_ssh_port}"
            env["GITHEARTH_SSH_HOST_KEY_PATH"] = str(self.server_host_key)
        return env

    def cli_env(self, db):
        env = self.server_env(db, self.storage_root)
        env["GITHEARTH_BIND"] = f"127.0.0.1:{self.http_port}"
        env["GITHEARTH_SSH_BIND"] = f"127.0.0.1:{self.server_ssh_port}"
        return env

    def run_cli(self, args, env, input=None):
        return subprocess.run(
            [self.binary] + args, env=env, input=input,
            capture_output=True, text=True)

    # ---- manifest inspector (verification helper, not a product command) --

    def inspector(self):
        if self._inspector is None:
            subprocess.run(
                ["cargo", "build", "--release", "-p", "githearth-server",
                 "--example", "backup-inspect"],
                cwd=ROOT, check=True,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600)
            target = os.environ.get("CARGO_TARGET_DIR")
            base = Path(target) if target else (ROOT / "target")
            built = base / "release" / "examples" / "backup-inspect"
            if not built.exists():
                raise HarnessError("backup-inspect helper not found after build")
            self._inspector = str(built)
        return self._inspector

    def inspect_archive(self, archive, key):
        result = subprocess.run(
            [self.inspector(), str(archive), str(key)],
            capture_output=True, text=True, timeout=120)
        if result.returncode != 0:
            raise HarnessError(
                f"cannot inspect {archive.name}: {result.stderr.strip()[:300]}")
        manifest = header = None
        for line in result.stdout.splitlines():
            if line.startswith("MANIFEST "):
                manifest = json.loads(line[len("MANIFEST "):])
            elif line.startswith("HEADER "):
                header = json.loads(line[len("HEADER "):])
        if manifest is None or header is None:
            raise HarnessError("the inspector printed no manifest or header")
        return manifest, header

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
            check=True, capture_output=True, text=True, timeout=300)

    def _backup_scp_from(self, remote_src, dst):
        subprocess.run(
            ["scp", "-P", str(self.backup_port), "-i", str(self.backup_key),
             "-o", "StrictHostKeyChecking=yes",
             "-o", "UserKnownHostsFile=" + str(self.backup_known_hosts),
             "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR",
             f"{BACKUP_USER}@127.0.0.1:{remote_src}", str(dst)],
            check=True, capture_output=True, text=True, timeout=300)

    def _backup_ssh(self, command, check=True):
        return subprocess.run(
            ["ssh"] + self._backup_ssh_opts()
            + ["-p", str(self.backup_port), "-i", str(self.backup_key),
               f"{BACKUP_USER}@127.0.0.1", command],
            check=check, capture_output=True, text=True, timeout=120)

    def _backup_host_network(self):
        """Docker arguments that put the backup host's sshd on 127.0.0.1.

        On Linux, `--network host` shares the host loopback, so the client
        reaches the container over the host's 127.0.0.1 and sshd can stay bound
        to loopback there.

        On macOS (Docker Desktop), the "host" network namespace is the Linux
        VM's, not the host's: ssh-keyscan on the Mac finds nothing and the
        drill stops before it starts `backup`. Publishing the port on the host
        loopback (-p 127.0.0.1:<port>:<port>) forwards inside the VM to the
        container, and sshd must then accept connections on any address in the
        container. That is the only difference; every assertion is the same.
        """
        if platform.system() == "Darwin":
            return (["--publish", f"127.0.0.1:{self.backup_port}:{self.backup_port}"],
                    f"0.0.0.0:{self.backup_port}")
        return ["--network", "host"], f"127.0.0.1:{self.backup_port}"

    def start_backup_host(self):
        bk = self.backup_key = self.tmp / "backup_ed25519"
        self.ssh_keygen(bk)
        pubkey = bk.with_suffix(".pub").read_text().strip()

        self.backup_known_hosts = self.tmp / "backup_known_hosts"

        self.backup_port = self.free_port()
        container_name = "githearth-bh-" + secrets.token_hex(6)
        self._extra_containers.append(container_name)

        # A small image with sshd and coreutils, built once and then cached.
        subprocess.run(
            ["docker", "build", "-q", "-t", BACKUP_HOST_IMAGE, "-"],
            input=BACKUP_HOST_DOCKERFILE, text=True, check=True,
            stdout=subprocess.DEVNULL, timeout=600)
        authorized = self.tmp / "backup_authorized_keys"
        authorized.write_text(pubkey + "\n")
        authorized.chmod(0o644)
        network_args, listen_address = self._backup_host_network()
        print(f"backup host network: {network_args}, sshd ListenAddress={listen_address}")
        setup = (
            f"useradd -m -s /bin/bash {BACKUP_USER} && "
            f"install -d -o {BACKUP_USER} -m 700 /home/{BACKUP_USER}/.ssh {BACKUP_REMOTE_DIR} && "
            f"install -o {BACKUP_USER} -m 600 /keys/authorized_keys /home/{BACKUP_USER}/.ssh/ && "
            "ssh-keygen -A && "
            f"exec /usr/sbin/sshd -D -e -o ListenAddress={listen_address} "
            "-o PasswordAuthentication=no -o PermitRootLogin=no"
        )
        # No --rm: if sshd dies, its logs explain why; cleanup removes the container.
        subprocess.run(
            ["docker", "run", "--detach", *network_args,
             "--name", container_name,
             "-v", f"{authorized}:/keys/authorized_keys:ro",
             BACKUP_HOST_IMAGE, "bash", "-c", setup],
            check=True, stdout=subprocess.DEVNULL)
        self.backup_container = container_name

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
        env = self.server_env(MAIN_DB, self.storage_root)
        subprocess.run(
            [self.binary, "migrate"], env=env, check=True,
            capture_output=True, text=True)
        self.admin_password = secrets.token_urlsafe(32)
        subprocess.run(
            [self.binary, "bootstrap-admin", "--username", "b28-admin"],
            env=env, input=self.admin_password, text=True, capture_output=True,
            check=True)
        self.admin_username = "b28-admin"
        self.server_host_key = self.tmp / "server_host_key"
        self.server_ssh_port = self.free_port()
        self.ssh_keygen(self.server_host_key)
        # The server's own host key, not the drill's upload key.
        self.start_server(MAIN_DB, self.storage_root, "initial")
        self.http_port = self.server.port
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
        return f"http://127.0.0.1:{self.http_port}/{self.org_slug}/{self.repo_name}.git"

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

    # ---- encrypted backup and fence --------------------------------------

    def assert_backup(self, archive, key, kind):
        env = self.cli_env(MAIN_DB)
        result = self.run_cli(
            ["backup", "--output", str(archive), "--key-file", str(key)], env)
        if result.returncode != 0:
            raise HarnessError(
                f"backup ({kind}) failed: {(result.stdout + result.stderr).strip()[:600]}")
        out = result.stdout + result.stderr
        for needle in (f"fence: {kind}", f"fence used: {kind}"):
            if needle not in out:
                raise HarnessError(f"backup did not print {needle!r}: {out[:600]}")
        return self.assert_archive(archive, key, kind, out)

    def assert_archive(self, archive, key, kind, output):
        raw = archive.read_bytes()
        if not raw.startswith(ARCHIVE_MAGIC):
            raise HarnessError("the archive is not a GITHEARTH-BACKUP1 container")
        try:
            header = json.loads(raw.split(b"\n", 2)[1])
        except (IndexError, json.JSONDecodeError) as error:
            raise HarnessError(f"the archive header is not JSON: {error}")
        if header.get("cipher") != "AES-256-GCM":
            raise HarnessError(f"archive cipher is {header.get('cipher')!r}")
        if not str(header.get("kdf", "")).startswith("Argon2id"):
            raise HarnessError(f"archive KDF is {header.get('kdf')!r}")
        if header.get("kdf_memory_kib") != 65536 or header.get("kdf_iterations") != 3:
            raise HarnessError("archive KDF parameters are not 64 MiB / 3 iterations")
        for field in ("salt", "nonce", "key_id"):
            if not header.get(field):
                raise HarnessError(f"archive header has no {field}")
        secret = key.read_bytes().strip()
        for forbidden in (
                secret, self.admin_password.encode(), self.db_password.encode(),
                b"PGDMP", b"manifest.json", b"config.toml", b"database.dump",
                b"storage.tar", b"ssh_host_key"):
            if forbidden and forbidden in raw:
                raise HarnessError(
                    f"the encrypted archive leaks {forbidden[:24]!r}")
        if f"key {header['key_id']}" not in output:
            raise HarnessError("backup did not print the archive key id")

        manifest, inspected_header = self.inspect_archive(archive, key)
        if inspected_header.get("key_id") != header.get("key_id"):
            raise HarnessError("the inspected header and raw header disagree")
        if manifest["fence"]["kind"] != kind:
            raise HarnessError(
                f"manifest fence is {manifest['fence']['kind']!r}, expected {kind!r}")
        if not manifest["fence"].get("detail"):
            raise HarnessError("manifest has no fence detail")
        encryption = manifest["encryption"]
        if encryption.get("key_id") != header["key_id"]:
            raise HarnessError("manifest key id does not match the archive header")
        if encryption.get("algorithm") != "AES-256-GCM":
            raise HarnessError(f"manifest algorithm is {encryption.get('algorithm')!r}")
        if not str(encryption.get("kdf", "")).startswith("Argon2id"):
            raise HarnessError(f"manifest KDF is {encryption.get('kdf')!r}")
        if encryption.get("wrap") != "argon2id-symmetric":
            raise HarnessError(f"manifest wrap is {encryption.get('wrap')!r}")
        if encryption.get("kdf_memory_kib") != 65536 or encryption.get("kdf_iterations") != 3:
            raise HarnessError("manifest KDF parameters are not 64 MiB / 3 iterations")
        for field in ("salt", "nonce"):
            if not encryption.get(field):
                raise HarnessError(f"manifest encryption has no {field}")
        if "key" in encryption:
            raise HarnessError("manifest encryption contains a key field")
        text = json.dumps(manifest)
        if secret.decode(errors="replace") in text:
            raise HarnessError("the manifest contains the key")
        names = {part["name"] for part in manifest["parts"]}
        for required in ("database.dump", "storage.tar", "config.toml", "ssh_host_key"):
            if required not in names:
                raise HarnessError(f"manifest does not cover {required}")
        if manifest.get("ssh_host_key") is None:
            raise HarnessError("manifest has no SSH host key part")
        excluded = " ".join(manifest.get("excluded", []))
        for part in ("run/", "tmp/", "hooks/"):
            if part not in excluded:
                raise HarnessError(f"manifest does not name the excluded {part}")
        return manifest, header

    def s3_backup_fences_and_offhost_copy(self):
        """Refuse while serve accepts writes; back up under both fences; copy."""
        key = self.tmp / "backup.key"
        key.write_text(secrets.token_hex(32))
        key.chmod(0o600)
        self.archive_key = key

        # 1. The server is up and accepting writes: `backup` must refuse and
        #    name the fence it could not prove.
        refused = self.run_cli(
            ["backup", "--output", str(self.tmp / "refused.ghbackup"),
             "--key-file", str(key)], self.cli_env(MAIN_DB))
        if refused.returncode == 0:
            raise HarnessError("backup succeeded while serve accepted writes")
        combined = refused.stdout + refused.stderr
        if "accepting writes" not in combined:
            raise HarnessError(f"refusal does not name the live writer: {combined[:600]}")

        # 2. Maintenance mode: the fence kind is printed and recorded.
        started = self.run_cli(["maintenance", "start"], self.cli_env(MAIN_DB))
        if started.returncode != 0:
            raise HarnessError(
                f"maintenance start failed: {(started.stdout + started.stderr)[:600]}")
        if ("backup may proceed" not in started.stdout
                and "drained" not in started.stdout):
            raise HarnessError(f"maintenance did not report drained: {started.stdout[:400]}")
        self.archive = self.tmp / "b28-maintenance.ghbackup"
        manifest, _ = self.assert_backup(self.archive, key, "maintenance")
        if manifest["postgres"]["dump_lsn"] == "":
            raise HarnessError("manifest has no advisory PostgreSQL position")
        self.run_cli(["maintenance", "stop"], self.cli_env(MAIN_DB))

        # 3. Server stopped: the other fence, printed and recorded.
        self.stop_server()
        stopped_archive = self.tmp / "b28-not-running.ghbackup"
        self.assert_backup(stopped_archive, key, "not-running")

        # 4. The off-host copy is one encrypted file, not dump + tar + SHA256SUMS.
        remote = f"{BACKUP_REMOTE_DIR}/{self.archive.name}"
        self._backup_scp(self.archive, remote)
        local_hash = sha256_file(self.archive)
        remote_hash = self._backup_ssh(f"sha256sum {remote}").stdout.split()[0]
        if local_hash != remote_hash:
            raise HarnessError(
                f"off-host copy differs: local {local_hash}, remote {remote_hash}")
        self.remote_archive = remote
        self.archive_hash = local_hash

    # ---- the four restore refusals ---------------------------------------

    def db_exists(self, name):
        out = subprocess.run(
            ["docker", "exec", self.name, "psql", "-U", "githearth",
             "-d", "postgres", "-tA", "-c",
             f"SELECT 1 FROM pg_database WHERE datname = '{name}'"],
            capture_output=True, text=True, timeout=30)
        return out.stdout.strip() == "1"

    def restore_attempt(self, label, archive, key_args, expected):
        database = f"b28{label}"
        storage = self.tmp / f"storage-{label}"
        config = self.tmp / f"config-{label}.toml"
        host_key = self.tmp / f"hostkey-{label}"
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("GITHEARTH_")}
        env["GITHEARTH_DATABASE_URL"] = self.db_url(database)
        args = [self.binary, "--config", str(config), "restore",
                "--archive", str(archive), "--storage-root", str(storage),
                "--ssh-host-key-path", str(host_key)] + key_args
        result = subprocess.run(
            args, env=env, capture_output=True, text=True, timeout=180)
        if result.returncode == 0:
            raise HarnessError(f"{label}: restore unexpectedly succeeded")
        combined = result.stdout + result.stderr
        if expected not in combined:
            raise HarnessError(
                f"{label}: refusal does not say {expected!r}: {combined[:600]}")
        if self.db_exists(database):
            raise HarnessError(f"{label}: the destination database was created")
        for path in (storage, config, host_key):
            if path.exists():
                raise HarnessError(f"{label}: the destination {path.name} was created")
        return combined.strip().splitlines()[0]

    def s4_restore_refusals(self):
        """Wrong key, missing key, corrupted archive, incomplete archive."""
        wrong = self.tmp / "wrong.key"
        wrong.write_text(secrets.token_hex(32))
        wrong.chmod(0o600)

        corrupt = self.tmp / "corrupt.ghbackup"
        data = bytearray(self.archive.read_bytes())
        data[len(data) // 2] ^= 0x01
        corrupt.write_bytes(data)

        incomplete = self.tmp / "incomplete.ghbackup"
        raw = self.archive.read_bytes()
        incomplete.write_bytes(raw[:len(raw) // 2])

        messages = [
            self.restore_attempt(
                "wrong", self.archive, ["--key-file", str(wrong)],
                "wrong backup key for this archive"),
            self.restore_attempt(
                "missing", self.archive, [],
                "no backup key supplied"),
            self.restore_attempt(
                "corrupt", corrupt, ["--key-file", str(self.archive_key)],
                "backup archive is corrupt"),
            self.restore_attempt(
                "incomplete", incomplete,
                ["--key-file", str(self.archive_key)],
                "backup archive is incomplete"),
        ]
        if len(set(messages)) != 4:
            raise HarnessError(f"the four refusals are not distinct: {messages}")

    # ---- disaster and restore from the off-host copy ---------------------

    def s5_disaster_and_restore(self):
        """Stop server, drop DB, delete storage, fetch + verify + restore."""
        self.stop_server()
        subprocess.run(
            ["docker", "exec", self.name, "dropdb", "-U", "githearth",
             "--if-exists", "--force", MAIN_DB],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=60)
        shutil.rmtree(str(self.storage_root))

        restore_dir = self.tmp / "restore-fetched"
        restore_dir.mkdir()
        fetched = restore_dir / self.archive.name
        self._backup_scp_from(self.remote_archive, fetched)
        if sha256_file(fetched) != self.archive_hash:
            raise HarnessError("the fetched archive differs from the off-host copy")

        restored_config = self.tmp / "restored-config.toml"
        restored_storage = self.tmp / "storage-restored"
        restored_host_key = self.tmp / "restored-ssh-host-key"
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("GITHEARTH_")}
        env["GITHEARTH_DATABASE_URL"] = self.db_url(RESTORE_DB)
        result = subprocess.run(
            [self.binary, "--config", str(restored_config), "restore",
             "--archive", str(fetched), "--key-file", str(self.archive_key),
             "--storage-root", str(restored_storage),
             "--ssh-host-key-path", str(restored_host_key)],
            env=env, capture_output=True, text=True, timeout=300)
        if result.returncode != 0:
            raise HarnessError(
                f"restore failed: {(result.stdout + result.stderr).strip()[:800]}")
        if "restore complete" not in result.stdout:
            raise HarnessError(f"restore printed no completion: {result.stdout[:400]}")
        if "database-to-storage consistency verified" not in result.stdout:
            raise HarnessError("restore did not report the consistency check")
        for path in (restored_config, restored_storage, restored_host_key):
            if not path.exists():
                raise HarnessError(f"restore did not publish {path.name}")
        if not self.db_exists(RESTORE_DB):
            raise HarnessError("restore did not create the destination database")

        # Start the restored installation with the restored host key.
        self.server_host_key = restored_host_key
        self.start_server(RESTORE_DB, restored_storage, "restored")
        self.http_port = self.server.port

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

    # ---- run ---------------------------------------------------------------

    def disk_free_gib(self):
        """Free space in the Docker VM, where images, volumes and builds live."""
        try:
            out = subprocess.run(
                ["docker", "run", "--rm", _m2.IMAGE, "df", "-BG", "--output=avail", "/"],
                capture_output=True, text=True, timeout=180)
            return int(out.stdout.strip().splitlines()[-1].rstrip("G").strip())
        except (subprocess.SubprocessError, ValueError, IndexError):
            return None

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
        free = self.disk_free_gib()
        self.scenarios[-1]["docker_vm_free_gib"] = free
        print(f"    docker VM free disk: {free} GiB")
        if free is not None and free < 15:
            raise HarnessError(
                "free disk in the Docker VM fell below the 15 GiB floor")
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
            self.scenario("backup_fences_and_offhost_copy",
                          self.s3_backup_fences_and_offhost_copy)
            self.scenario("restore_refusals", self.s4_restore_refusals)
            self.scenario("disaster_and_restore", self.s5_disaster_and_restore)
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
        info["backup_host_network"] = self._backup_host_network()
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
            f"POSTGRES_DB={MAIN_DB}\n"
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
                 "-U", "githearth", "-d", MAIN_DB],
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
    main()
