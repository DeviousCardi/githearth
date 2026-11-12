#!/usr/bin/env python3
"""Hardware qualification harness: measure RSS, CPU and latency on one machine
for (a) a large synthetic repository (clone/push/browse), (b) a concurrency
sweep (1, 8, 32, 64 clients) and (c) a soak run checking for memory growth and
errors.

Same conventions as the other verify-*.py harnesses: it builds the release
binary, runs an ephemeral PostgreSQL container and the server on ephemeral
ports, never prints generated secrets, and removes every container, process and
temporary directory on exit (including on SIGINT/SIGTERM).

It subclasses the M4 harness for its server/API/Git plumbing and reuses its
process-sampling helpers. Python 3 stdlib only.
"""
import argparse
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
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "postgres:17-bookworm"
HW_DB = "hw"

# Default scale for the "larger repository" drill: 50k commits plus enough
# incompressible data to bring the object store to roughly 1 GiB.
LARGE_COMMITS = 50_000
LARGE_BIG_FILE_COUNT = 9
LARGE_BIG_FILE_BYTES = 100 * 1024 * 1024  # 100 MiB each
CONCURRENCY_LEVELS = (1, 8, 32, 64)
DEFAULT_SOAK_MINUTES = 20.0

# Memory-growth ceiling for the soak: more than this much RSS gain between the
# start and end of a soak run is treated as a leak, not allocator noise.
SOAK_GROWTH_CEILING_KIB = 32 * 1024


# ---- Load verify-m4 and reuse its Harness + helpers ------------------------

