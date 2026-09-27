"""Equivalent logical transactions for the two experimental backends."""
import json
import os
from pathlib import Path
import random
import time

from model import BODY, BUCKETS, COMMENT, issue_record, user_for

DSN = os.getenv("GITHEARTH_BENCH_PG_DSN", "host=127.0.0.1 port=15432 dbname=githearth_bench user=githearth_bench password=local-benchmark-only")
CLUSTER = os.getenv("GITHEARTH_BENCH_FDB_CLUSTER", str(Path(__file__).with_name("fdb.cluster")))


def encode(value):
    return json.dumps(value, separators=(",", ":")).encode()


class Postgres:
    def __init__(self):
        import psycopg
        self.psycopg = psycopg
        self.conn = psycopg.connect(DSN, autocommit=True, connect_timeout=5)
        self.conn.execute("SET default_transaction_isolation = 'serializable'")
        self.conn.execute("SET statement_timeout = '5s'")
        self.conn.execute("SET lock_timeout = '5s'")

    def close(self):
        self.conn.close()

    def seed(self, profile):
        # The fixed database name is a deliberate guard for this destructive experiment.
        if self.conn.info.dbname != "githearth_bench":
            raise ValueError("Refusing to reset a database other than githearth_bench")
        self.conn.execute(Path(__file__).with_name("schema.sql").read_text())
        self.conn.execute("TRUNCATE users, memberships, repositories, issues, comments, jobs, outbox, probes")
        with self.conn.transaction():
            # Bulk fixture loading is not a measured interactive transaction.
            # SET LOCAL restores the 5-second request limit when this block ends.
            self.conn.execute("SET LOCAL statement_timeout = '5min'")
            with self.conn.cursor().copy("COPY users FROM STDIN") as c:
                for user in range(1, profile.users + 1):
                    c.write_row((user, f"User {user}"))
            with self.conn.cursor().copy("COPY memberships FROM STDIN") as c:
                for tenant in range(1, profile.tenants + 1):
                    for local in range(1, profile.users_per_tenant + 1):
                        c.write_row((tenant, user_for(profile, tenant, local), 2))
            with self.conn.cursor().copy("COPY repositories (tenant,id,name) FROM STDIN") as c:
                for tenant in range(1, profile.tenants + 1):
                    for repo in range(1, profile.repos_per_tenant + 1):
                        c.write_row((tenant, repo, f"repo-{repo}"))
            with self.conn.cursor().copy("COPY issues FROM STDIN") as c:
                for tenant in range(1, profile.tenants + 1):
                    for repo in range(1, profile.repos_per_tenant + 1):
                        for issue in range(1, profile.issues_per_repo + 1):
                            r = issue_record(tenant, repo, issue)
                            c.write_row(tuple(r.values()))
            with self.conn.cursor().copy("COPY jobs (id,tenant,bucket,state) FROM STDIN") as c:
                for tenant in range(1, profile.tenants + 1):
                    for n in range(1000):
                        c.write_row((f"seed-{tenant}-{n:06}", tenant, n % BUCKETS, "queued"))
        self.conn.execute("ANALYZE")

    def execute(self, op):
        retries = 0
        deadline = time.monotonic() + 5
        while True:
            try:
                with self.conn.transaction():
                    return self._execute(op), retries
            except (self.psycopg.errors.SerializationFailure,
                    self.psycopg.errors.DeadlockDetected,
                    self.psycopg.errors.UniqueViolation):
                retries += 1
                if time.monotonic() >= deadline:
                    raise
                time.sleep(random.uniform(0, min(0.02, 0.0005 * 2 ** min(retries, 6))))

    def _execute(self, op):
        t, repo, user = op["tenant"], op.get("repo", 1), op.get("user", 1)
        kind, oid = op["kind"], op["id"]
        if kind in ("permission", "list", "comment"):
            allowed = self.conn.execute(
                "SELECT m.role FROM memberships m JOIN repositories r ON r.tenant=m.tenant "
                "WHERE m.tenant=%s AND m.user_id=%s AND r.id=%s", (t, user, repo)).fetchone()
            if kind == "permission":
                return bool(allowed and allowed[0] >= 1)
            if not allowed or allowed[0] < (2 if kind == "comment" else 1):
                raise PermissionError("Access denied")
        if kind == "list":
            rows = self.conn.execute(
                "SELECT tenant,repo,id,state,title,body,comment_count FROM issues "
                "WHERE tenant=%s AND repo=%s AND state='open' AND id<%s ORDER BY id DESC LIMIT 20",
                (t, repo, op["before"])).fetchall()
            return [dict(zip(("tenant", "repo", "id", "state", "title", "body", "comment_count"), row)) for row in rows]
        if kind == "comment":
            identity = (t, repo, op["issue"], COMMENT)
            old = self.conn.execute("SELECT tenant,repo,issue,body FROM comments WHERE operation_id=%s", (oid,)).fetchone()
            if old:
                if old != identity:
                    raise ValueError("Idempotency key reused for a different comment")
                return oid
            row = self.conn.execute(
                "UPDATE issues SET comment_count=comment_count+1 WHERE tenant=%s AND repo=%s AND id=%s RETURNING id",
                (t, repo, op["issue"])).fetchone()
            if row is None:
                raise ValueError("Missing issue")
            self.conn.execute("INSERT INTO comments VALUES (%s,%s,%s,%s,%s)", (oid, *identity))
            self.conn.execute("INSERT INTO outbox VALUES (%s,'comment',%s,%s)", ("comment:" + oid, t, oid))
            return oid
        if kind == "enqueue":
            old = self.conn.execute("SELECT tenant,bucket FROM jobs WHERE id=%s", (oid,)).fetchone()
            if old:
                if old != (t, op["bucket"]):
                    raise ValueError("Idempotency key reused for a different job")
                return oid
            self.conn.execute("INSERT INTO jobs (id,tenant,bucket,state) VALUES (%s,%s,%s,'queued')", (oid,t,op["bucket"]))
            self.conn.execute("INSERT INTO outbox VALUES (%s,'enqueue',%s,%s)", ("enqueue:" + oid,t,oid))
            return oid
        if kind == "claim":
            existing = self.conn.execute("SELECT id,tenant FROM jobs WHERE claim_id=%s", (oid,)).fetchone()
            if existing:
                if existing[1] != t:
                    raise ValueError("Claim id reused across tenants")
                return existing[0]
            job = self.conn.execute(
                "SELECT id FROM jobs WHERE tenant=%s AND bucket=%s AND state='queued' "
                "ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1", (t,op["bucket"])).fetchone()
            if job is None:
                return None
            self.conn.execute("UPDATE jobs SET state='running',claim_id=%s WHERE id=%s", (oid,job[0]))
            self.conn.execute("INSERT INTO outbox VALUES (%s,'claim',%s,%s)", ("claim:"+oid,t,job[0]))
            return job[0]
        raise ValueError(kind)

    def verify(self):
        with self.conn.transaction():
            counts = {name:int(value) for name,value in self.conn.execute("SELECT 'comments',count(*) FROM comments UNION ALL SELECT 'comment_count',coalesce(sum(comment_count),0) FROM issues UNION ALL SELECT 'comment_events',count(*) FROM outbox WHERE kind='comment'").fetchall()}
            bad_comments = self.conn.execute("SELECT count(*) FROM comments c LEFT JOIN outbox o ON o.id='comment:'||c.operation_id WHERE o.id IS NULL OR o.payload<>c.operation_id OR o.tenant<>c.tenant").fetchone()[0]
            bad_claims = self.conn.execute("SELECT count(*) FROM jobs j LEFT JOIN outbox o ON o.id='claim:'||j.claim_id WHERE j.state='running' AND (j.claim_id IS NULL OR o.id IS NULL OR o.payload<>j.id OR o.tenant<>j.tenant)").fetchone()[0]
            assert len(set(counts.values())) == 1, counts
            assert bad_comments == 0 and bad_claims == 0, (bad_comments, bad_claims)
            running = self.conn.execute("SELECT count(*) FROM jobs WHERE state='running'").fetchone()[0]
            claims = self.conn.execute("SELECT count(*) FROM outbox WHERE kind='claim'").fetchone()[0]
            assert running == claims, (running,claims)
            return {**counts, "bad_comments": bad_comments, "bad_claims": bad_claims}

    def probe(self, key, value=None):
        if value is not None:
            with self.conn.transaction():
                self.conn.execute("INSERT INTO probes VALUES (%s,%s) ON CONFLICT (id) DO UPDATE SET payload=EXCLUDED.payload", (key,value))
        row = self.conn.execute("SELECT payload FROM probes WHERE id=%s", (key,)).fetchone()
        return row[0] if row else None

    def metadata(self):
        return {"version": self.conn.execute("SELECT version()").fetchone()[0],
                "database_bytes": self.conn.execute("SELECT pg_database_size(current_database())").fetchone()[0],
                "settings": dict(self.conn.execute("SELECT name,setting FROM pg_settings WHERE name IN ('fsync','synchronous_commit','full_page_writes','shared_buffers','default_transaction_isolation')").fetchall())}


