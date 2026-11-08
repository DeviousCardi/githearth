#!/usr/bin/env python3
"""B26 TLS verification harness: nginx TLS proxy, streaming push, cookie
assertions, HSTS, plain-HTTP rejection.

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
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NGINX_IMAGE = "nginx:stable"


def _load_m2():
    spec = importlib.util.spec_from_file_location(
        "verify_m2", ROOT / "scripts" / "verify-m2.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_m2 = _load_m2()
HarnessError = _m2.HarnessError


class TlsHarness(_m2.Harness):
    """Subclass M2 Harness: add TLS nginx proxy and verify TLS behaviour."""

    def __init__(self, report_path):
        self.name = "githearth-b26-" + uuid.uuid4().hex[:12]
        super().__init__(report_path)
        self.admin_username = "b26-admin"
        self.admin_password = secrets.token_urlsafe(32)
        self.nginx_container = None
        self.nginx_name = "githearth-b26-nginx-" + uuid.uuid4().hex[:12]
        self.ca_cert = self.tmp / "ca.crt"
        self.ca_key = self.tmp / "ca.key"
        self.server_cert = self.tmp / "server.crt"
        self.server_key = self.tmp / "server.key"
        self.server_csr = self.tmp / "server.csr"
        self.tls_port = None
        self._nginx_conf_path = self.tmp / "nginx.conf"
        self._certs_dir = self.tmp / "certs"
        self._certs_dir.mkdir(exist_ok=True)
        self.tls_results = []
        self._extra_dirs = []

    def _generate_certs(self):
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(self.ca_key), "-out", str(self.ca_cert),
             "-days", "1", "-subj", "/CN=TestCA"],
            check=True, capture_output=True)
        subprocess.run(
            ["openssl", "req", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(self.server_key), "-out", str(self.server_csr),
             "-subj", "/CN=127.0.0.1"],
            check=True, capture_output=True)
        ext = self.tmp / "ext.cnf"
        ext.write_text(
            "subjectAltName=DNS:localhost,IP:127.0.0.1\n"
            "basicConstraints=CA:FALSE\n")
        subprocess.run(
            ["openssl", "x509", "-req", "-in", str(self.server_csr),
             "-CA", str(self.ca_cert), "-CAkey", str(self.ca_key),
             "-CAcreateserial", "-out", str(self.server_cert),
             "-days", "1", "-extfile", str(ext)],
            check=True, capture_output=True)

    def tls_api(self, method, path, data=None, token=None, headers=None, body=None):
        url = f"https://127.0.0.1:{self.tls_port}" + path
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

        ctx = ssl.create_default_context(cafile=str(self.ca_cert))
        req = urllib.request.Request(url, data=req_data, headers=hdrs, method=method)
        try:
            with urllib.request.urlopen(req, timeout=60, context=ctx) as response:
                return response.status, response.getheaders(), response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.headers.items(), error.read()

    def tls_api_json(self, method, path, data=None, token=None, headers=None):
        status, _, body = self.tls_api(method, path, data=data, token=token, headers=headers)
        parsed = None
        if body:
            try:
                parsed = json.loads(body)
            except ValueError:
                parsed = None
        return status, parsed

    def start_nginx(self):
        self._generate_certs()
        self.tls_port = self.free_port()
        backend_port = self.server.port

        shutil.copy2(str(self.ca_cert), self._certs_dir / "ca.crt")
        shutil.copy2(str(self.server_key), self._certs_dir / "server.key")
        shutil.copy2(str(self.server_cert), self._certs_dir / "server.crt")

        # The runbook's nginx block verbatim (docs/operations/tls.md), so the doc and the
        # drill cannot drift. Only host-specific values are substituted; each must exist.
        doc = (ROOT / "docs" / "operations" / "tls.md").read_text()
        block = re.search(r"```nginx\n(.*?)```", doc, re.S)
        if not block:
            raise HarnessError("no nginx block in docs/operations/tls.md")
        conf = block.group(1)
        for old, new_value in (
            ("server 127.0.0.1:8080;", f"server 127.0.0.1:{backend_port};"),
            ("listen 443 ssl http2;", f"listen 127.0.0.1:{self.tls_port} ssl http2;"),
            # The host network may have no IPv6; the drill listens on loopback only.
            ("listen [::]:443 ssl http2;", ""),
            ("/etc/ssl/certs/githearth.crt", "/etc/nginx/certs/server.crt"),
            ("/etc/ssl/private/githearth.key", "/etc/nginx/certs/server.key"),
        ):
            if old not in conf:
                raise HarnessError(f"runbook nginx block changed; missing {old!r}")
            conf = conf.replace(old, new_value)
        # nginx runs with --network host and the temp dir mounted at /etc/nginx.
        self._nginx_conf_path.write_text(
            "events { worker_connections 1024; }\n"
            "http {\n"
            "    access_log /dev/stdout;\n"
            "    error_log  /dev/stderr;\n"
            + conf +
            "}\n"
        )

        subprocess.run(
            ["docker", "run", "--detach", "--rm", "--name", self.nginx_name,
             "--network", "host",
             "-v", f"{self.tmp}:/etc/nginx:ro",
             NGINX_IMAGE],
            check=True, stdout=subprocess.DEVNULL)
        self.nginx_container = self.nginx_name

        for _ in range(60):
            try:
                ctx = ssl.create_default_context(cafile=str(self.ca_cert))
                req = urllib.request.Request(f"https://127.0.0.1:{self.tls_port}/health/ready")
                with urllib.request.urlopen(req, timeout=5, context=ctx) as resp:
                    if resp.status == 200:
                        return
            except Exception:
                pass
            time.sleep(0.5)
        raise HarnessError("nginx TLS proxy startup timed out")

    def git_env(self, user=None, password=None):
        env = super().git_env(user, password)
        env.pop("GIT_SSL_NO_VERIFY", None)
        env["GIT_SSL_CAINFO"] = str(self.ca_cert)
        return env

    @property
    def repo_url(self):
        return f"https://127.0.0.1:{self.tls_port}/{self.org_slug}/{self.repo_name}.git"

    def run(self):
        signal.signal(signal.SIGINT, lambda *_a: sys.exit(1))
        signal.signal(signal.SIGTERM, lambda *_a: sys.exit(1))
        try:
            self.start_db()
            self.scenario("s1_fresh_install_tls", self.s1_fresh_install_tls)
            self.scenario("s2_tls_push_and_fetch", self.s2_tls_push_and_fetch)
            self.scenario("s3_tls_cookies_and_hsts", self.s3_tls_cookies_and_hsts)
            self.scenario("s4_plain_http_rejected", self.s4_plain_http_rejected)
        finally:
            self.cleanup()
        self.write_report()
        ps = sum(1 for s in self.scenarios if s["result"] == "PASS")
        print(f"\nB26 TLS verification: {ps}/{len(self.scenarios)} scenarios passed")
        print(f"report: {self.report_path}")
        return 0 if ps == len(self.scenarios) else 1

    # ================================================================== S1

    def s1_fresh_install_tls(self):
        subprocess.run(
            [self.binary, "migrate"],
            env=self.server_env(self._m2_main_db(), self.storage_root),
            check=True, capture_output=True, text=True)
        b = subprocess.run(
            [self.binary, "bootstrap-admin", "--username", self.admin_username],
            env=self.server_env(self._m2_main_db(), self.storage_root),
            input=self.admin_password, text=True, capture_output=True)
        if b.returncode != 0:
            raise HarnessError("bootstrap-admin failed: " + b.stderr.strip()[:300])

        self.start_server(self._m2_main_db(), self.storage_root, "initial")
        self.start_nginx()

        tok = self.api_session_tls(self.admin_username, self.admin_password)
        if not tok:
            raise HarnessError("TLS login failed")

        st, org = self.tls_api_json(
            "POST", "/api/v1/organizations",
            {"slug": "b26-org", "name": "B26 Org"}, token=tok)
        if st != 201:
            raise HarnessError(f"create org over TLS status {st}")
        self.org_slug = org["slug"]
        self.org_id = org["id"]

        st, repo = self.tls_api_json(
            "POST", f"/api/v1/organizations/{self.org_slug}/repositories",
            {"name": "b26-repo", "visibility": "private"}, token=tok)
        if st != 201:
            raise HarnessError(f"create repo over TLS status {st}")
        self.repo_name = "b26-repo"
        self.admin_token = tok

    def api_session_tls(self, user, pw):
        st, s = self.tls_api_json("POST", "/api/v1/login",
                                   {"username": user, "password": pw})
        if st != 200 or not s.get("token"):
            raise HarnessError(f"TLS login for {user} status {st}: {s}")
        return s["token"]

    # ================================================================== S2

    def s2_tls_push_and_fetch(self):
        self.main_work.mkdir()
        self.wgit("init", "-b", "main")
        self.wgit("config", "user.name", "B26 Test")
        self.wgit("config", "user.email", "b26@example.com")

        (self.main_work / "README.md").write_text("# B26 TLS Test\n")
        self.wgit("add", "-A")
        self.wgit("commit", "-m", "initial")

        self.git("push", self.repo_url, "refs/heads/main",
                 user=self.admin_username, password=self.admin_password,
                 label="tls push", cwd=str(self.main_work))

# push 20 MiB random data: proves streaming and body size over TLS
        data = os.urandom(20 * 1024 * 1024)
        (self.main_work / "big.bin").write_bytes(data)
        self.wgit("add", "big.bin")
        self.wgit("commit", "-m", "20MiB")

        push_out =         self.git("push", self.repo_url, "HEAD:refs/heads/tls-20mb",
                             user=self.admin_username, password=self.admin_password,
                             label="tls 20MiB push", timeout=180,
                             cwd=str(self.main_work))

        ct = tempfile.mkdtemp(prefix="b26-clone-")
        self._extra_dirs.append(ct)
        subprocess.run(["git", "init", ct], capture_output=True, check=True)
        subprocess.run(
            ["git", "remote", "add", "origin", self.repo_url],
            capture_output=True, check=True, cwd=ct)
        gt_env = self.git_env(self.admin_username, self.admin_password)
        fet = subprocess.run(
            ["git", "-c", f"credential.helper={self.cred_helper}",
             "fetch", "origin", "refs/heads/tls-20mb"],
            capture_output=True, text=True, cwd=ct,
            env=gt_env, timeout=300)
        if fet.returncode != 0:
            raise HarnessError(f"fetch failed: {fet.stderr.strip()[:400]}")

        show = subprocess.run(
            ["git", "show", "FETCH_HEAD:big.bin"],
            capture_output=True, cwd=ct)
        if show.returncode != 0:
            raise HarnessError(f"show big.bin failed: {show.stderr.strip()[:200]}")

        subprocess.run(
            ["git", "reset", "--hard", "FETCH_HEAD"],
            capture_output=True, check=True, cwd=ct)
        found = sorted([p.name for p in Path(ct).iterdir()])
        if "big.bin" not in found:
            raise HarnessError(f"big.bin missing after reset. ct={ct}; found={found}")

    # ================================================================== S3

    def s3_tls_cookies_and_hsts(self):
        ctx = ssl.create_default_context(cafile=str(self.ca_cert))

        login_req = urllib.request.Request(
            f"https://127.0.0.1:{self.tls_port}/login")
        with urllib.request.urlopen(login_req, timeout=10, context=ctx) as resp:
            body = resp.read().decode()
            hdrs = {k.lower(): v for k, v in resp.getheaders()}
            csrf_cookie = _m2.Harness.get_cookie(resp.getheaders(), "githearth_csrf")
            if not csrf_cookie:
                raise HarnessError("no CSRF cookie on login page")

        csrf_match = re.search(r'name="csrf_token" value="([^"]+)"', body)
        if not csrf_match:
            raise HarnessError("no csrf_token on login form")

        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
            def http_error_302(self, req, fp, code, msg, headers):
                return fp
            http_error_301 = http_error_303 = http_error_307 = http_error_302

        post_data = urllib.parse.urlencode({
            "csrf_token": csrf_match.group(1),
            "username": self.admin_username,
            "password": self.admin_password,
        }).encode()
        req = urllib.request.Request(
            f"https://127.0.0.1:{self.tls_port}/login",
            data=post_data, method="POST")
        req.add_header("Cookie", f"githearth_csrf={csrf_cookie}")

        login_opener = urllib.request.build_opener(
            urllib.request.HTTPSHandler(context=ctx), NoRedirect)
        try:
            with login_opener.open(req, timeout=10) as resp:
                pass
        except urllib.error.HTTPError as e:
            resp = e

        if resp.status not in (302, 303):
            raise HarnessError(f"login POST returned {resp.status}, expected redirect")

        session_cookie = _m2.Harness.get_cookie(
            resp.getheaders(), "githearth_session")
        if not session_cookie:
            raise HarnessError("no session cookie after login")

        set_cookies = [v for k, v in resp.getheaders() if k.lower() == "set-cookie"]
        session_header = ""
        for sc in set_cookies:
            if "githearth_session=" in sc:
                session_header = sc.lower()
                break

        missing = []
        for flag in ["secure", "httponly", "samesite=lax"]:
            if flag not in session_header:
                missing.append(flag)
        if missing:
            raise HarnessError(f"session cookie missing flags: {missing}")

        self.tls_results.append(
            {"check": "session_cookie_flags", "result": "pass",
             "value": session_header})

        hsts_req = urllib.request.Request(
            f"https://127.0.0.1:{self.tls_port}/")
        with urllib.request.urlopen(hsts_req, timeout=10, context=ctx) as resp:
            hsts = resp.getheader("Strict-Transport-Security")
            if not hsts:
                raise HarnessError("missing HSTS header")
            if "max-age=31536000" not in hsts:
                raise HarnessError(f"unexpected HSTS value: {hsts}")

        self.tls_results.append(
            {"check": "hsts_header", "result": "pass",
             "value": hsts})

    # ================================================================== S4

    def s4_plain_http_rejected(self):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{self.tls_port}/health/ready")
            urllib.request.urlopen(req, timeout=5)
            raise HarnessError("plain HTTP to TLS port should fail")
        except (urllib.error.URLError, ConnectionError, OSError,
                urllib.error.HTTPError) as e:
            code = getattr(e, "code", None)
            if code and code < 400:
                raise HarnessError(f"plain HTTP to TLS port unexpectedly OK: {code}")
        self.tls_results.append(
            {"check": "plain_http_rejected", "result": "pass"})

    # ================================================================== infra

    def server_env(self, db, storage_root):
        env = super().server_env(db, storage_root)
        env["GITHEARTH_SECURE_COOKIES"] = "true"
        env["GITHEARTH_REQUEST_TIMEOUT_SECONDS"] = "120"
        env["GITHEARTH_GIT_TIMEOUT_SECONDS"] = "900"
        return env

    def _m2_main_db(self):
        return _m2.MAIN_DB

    def start_server(self, db, storage_root, phase):
        srv = super().start_server(db, storage_root, phase)
        return srv

    def write_report(self):
        info = {}
        try:
            info["git_version"] = subprocess.run(
                ["git", "--version"], capture_output=True, text=True).stdout.strip()
        except Exception:
            info["git_version"] = "unknown"
        try:
            info["source_revision"] = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=ROOT,
                capture_output=True, text=True).stdout.strip()
        except Exception:
            info["source_revision"] = "unknown"
        info["os"] = platform.platform() + " " + platform.machine()
        info["nginx_image"] = NGINX_IMAGE
        info["tls_port"] = self.tls_port
        report = {
            "environment": info,
            "scenarios": self.scenarios,
            "git_operations": self.git_ops,
            "server_lifetimes": self.lifetimes,
            "tls_checks": self.tls_results,
        }
        with open(self.report_path, "w") as f:
            json.dump(report, f, indent=2, sort_keys=True)

    def cleanup(self):
        for name in [self.nginx_name]:
            subprocess.run(
                ["docker", "rm", "--force", "--volumes", name],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for d in getattr(self, "_extra_dirs", []):
            shutil.rmtree(d, ignore_errors=True)
        super().cleanup()


def main():
    p = argparse.ArgumentParser(description="Verify B26 TLS end to end.")
    p.add_argument("--report", help="write measurements JSON to this path")
    a = p.parse_args()
    rp = a.report
    if rp is None:
        fd, rp = tempfile.mkstemp(prefix="b26-tls-", suffix=".json")
        os.close(fd)
    h = TlsHarness(rp)
    sys.exit(h.run())


if __name__ == "__main__":
    main()
