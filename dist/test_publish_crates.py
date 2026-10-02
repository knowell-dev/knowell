"""Offline tests for dist/publish_crates.py (no cargo, no network)."""

import unittest

import publish_crates


def pkg(name, deps=(), publish=None, dev=()):
    dependencies = [{"name": d, "kind": None} for d in deps] + [{"name": d, "kind": "dev"} for d in dev]
    return {"id": f"{name} 1.0.0", "name": name, "version": "1.0.0", "publish": publish, "dependencies": dependencies}


def meta(*pkgs):
    return {"packages": list(pkgs), "workspace_members": [p["id"] for p in pkgs]}


class Order(unittest.TestCase):
    def test_dependencies_come_first(self):
        order = publish_crates.publish_order(
            meta(pkg("app", ["lib-b", "lib-a"]), pkg("lib-b", ["core"]), pkg("lib-a", ["core"]), pkg("core"))
        )
        self.assertEqual([n for n, _ in order], ["core", "lib-a", "lib-b", "app"])

    def test_unpublishable_members_are_skipped(self):
        order = publish_crates.publish_order(meta(pkg("a"), pkg("private", publish=[])))
        self.assertEqual([n for n, _ in order], ["a"])

    def test_dev_dependencies_do_not_create_cycles(self):
        order = publish_crates.publish_order(meta(pkg("a", dev=["b"]), pkg("b", ["a"])))
        self.assertEqual([n for n, _ in order], ["a", "b"])

    def test_cycle_is_reported(self):
        with self.assertRaises(SystemExit):
            publish_crates.publish_order(meta(pkg("a", ["b"]), pkg("b", ["a"])))

    def test_rate_limit_detection(self):
        self.assertTrue(publish_crates.is_rate_limited("error: 429 Too Many Requests"))
        self.assertFalse(publish_crates.is_rate_limited("error: failed to verify package"))


if __name__ == "__main__":
    unittest.main()