class FoundationDB:
    ROOT = "githearth-bench-v1"

    def __init__(self):
        import fdb
        fdb.api_version(730)
        import fdb.tuple
        self.fdb = fdb
        self.db = fdb.open(CLUSTER)

    def close(self):
        pass

    def key(self, *parts):
        return self.fdb.tuple.pack((self.ROOT, *parts))

    def range(self, *parts):
        return self.fdb.tuple.range((self.ROOT, *parts))

    def transaction(self, callback):
        tr = self.db.create_transaction()
        tr.options.set_timeout(5000)
        retries = 0
        while True:
            try:
                result = callback(tr)
                tr.commit().wait()
                return result, retries
            except self.fdb.FDBError as exc:
                retries += 1
                tr.on_error(exc.code).wait()

    def seed(self, profile):
        self.transaction(lambda tr: tr.clear_range(self.range().start, self.range().stop))
        batch = []

        def write(key, value):
            batch.append((key, value))
            if len(batch) >= 500:
                flush()

        def flush():
            if not batch:
                return
            def commit(tr):
                for key, value in batch:
                    tr[key] = value
            self.transaction(commit)
            batch.clear()

        for user in range(1, profile.users + 1):
            write(self.key("user",user), encode({"id":user,"display_name":f"User {user}"}))
        for tenant in range(1, profile.tenants + 1):
            for local in range(1, profile.users_per_tenant + 1):
                write(self.key("member",tenant,user_for(profile,tenant,local)), b"2")
            for repo in range(1, profile.repos_per_tenant + 1):
                write(self.key("repo",tenant,repo), encode({"name":f"repo-{repo}","policy_version":1}))
                write(self.key("repo-name",tenant,f"repo-{repo}"), encode(repo))
                for issue in range(1, profile.issues_per_repo + 1):
                    row = issue_record(tenant,repo,issue)
                    write(self.key("issue",tenant,repo,issue), encode(row))
                    write(self.key("issue-list",tenant,repo,row["state"],-issue), self.key("issue",tenant,repo,issue))
            for n in range(1000):
                jid = f"seed-{tenant}-{n:06}"
                write(self.key("job",jid), encode({"id":jid,"tenant":tenant,"bucket":n%BUCKETS,"state":"queued","claim_id":None}))
                write(self.key("ready",tenant,n%BUCKETS,jid), b"")
        flush()

    def execute(self, op):
        return self.transaction(lambda tr: self._execute(tr,op))

    def _execute(self, tr, op):
        t, repo, user = op["tenant"], op.get("repo",1), op.get("user",1)
        kind, oid = op["kind"], op["id"]
        if kind in ("permission","list","comment"):
            member = tr[self.key("member",t,user)]
            repository = tr[self.key("repo",t,repo)]
            role = int(member.value or b"0")
            allowed = bool(repository.value) and role >= (2 if kind=="comment" else 1)
            if kind == "permission":
                return allowed
            if not allowed:
                raise PermissionError("Access denied")
        if kind == "list":
            r = self.range("issue-list",t,repo,"open")
            begin = self.key("issue-list",t,repo,"open",-op["before"]+1)
            entries = list(tr.get_range(begin,r.stop,limit=20))
            futures = [tr[entry.value] for entry in entries]
            return [json.loads(future.value) for future in futures]
        if kind == "comment":
            identity = {"tenant":t,"repo":repo,"issue":op["issue"],"body":COMMENT}
            old = tr[self.key("comment",oid)].value
            if old:
                if json.loads(old) != identity:
                    raise ValueError("Idempotency key reused for a different comment")
                return oid
            ikey = self.key("issue",t,repo,op["issue"])
            raw = tr[ikey].value
            if raw is None:
                raise ValueError("Missing issue")
            row = json.loads(raw)
            row["comment_count"] += 1
            tr[ikey] = encode(row)
            tr[self.key("comment",oid)] = encode(identity)
            tr[self.key("outbox","comment",oid)] = encode({"tenant":t,"payload":oid})
            return oid
        if kind == "enqueue":
            old = tr[self.key("job",oid)].value
            if old:
                row = json.loads(old)
                if (row["tenant"],row["bucket"]) != (t,op["bucket"]):
                    raise ValueError("Idempotency key reused for a different job")
                return oid
            tr[self.key("job",oid)] = encode({"id":oid,"tenant":t,"bucket":op["bucket"],"state":"queued","claim_id":None})
            tr[self.key("ready",t,op["bucket"],oid)] = b""
            tr[self.key("outbox","enqueue",oid)] = encode({"tenant":t,"payload":oid})
            return oid
        if kind == "claim":
            old = tr[self.key("claim",oid)].value
            if old:
                row = json.loads(old)
                if row["tenant"] != t:
                    raise ValueError("Claim id reused across tenants")
                return row["job"]
            r = self.range("ready",t,op["bucket"])
            entries = list(tr.get_range(r.start,r.stop,limit=1))
            if not entries:
                return None
            key = entries[0].key
            jid = self.fdb.tuple.unpack(key)[-1]
            jkey = self.key("job",jid)
            job = json.loads(tr[jkey].value)
            assert job["state"] == "queued"
            job.update(state="running",claim_id=oid)
            tr[jkey] = encode(job)
            del tr[key]
            tr[self.key("claim",oid)] = encode({"tenant":t,"job":jid})
            tr[self.key("outbox","claim",oid)] = encode({"tenant":t,"payload":jid})
            return jid
        raise ValueError(kind)

    def scan(self, *parts):
        # Verification runs with writers stopped; page to avoid long transactions.
        bounds = self.range(*parts)
        begin = bounds.start
        while True:
            rows, _ = self.transaction(lambda tr: list(tr.get_range(begin,bounds.stop,limit=1000)))
            if not rows:
                return
            yield from rows
            begin = rows[-1].key + b"\x00"

    def verify(self):
        comments = {self.fdb.tuple.unpack(kv.key)[-1]:json.loads(kv.value) for kv in self.scan("comment")}
        events = {self.fdb.tuple.unpack(kv.key)[-1]:json.loads(kv.value) for kv in self.scan("outbox","comment")}
        assert set(comments) == set(events)
        for oid, row in comments.items():
            assert events[oid] == {"tenant":row["tenant"],"payload":oid}
        total = sum(json.loads(kv.value)["comment_count"] for kv in self.scan("issue"))
        assert total == len(comments), (total,len(comments))
        claims = {self.fdb.tuple.unpack(kv.key)[-1]:json.loads(kv.value) for kv in self.scan("claim")}
        claim_events = {self.fdb.tuple.unpack(kv.key)[-1]:json.loads(kv.value) for kv in self.scan("outbox","claim")}
        ready = {self.fdb.tuple.unpack(kv.key)[-1] for kv in self.scan("ready")}
        running = 0
        for kv in self.scan("job"):
            job = json.loads(kv.value)
            if job["state"] == "running":
                running += 1
                assert job["id"] not in ready
                assert claims[job["claim_id"]] == {"tenant":job["tenant"],"job":job["id"]}
                assert claim_events[job["claim_id"]] == {"tenant":job["tenant"],"payload":job["id"]}
            else:
                assert job["id"] in ready
        assert running == len(claims) == len(claim_events)
        return {"comments":len(comments),"comment_count":total,"comment_events":len(events),"bad_comments":0,"bad_claims":0}

    def probe(self, key, value=None):
        def action(tr):
            k = self.key("probe",key)
            if value is not None:
                tr[k] = value.encode()
            result = tr[k].value
            return result.decode() if result else None
        return self.transaction(action)[0]

    def metadata(self):
        return {"version":"7.3.77", "api_version":730, "storage_engine":"ssd-2", "redundancy":"single"}


def connect(backend):
    return Postgres() if backend == "postgres" else FoundationDB()
