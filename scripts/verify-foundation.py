#!/usr/bin/env python3
"""Verify M1 using an isolated disposable PostgreSQL container. Requires Docker/Rust."""
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]

def run(args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, **kwargs)

def main():
    name = 'githearth-review-' + uuid.uuid4().hex[:12]
    server = None
    with tempfile.TemporaryDirectory(prefix='githearth-review-') as folder:
        secret_path = Path(folder) / 'db.env'
        password = secrets.token_hex(32)
        secret_path.write_text('POSTGRES_USER=githearth\nPOSTGRES_DB=contract\nPOSTGRES_PASSWORD=' + password + '\n')
        secret_path.chmod(0o600)
        log_path = Path(folder) / 'app.log'
        try:
            # Docker reallocates automatic host ports on restart; keep the DB address stable.
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                database_port = sock.getsockname()[1]
            run(['docker', 'run', '--detach', '--rm', '--name', name, '--env-file', str(secret_path), '--publish', f'127.0.0.1:{database_port}:5432', 'postgres:17-bookworm'], stdout=subprocess.DEVNULL)
            for _ in range(60):
                if subprocess.run(['docker', 'exec', name, 'pg_isready', '-h', '127.0.0.1', '-U', 'githearth', '-d', 'contract'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError('PostgreSQL startup timed out')
            mapping = subprocess.check_output(['docker', 'port', name, '5432/tcp'], text=True).strip()
            port = int(mapping.rsplit(':', 1)[1])
            assert port == database_port
            database = f'postgres://githearth:{password}@127.0.0.1:{port}/'
            env = dict(os.environ, GITHEARTH_TEST_DATABASE_URL=database + 'contract')
            run(['cargo', 'test', '-p', 'githearth-server', '--test', 'postgres', '--', '--ignored', '--nocapture'], env=env)
            run(['docker', 'exec', name, 'createdb', '-U', 'githearth', 'smoke'])
            run(['cargo', 'build', '-p', 'githearth-server'])
            binary = str(ROOT / 'target/debug/githearth-server')
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                http_port = sock.getsockname()[1]
            env = dict(os.environ, GITHEARTH_DATABASE_URL=database + 'smoke', GITHEARTH_BIND=f'127.0.0.1:{http_port}', GITHEARTH_DATABASE_TIMEOUT_SECONDS='1')
            run([binary, 'migrate'], env=env, stdout=subprocess.DEVNULL)
            admin_password = secrets.token_urlsafe(32)
            run([binary, 'bootstrap-admin', '--username', 'review-admin'], input=admin_password, text=True, env=env, stdout=subprocess.DEVNULL)
            duplicate = subprocess.run([binary, 'bootstrap-admin', '--username', 'replacement-admin'], input=secrets.token_urlsafe(32), text=True, env=env, capture_output=True)
            assert duplicate.returncode != 0 and 'already exists' in duplicate.stderr
            with log_path.open('w') as log:
                server = subprocess.Popen([binary, 'serve'], env=env, stdout=log, stderr=log)
                def restart_server():
                    nonlocal server
                    server.terminate()
                    server.wait(timeout=20)
                    assert server.returncode == 0
                    server = subprocess.Popen([binary, 'serve'], env=env, stdout=log, stderr=log)
                    return server
                token = verify_http(server, http_port, name, admin_password, restart_server, mapping)
                server.terminate()
                server.wait(timeout=20)
                server = None
            assert password not in log_path.read_text()
            assert admin_password not in log_path.read_text()
            assert token not in log_path.read_text()
            print('Independent M1 PostgreSQL and executable HTTP verification passed, including DB and HTTP process restart persistence.')
        finally:
            if server is not None and server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()
            subprocess.run(['docker', 'unpause', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            subprocess.run(['docker', 'rm', '--force', '--volumes', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def verify_http(server, port, container, password, restart_server, expected_mapping):
    base = f'http://127.0.0.1:{port}'
    def call(method, path, data=None, token=None):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        request = urllib.request.Request(base + path, data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                raw = response.read()
                return response.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as error:
            return error.code, None
    for _ in range(100):
        assert server.poll() is None, 'HTTP process exited'
        try:
            if call('GET', '/health/ready')[0] == 200:
                break
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    else:
        raise RuntimeError('HTTP readiness timed out')
    status, session = call('POST', '/api/v1/login', {'username': 'review-admin', 'password': password})
    assert status == 200
    token = session['token']
    assert call('GET', '/api/v1/me', token=token)[0] == 200
    status, organization = call('POST', '/api/v1/organizations', {'slug': 'review-team', 'name': 'Review Team'}, token)
    assert status == 201
    assert call('GET', '/api/v1/organizations/' + organization['id'], token=token)[0] == 200
    try:
        run(['docker', 'pause', container], stdout=subprocess.DEVNULL)
        assert call('GET', '/health/live')[0] == 200
        assert call('GET', '/health/ready')[0] == 503
    finally:
        run(['docker', 'unpause', container], stdout=subprocess.DEVNULL)
    run(['docker', 'restart', container], stdout=subprocess.DEVNULL)
    assert subprocess.check_output(['docker', 'port', container, '5432/tcp'], text=True).strip() == expected_mapping
    for _ in range(30):
        if call('GET', '/health/ready')[0] == 200:
            break
        time.sleep(0.2)
    else:
        raise RuntimeError('readiness did not recover')
    assert call('GET', '/api/v1/organizations/' + organization['id'], token=token)[0] == 200
    server = restart_server()
    for _ in range(100):
        assert server.poll() is None
        try:
            if call('GET', '/health/ready')[0] == 200:
                break
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    else:
        raise RuntimeError('HTTP executable restart readiness timed out')
    assert call('GET', '/api/v1/me', token=token)[0] == 200
    assert call('GET', '/api/v1/organizations/' + organization['id'], token=token)[1]['id'] == organization['id']
    assert call('POST', '/api/v1/logout', token=token)[0] == 204
    assert call('GET', '/api/v1/me', token=token)[0] == 401
    return token

if __name__ == '__main__':
    main()
