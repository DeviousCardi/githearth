"""Shared, deterministic dataset and logical operation definitions."""
from dataclasses import asdict, dataclass
import random


@dataclass(frozen=True)
class Profile:
    users: int
    tenants: int
    repos_per_tenant: int
    issues_per_repo: int = 100

    @property
    def users_per_tenant(self):
        return self.users // self.tenants

    @property
    def issue_count(self):
        return self.tenants * self.repos_per_tenant * self.issues_per_repo

    def describe(self):
        return {**asdict(self), "total_issues": self.issue_count}


PROFILES = {
    "solo": Profile(1, 1, 10),
    "small": Profile(100, 1, 50),
    "growth": Profile(10_000, 10, 100),
    "enterprise": Profile(100_000, 1, 10_000),
}
BODY = "GitHearth benchmark issue body. " * 16
COMMENT = "Review comment for a deterministic synthetic change. " * 4
BUCKETS = 16


def user_for(profile, tenant, local=1):
    return (tenant - 1) * profile.users_per_tenant + local


def issue_record(tenant, repo, issue):
    return {
        "tenant": tenant, "repo": repo, "id": issue,
        "state": "closed" if issue % 4 == 0 else "open",
        "title": f"Issue {issue} in repository {repo}",
        "body": BODY, "comment_count": 0,
    }


def operation(rng: random.Random, profile, operation_id, scenario):
    tenant = rng.randint(1, profile.tenants)
    repo = rng.randint(1, profile.repos_per_tenant)
    issue = rng.randint(1, profile.issues_per_repo)
    user = user_for(profile, tenant, rng.randint(1, profile.users_per_tenant))
    if scenario == "hot":
        return {"kind": "comment", "tenant": 1, "repo": 1, "issue": 1,
                "user": 1, "id": operation_id}
    n = rng.randrange(100)
    kind = "permission" if n < 30 else "list" if n < 55 else "comment" if n < 80 else "enqueue" if n < 90 else "claim"
    return {"kind": kind, "tenant": tenant, "repo": repo, "issue": issue,
            "user": user, "id": operation_id, "before": rng.randint(21, profile.issues_per_repo + 1),
            "bucket": rng.randrange(BUCKETS)}