def _load_m4():
    spec = importlib.util.spec_from_file_location(
        "verify_m4", ROOT / "scripts" / "verify-m4.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_m4 = _load_m4()
HarnessError = _m4.HarnessError
Skipped = _m4.Skipped
M4Harness = _m4.M4Harness
_proc_rss_kib = _m4._proc_rss_kib
_all_children = _m4._all_children
_count_git_children = _m4._count_git_children


def _clk_tck():
    try:
        return os.sysconf("SC_CLK_TCK") or 100
    except (AttributeError, ValueError, OSError):
        return 100


def _proc_jiffies(pid):
    """utime + stime in clock ticks, from /proc/<pid>/stat."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            data = f.read()
        rest = data[data.rfind(")") + 2:].split()
        return int(rest[11]) + int(rest[12])
    except (OSError, ValueError, IndexError):
        return 0


def _tree_jiffies(pid):
    total = _proc_jiffies(pid)
    for child in _all_children(pid):
        total += _proc_jiffies(child)
    return total


def _tree_rss_kib(pid):
    total = _proc_rss_kib(pid)
    for child in _all_children(pid):
        total += _proc_rss_kib(child)
    return total


def _percentile(sorted_values, p):
    if not sorted_values:
        return None
    k = (len(sorted_values) - 1) * (p / 100.0)
    lo = int(k)
    hi = min(lo + 1, len(sorted_values) - 1)
    frac = k - lo
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * frac


def _host_memory_kib():
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


class HardwareHarness(M4Harness):
    """Reuses M4's server/Git/API plumbing; runs only the hardware scenarios."""

    def __init__(self, report_path, soak_minutes, commits, big_files, big_file_bytes, workdir):
        self._hw_admin_username = "hw-admin"
        self._hw_admin_password = secrets.token_urlsafe(32)
        self.soak_minutes = soak_minutes
        self.large_commits = commits
        self.big_file_count = big_files
        self.big_file_bytes = big_file_bytes
        super().__init__(report_path)
        # Set after super().__init__: M4Harness then _m2.Harness overwrite the
        # container name, so assigning here keeps the intended "hw" prefix.
        self.name = "githearth-hw-" + uuid.uuid4().hex[:12]
        self.admin_username = self._hw_admin_username
        self.admin_password = self._hw_admin_password
        self.org_slug = "hw-org"
        self.org_id = None
        # Keep every synthetic repository, clone and the server storage root under
        # the worktree so the drill never writes to /tmp or an absolute path outside
        # this directory (TASK rule). The parent M2/M4 harness created its temporary
        # tree with tempfile.TemporaryDirectory() in /tmp; rebuild it under <workdir>.
        self._relocate_temp(Path(workdir))

    def _relocate_temp(self, workdir):
        """Move the harness temporary tree under `workdir` (inside the worktree).

        Mirrors the paths the parent harness creates, then drops the /tmp tree.
        """
        env_text = self.env_path.read_text() if self.env_path.exists() else ""
        old_folder = self.folder
        try:
            self.log_file.close()
        except Exception:
            pass
        workdir.mkdir(parents=True, exist_ok=True)
        # Keep the temp name short: the server's hook admission socket is
        # <storage_root>/run/hook-XXXXXXXX.sock (107-byte Unix limit), so an
        # absolute storage_root longer than 84 bytes fails config validation.
        self.folder = tempfile.TemporaryDirectory(prefix="hw-", dir=str(workdir))
        self.tmp = Path(self.folder.name)
        self.env_path = self.tmp / "db.env"
        if env_text:
            self.env_path.write_text(env_text)
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
        self.storage_root = self.tmp / "storage"
        self.main_work = self.tmp / "work"
        self.clone = self.tmp / "clone"
        self.ssh_host_key_path = self.tmp / "ssh_host_key"
        old_folder.cleanup()

    # ---- setup: migrate, bootstrap, create org -------------------------

    def setup(self):
        self.start_db()
        self.docker(["exec", self.name, "createdb", "-U", "githearth", HW_DB],
                    stdout=subprocess.DEVNULL)
        env = self.server_env(HW_DB, self.storage_root, ssh_enabled=False)
        subprocess.run([self.binary, "migrate"], env=env, check=True,
                       capture_output=True, text=True)
        b = subprocess.run(
            [self.binary, "bootstrap-admin", "--username", self.admin_username],
            env=env, input=self.admin_password, text=True, capture_output=True)
        if b.returncode != 0:
            raise HarnessError("bootstrap-admin failed: " + b.stderr.strip()[:300])
        self.start_server(HW_DB, self.storage_root, "hw", ssh_enabled=False)
        tok = self.admin_token = self.api_session(self.admin_username, self.admin_password)
        st, org = self.api_json(
            "POST", "/api/v1/organizations",
            {"slug": self.org_slug, "name": "HW Org"}, token=tok)
        if st != 201:
            raise HarnessError(f"create org status {st}")
        self.org_id = org["id"]

    def _create_repo(self, name):
        st, repo = self.api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories",
            {"name": name, "visibility": "private"}, token=self.admin_token)
        if st != 201:
            raise HarnessError(f"create repo {name} status {st}")
        return repo

    def _repo_url(self, name):
        return f"http://127.0.0.1:{self.server.http_port}/{self.org_slug}/{name}.git"

    # ---- sampled single process: run argv, sample RSS/CPU until done -----

    def _run_sampled(self, argv, cwd, env, timeout=3600, label=""):
        """Run a client process, sampling the server tree's RSS/CPU throughout.

        Returns (returncode, stderr, wall_seconds, peak_tree_rss_kib,
        peak_server_rss_kib, cpu_seconds)."""
        pid = self.server.proc.pid
        start_jiffies = _tree_jiffies(pid)
        start = time.time()
        deadline = start + timeout
        proc = subprocess.Popen(argv, cwd=cwd, env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        peak_tree = peak_server = 0
        while proc.poll() is None:
            peak_tree = max(peak_tree, _tree_rss_kib(pid))
            peak_server = max(peak_server, _proc_rss_kib(pid))
            if time.time() > deadline:
                proc.kill()
                proc.wait()
                raise HarnessError(f"{label or argv[0]} timed out after {timeout}s")
            time.sleep(0.05)
        proc.wait()
        wall = time.time() - start
        end_jiffies = _tree_jiffies(pid)
        cpu = (end_jiffies - start_jiffies) / _clk_tck()
        return proc.returncode, proc.stderr.read(), wall, peak_tree, peak_server, cpu

    # ---- large repository generation ------------------------------------

    def _generate_large_repo(self, dir_path, commits, big_file_count, big_file_bytes):
        env = self.git_env()
        subprocess.run(["git", "init", "-b", "main"], cwd=str(dir_path), env=env,
                       check=True, capture_output=True)
        proc = subprocess.Popen(["git", "fast-import", "--quiet"], cwd=str(dir_path),
                                env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        w = proc.stdin
        base_ts = 1_700_000_000
        for i in range(commits):
            ts = base_ts + i
            msg = f"commit {i}\n".encode()
            blob = (f"{i:08d} ".encode() * 128)[:1024]
            w.write(b"commit refs/heads/main\n")
            w.write(f"author Hw <hw@example.com> {ts} +0000\n".encode())
            w.write(f"committer Hw <hw@example.com> {ts} +0000\n".encode())
            w.write(f"data {len(msg)}\n".encode())
            w.write(msg)
            w.write(f"M 100644 inline data/f{i:05d}.txt\n".encode())
            w.write(f"data {len(blob)}\n".encode())
            w.write(blob)
            w.write(b"\n")
        w.close()
        proc.wait(timeout=600)
        err = proc.stderr.read()
        if proc.returncode != 0:
            raise HarnessError(f"fast-import failed: {err.decode(errors='replace')[:400]}")
        # Append big incompressible files with normal git (avoids a 1 GiB stream).
        for j in range(big_file_count):
            big = dir_path / f"big{j}.bin"
            subprocess.run(
                ["dd", "if=/dev/urandom", f"of={big}", "bs=1M",
                 f"count={big_file_bytes // (1024 * 1024)}"],
                capture_output=True, check=True, timeout=600)
            for ea in (["add", f"big{j}.bin"], ["-c", "user.name=Hw", "-c", "user.email=hw@example.com",
                                                 "commit", "-m", f"big file {j}"]):
                subprocess.run(["git", "-C", str(dir_path)] + ea, env=env,
                               capture_output=True, check=True)
        out = subprocess.run(["git", "-C", str(dir_path), "rev-parse", "HEAD"],
                             env=env, capture_output=True, text=True, check=True)
        size = self.dir_size(dir_path / ".git")
        return out.stdout.strip(), size

    # ---- scenario (a): large repository ----------------------------------

    def s1_large_repository(self):
        self.stop_server()
        # Raise the push ceiling so a ~1 GiB pack is accepted.
        self.start_server(HW_DB, self.storage_root, "hw-large", ssh_enabled=False,
                          extra={"GITHEARTH_GIT_PUSH_LIMIT_BYTES": str(2 * 1024 * 1024 * 1024)})
        tok = self.admin_token = self.api_session(self.admin_username, self.admin_password)
        self._create_repo("large")

        src = self.tmp / "large-src"
        src.mkdir()
        t0 = time.time()
        head_sha, git_size = self._generate_large_repo(
            src, self.large_commits, self.big_file_count, self.big_file_bytes)
        gen_seconds = time.time() - t0

        url = self._repo_url("large")
        penv = self.git_env(self.admin_username, self.admin_password)

        t0 = time.time()
        rc, err, push_s, push_tree, push_server, push_cpu = self._run_sampled(
            ["git", "-c", f"credential.helper={self.cred_helper}", "push", url, "main"],
            cwd=str(src), env=penv, timeout=3600, label="large push")
        push_wall = time.time() - t0
        if rc != 0:
            raise HarnessError(f"large push failed: {err[:400]}")

        clone_dir = self.tmp / "large-clone"
        rc, err, clone_s, clone_tree, clone_server, clone_cpu = self._run_sampled(
            ["git", "-c", f"credential.helper={self.cred_helper}", "clone", url, str(clone_dir)],
            cwd=str(src), env=penv, timeout=3600, label="large clone")
        if rc != 0:
            raise HarnessError(f"large clone failed: {err[:400]}")

        # Browse latency (single client, repeated, p50/p95/p99).
        browse = self._measure_browse("large")
        shutil.rmtree(str(src), ignore_errors=True)
        shutil.rmtree(str(clone_dir), ignore_errors=True)

        self._large = {
            "commits": self.large_commits,
            "big_files": self.big_file_count,
            "big_file_bytes": self.big_file_bytes,
            "generated_git_size_bytes": git_size,
            "generate_seconds": round(gen_seconds, 3),
            "push_wall_seconds": round(push_wall, 3),
            "push_client_seconds": round(push_s, 3),
            "push_peak_tree_rss_kib": push_tree,
            "push_peak_server_rss_kib": push_server,
            "push_cpu_seconds": round(push_cpu, 3),
            "clone_wall_seconds": round(clone_s, 3),
            "clone_peak_tree_rss_kib": clone_tree,
            "clone_peak_server_rss_kib": clone_server,
            "clone_cpu_seconds": round(clone_cpu, 3),
            "head_sha": head_sha,
            "browse": browse,
        }
        print(f"    large repo: {self.large_commits} commits, "
              f"{git_size / (1024 ** 3):.2f} GiB objects")
        print(f"    generate {gen_seconds:.1f}s, push {push_wall:.1f}s "
              f"(peak tree {push_tree} KiB), clone {clone_s:.1f}s "
              f"(peak tree {clone_tree} KiB)")

    def _measure_browse(self, repo):
        base = f"/api/v1/organizations/{self.org_slug}/repositories/{repo}"
        refs = self.api_json("GET", base + "/refs", token=self.admin_token)
        if refs[0] != 200 or not refs[1].get("branches"):
            raise HarnessError(f"browse refs status {refs[0]}")
        branches = {b["name"]: b["target"] for b in refs[1]["branches"]}
        head = branches.get("main") or next(iter(branches.values()))
        endpoints = {
            "refs": ("GET", base + "/refs"),
            "tree": ("GET", base + "/tree/main"),
            "commits": ("GET", base + "/commits/main?limit=50"),
            "commit": ("GET", base + f"/commit/{head}"),
        }
        result = {}
        for name, (method, path) in endpoints.items():
            latencies = []
            for _ in range(20):
                t0 = time.time()
                st, _ = self.api_json(method, path, token=self.admin_token)
                latencies.append(time.time() - t0)
                if st != 200:
                    raise HarnessError(f"browse {name} status {st}")
            latencies.sort()
            result[name] = {
                "p50_ms": round(_percentile(latencies, 50) * 1000, 3),
                "p95_ms": round(_percentile(latencies, 95) * 1000, 3),
                "p99_ms": round(_percentile(latencies, 99) * 1000, 3),
            }
        return result

    # ---- scenario (b): concurrency sweep --------------------------------

    def s2_concurrency_sweep(self):
        self.stop_server()
        # A modest repo so a single clone is fast but still measures git work.
        self.start_server(HW_DB, self.storage_root, "hw-sweep", ssh_enabled=False)
        tok = self.admin_token = self.api_session(self.admin_username, self.admin_password)
        self._create_repo("sweep")

        w = self.tmp / "sweep-src"
        w.mkdir()
        self.git("init", "-b", "main", cwd=str(w))
        (w / "README.md").write_text("# sweep\n")
        self.git("add", "-A", cwd=str(w))
        self.git("-c", "user.name=Hw", "-c", "user.email=hw@example.com",
                 "commit", "-m", "seed", cwd=str(w))
        data = os.urandom(8 * 1024 * 1024)
        (w / "blob.bin").write_bytes(data)
        self.git("add", "blob.bin", cwd=str(w))
        self.git("-c", "user.name=Hw", "-c", "user.email=hw@example.com",
                 "commit", "-m", "8 MiB blob", cwd=str(w))
        url = self._repo_url("sweep")
        self.git("push", url, "main", user=self.admin_username,
                 password=self.admin_password, cwd=str(w))

        levels = []
        penv = self.git_env(self.admin_username, self.admin_password)
        for n in CONCURRENCY_LEVELS:
            levels.append(self._sweep_level(n, url, penv))
        shutil.rmtree(str(w), ignore_errors=True)
        self._sweep = {"repo_bytes": len(data) + 1024, "levels": levels}
        for lv in levels:
            print(f"    {lv['clients']} clients: {lv['successes']} ok + "
                  f"{lv['busy']} busy + {lv['errors']} err in {lv['wall_seconds']:.2f}s, "
                  f"p50 {lv['p50_ms']} ms, p95 {lv['p95_ms']} ms, "
                  f"peak tree {lv['peak_tree_rss_kib']} KiB")

    def _sweep_level(self, n, url, penv):
        pid = self.server.proc.pid
        dirs = [self.tmp / f"sw-{n}-{i}" for i in range(n)]
        procs = []
        starts = []
        for i in range(n):
            proc = subprocess.Popen(
                ["git", "-c", f"credential.helper={self.cred_helper}",
                 "clone", url, str(dirs[i])],
                env=penv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            procs.append(proc)
            starts.append(time.time())
        peak_tree = 0
        peak_server = 0
        max_children = 0
        start = time.time()
        while any(p.poll() is None for p in procs):
            peak_tree = max(peak_tree, _tree_rss_kib(pid))
            peak_server = max(peak_server, _proc_rss_kib(pid))
            max_children = max(max_children, _count_git_children(pid))
            time.sleep(0.02)
        wall = time.time() - start

        latencies = []
        successes = busy = errors = 0
        for proc, cd, st in zip(procs, dirs, starts):
            proc.wait(timeout=300)
            stderr = proc.stderr.read()
            shutil.rmtree(str(cd), ignore_errors=True)
            if proc.returncode == 0:
                successes += 1
                latencies.append(time.time() - st)
            elif "busy" in stderr.lower() or "unavailable" in stderr.lower() or "503" in stderr:
                busy += 1
            else:
                errors += 1
        latencies.sort()
        cap = int(os.environ.get("GITHEARTH_GIT_CONCURRENCY", "8"))
        if max_children > cap:
            raise HarnessError(
                f"sweep {n} clients: {max_children} concurrent git children "
                f"> git_concurrency cap {cap}")
        if errors:
            raise HarnessError(
                f"sweep {n} clients: {errors} clone errors (not busy/refused)")
        st, _ = self.api_json("GET", "/health/ready")
        if st != 200:
            raise HarnessError(f"sweep: server not ready after {n} clients")
        return {
            "clients": n,
            "wall_seconds": round(wall, 3),
            "successes": successes,
            "busy": busy,
            "errors": errors,
            "p50_ms": round(_percentile(latencies, 50) * 1000, 3) if latencies else None,
            "p95_ms": round(_percentile(latencies, 95) * 1000, 3) if latencies else None,
            "p99_ms": round(_percentile(latencies, 99) * 1000, 3) if latencies else None,
            "max_concurrent_git_children": max_children,
            "peak_tree_rss_kib": peak_tree,
            "peak_server_rss_kib": peak_server,
        }

    # ---- scenario (c): soak --------------------------------------------

    def s3_soak(self):
        self.stop_server()
        self.start_server(HW_DB, self.storage_root, "hw-soak", ssh_enabled=False)
        tok = self.admin_token = self.api_session(self.admin_username, self.admin_password)
        self._create_repo("soak")

        w = self.tmp / "soak-src"
        w.mkdir()
        self.git("init", "-b", "main", cwd=str(w))
        (w / "f.txt").write_text("soak\n")
        self.git("add", "-A", cwd=str(w))
        self.git("-c", "user.name=Hw", "-c", "user.email=hw@example.com",
                 "commit", "-m", "seed", cwd=str(w))
        url = self._repo_url("soak")
        self.git("push", url, "main", user=self.admin_username,
                 password=self.admin_password, cwd=str(w))

        pid = self.server.proc.pid
        penv = self.git_env(self.admin_username, self.admin_password)
        deadline = time.time() + self.soak_minutes * 60
        samples = []
        errors = []
        iteration = 0
        while time.time() < deadline:
            iteration += 1
            try:
                (w / f"it-{iteration}.txt").write_text(f"{iteration}\n")
                self.git("add", "-A", cwd=str(w))
                self.git("-c", "user.name=Hw", "-c", "user.email=hw@example.com",
                         "commit", "-m", f"iter {iteration}", cwd=str(w))
                rc, err, _, _, _, _ = self._run_sampled(
                    ["git", "-c", f"credential.helper={self.cred_helper}",
                     "push", url, "main"], cwd=str(w), env=penv, timeout=120, label="soak push")
                if rc != 0:
                    errors.append(f"push iter {iteration}: {err[:200]}")
                cd = self.tmp / f"soak-clone-{iteration}"
                rc, err, _, _, _, _ = self._run_sampled(
                    ["git", "-c", f"credential.helper={self.cred_helper}",
                     "clone", url, str(cd)], cwd=str(w), env=penv, timeout=120, label="soak clone")
                shutil.rmtree(str(cd), ignore_errors=True)
                if rc != 0:
                    errors.append(f"clone iter {iteration}: {err[:200]}")
                st, _ = self.api_json("GET", f"/api/v1/organizations/{self.org_slug}/repositories/soak/refs",
                                      token=self.admin_token)
                if st != 200:
                    errors.append(f"browse iter {iteration}: status {st}")
            except HarnessError as e:
                errors.append(f"iter {iteration}: {e}")
            samples.append(_proc_rss_kib(pid))
            time.sleep(0.5)

        # Settle, then look for leaked git children.
        time.sleep(3)
        leaked_children = _count_git_children(pid)
        rss_vals = [s for s in samples if s]
        rss_start = sorted(rss_vals[:3])[len(rss_vals[:3]) // 2] if len(rss_vals) >= 3 else (rss_vals[0] if rss_vals else 0)
        rss_end = sorted(rss_vals[-3:])[len(rss_vals[-3:]) // 2] if len(rss_vals) >= 3 else (rss_vals[-1] if rss_vals else 0)
        rss_peak = max(rss_vals) if rss_vals else 0
        growth = rss_end - rss_start

        self._soak = {
            "minutes": self.soak_minutes,
            "iterations": iteration,
            "samples": len(rss_vals),
            "rss_start_kib": rss_start,
            "rss_end_kib": rss_end,
            "rss_peak_kib": rss_peak,
            "rss_growth_kib": growth,
            "errors": errors,
            "leaked_git_children": leaked_children,
        }
        print(f"    soak {self.soak_minutes} min, {iteration} iterations, "
              f"rss {rss_start} -> {rss_end} KiB (growth {growth} KiB), "
              f"{len(errors)} errors, {leaked_children} leaked git children")
        if errors:
            raise HarnessError(f"soak: {len(errors)} errors, first: {errors[0]}")
        if growth > SOAK_GROWTH_CEILING_KIB:
            raise HarnessError(f"soak: RSS grew by {growth} KiB > {SOAK_GROWTH_CEILING_KIB} KiB")
        if leaked_children != 0:
            raise HarnessError(f"soak: {leaked_children} git children leaked after settle")

    # ---- runner ---------------------------------------------------------

    def run(self):
        signal.signal(signal.SIGINT, lambda *_a: sys.exit(1))
        signal.signal(signal.SIGTERM, lambda *_a: sys.exit(1))
        try:
            self.setup()
            self.scenario("large_repository", self.s1_large_repository)
            self.scenario("concurrency_sweep", self.s2_concurrency_sweep)
            self.scenario("soak", self.s3_soak)
        finally:
            self.cleanup()
        self.write_report()
        ps = sum(1 for s in self.scenarios if s["result"] == "PASS")
        print(f"\nHardware verification: {ps}/{len(self.scenarios)} scenarios passed")
        print(f"report: {self.report_path}")
        return 0 if all(s["result"] in ("PASS", "SKIP") for s in self.scenarios) else 1

    def write_report(self):
        env = {
            "git_version": subprocess.run(
                ["git", "--version"], capture_output=True, text=True).stdout.strip(),
            "source_revision": subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=ROOT,
                capture_output=True, text=True).stdout.strip(),
            "os": platform.platform() + " " + platform.machine(),
            "cpu_count": os.cpu_count(),
            "host_memory_kib": _host_memory_kib(),
        }
        try:
            env["uname"] = os.uname().release
        except AttributeError:
            pass
        try:
            env["docker_version"] = subprocess.check_output(
                ["docker", "--version"], text=True).strip()
        except Exception as e:
            env["docker_version"] = "unavailable: " + str(e)
        report = {
            "environment": env,
            "scenarios": self.scenarios,
            "git_operations": self.git_ops,
            "server_lifetimes": self.lifetimes,
            "large_repository": getattr(self, "_large", {}),
            "concurrency_sweep": getattr(self, "_sweep", {}),
            "soak": getattr(self, "_soak", {}),
        }
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)


def main():
    p = argparse.ArgumentParser(description="Measure hardware minimums on this machine.")
    p.add_argument("--report", help="write measurements JSON to this path")
    p.add_argument("--soak-minutes", type=float, default=DEFAULT_SOAK_MINUTES,
                   help="soak duration in minutes (default 20)")
    p.add_argument("--commits", type=int, default=LARGE_COMMITS,
                   help="synthetic commit count for the large repository")
    p.add_argument("--big-files", type=int, default=LARGE_BIG_FILE_COUNT,
                   help="number of incompressible files appended to the large repository")
    p.add_argument("--big-file-bytes", type=int, default=LARGE_BIG_FILE_BYTES,
                   help="size in bytes of each incompressible file")
    p.add_argument("--workdir", default=str(ROOT / ".scratch"),
                   help="directory under which all synthetic repositories and temp data live")
    a = p.parse_args()
    workdir = Path(a.workdir)
    rp = a.report
    if rp is None:
        workdir.mkdir(parents=True, exist_ok=True)
        fd, rp = tempfile.mkstemp(prefix="hw-report-", suffix=".json", dir=str(workdir))
        os.close(fd)
    h = HardwareHarness(rp, a.soak_minutes, a.commits, a.big_files, a.big_file_bytes, workdir)
    sys.exit(h.run())


if __name__ == "__main__":
    main()
