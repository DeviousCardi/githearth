#!/usr/bin/env python3
"""Tests for scripts/generate-third-party-notices.py."""

import importlib.util
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "generate-third-party-notices.py"

_spec = importlib.util.spec_from_file_location("generate_third_party_notices", SCRIPT)
assert _spec is not None and _spec.loader is not None
_generate = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_generate)


def _meta(packages, nodes):
    return {
        "packages": packages,
        "workspace_members": [
            p["id"] for p in packages if p["name"] in _generate.BINARY_CRATES
        ],
        "resolve": {"nodes": nodes},
    }


class ThirdPartyDependenciesTests(unittest.TestCase):
    def test_crate_without_license_and_not_allowlisted_fails(self):
        packages = [
            {
                "id": "githearth-server 0.0.1",
                "name": "githearth-server",
                "version": "0.0.1",
                "license": "BUSL-1.1",
            },
            {
                "id": "dep 1.0.0",
                "name": "dep",
                "version": "1.0.0",
                "license": None,
            },
        ]
        nodes = [
            {
                "id": "githearth-server 0.0.1",
                "deps": [{"pkg": "dep 1.0.0", "dep_kinds": [{"kind": None}]}],
            },
            {"id": "dep 1.0.0", "deps": []},
        ]
        with self.assertRaises(_generate.MissingLicenseError):
            _generate.third_party_dependencies(_meta(packages, nodes))

    def test_crate_without_license_but_allowlisted_uses_allowlist_licence(self):
        packages = [
            {
                "id": "githearth-server 0.0.1",
                "name": "githearth-server",
                "version": "0.0.1",
                "license": "BUSL-1.1",
            },
            {
                "id": "allowlisted 1.0.0",
                "name": "allowlisted",
                "version": "1.0.0",
                "license": None,
            },
        ]
        nodes = [
            {
                "id": "githearth-server 0.0.1",
                "deps": [
                    {"pkg": "allowlisted 1.0.0", "dep_kinds": [{"kind": None}]}
                ],
            },
            {"id": "allowlisted 1.0.0", "deps": []},
        ]
        _generate.ALLOWLIST["allowlisted"] = "MIT OR Apache-2.0"
        self.addCleanup(_generate.ALLOWLIST.pop, "allowlisted")
        deps = _generate.third_party_dependencies(_meta(packages, nodes))
        self.assertEqual(deps, [("allowlisted", "1.0.0", "MIT OR Apache-2.0")])


if __name__ == "__main__":
    unittest.main()
