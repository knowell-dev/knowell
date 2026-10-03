"""Synthetic release target, strict compatibility and malformed manifest tests."""

import json
import shutil
import tempfile
import unittest
from pathlib import Path

import update_assets as ua


class UpdateAssets(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.binary = self.root / "engine"
        self.launcher = self.root / "launcher"
        self.binary.write_bytes(b"KNOWELL_CANARY_ENGINE")
        self.launcher.write_bytes(b"KNOWELL_CANARY_LAUNCHER")
        self.migrations = self.root / "migrations"
        self.migrations.mkdir()
        (self.migrations / "0001_synthetic.sql").write_text("-- synthetic fixture\n")
        self.compatibility = self.root / "compatibility.json"
        self.contract = {"schema": {"read_min": 1, "read_max": 1, "write_min": 1, "write_max": 1}}
        self.contract.update({name: {"min": 1, "max": 1} for name in ua.RANGES})
        self.compatibility.write_text(json.dumps(self.contract))
        self.assets = self.root / "assets"

    def build(self, target="x86_64-unknown-linux-gnu"):
        return ua.build("1.0.0", target, self.binary, self.launcher, self.assets,
                        self.compatibility, self.migrations)

    def assemble(self, destination):
        return ua.assemble("1.0.0", self.assets, self.root / destination,
                           compatibility_file=self.compatibility, migrations=self.migrations)

    def test_public_version_gate_and_strict_semver(self):
        self.assertEqual(ua.check_version("1.0.0"), "stable")
        self.assertEqual(ua.check_version("1.1.0-rc.1"), "preview")
        self.assertEqual(ua.check_version("0.0.0", True), "stable")
        for value in ("0.9.9", "1.0.0-rc.1", "01.0.0", "1.0.0+build", "1.1.0-rc.01", "1.2", "1.2.3/../4", "1.2.3;bad"):
            with self.subTest(value=value), self.assertRaises(ua.UpdateAssetError):
                ua.check_version(value)

    def test_raw_artifacts_and_complete_unsigned_tuf_input(self):
        for target in ua.TARGETS:
            self.build(target)
        output = self.assemble("metadata-input")
        payload = json.loads(output.read_text())
        self.assertEqual(len(payload["targets"]), len(ua.TARGETS) * 2)
        logical = ua.target_name("1.0.0", ua.TARGETS[0], "engine")
        entry = payload["targets"][logical]
        self.assertEqual(entry["length"], self.binary.stat().st_size)
        self.assertEqual(entry["custom"]["knowell"]["compatibility"], self.contract)
        self.assertNotIn("signatures", payload)
        paired = payload["targets"][ua.target_name("1.0.0", ua.TARGETS[0], "launcher")]
        self.assertEqual(paired["custom"], {})
        raw = self.assets / f"{entry['hashes']['sha256']}.{Path(logical).name}"
        self.assertEqual(raw.read_bytes(), self.binary.read_bytes())

    def test_cumulative_input_preserves_revocation_and_refuses_reused_identities(self):
        for target in ua.TARGETS:
            self.build(target)
        previous = self.assemble("previous")
        old = json.loads(previous.read_text())
        engine = ua.target_name("1.0.0", ua.TARGETS[0], "engine")
        old["targets"][engine]["custom"]["knowell"]["revoked"] = True
        previous.write_text(json.dumps(old))
        for target in ua.TARGETS:
            ua.build("1.1.0", target, self.binary, self.launcher, self.assets, self.compatibility, self.migrations)
        cumulative = ua.assemble("1.1.0", self.assets, self.root / "cumulative", compatibility_file=self.compatibility,
                                 migrations=self.migrations, previous_input=previous)
        payload = json.loads(cumulative.read_text())
        self.assertEqual(len(payload["targets"]), len(ua.TARGETS) * 4)
        self.assertEqual(payload["targets"][engine], old["targets"][engine])
        with self.assertRaises(ua.UpdateAssetError):
            ua.assemble("1.0.0", self.assets, self.root / "overwrite", compatibility_file=self.compatibility,
                        migrations=self.migrations, previous_input=previous)

    def test_cumulative_input_rejects_unknown_truncated_and_duplicate_fields(self):
        path = self.root / "previous.json"
        for content in ('{"format_version":', '{"format_version":1,"format_version":1}',
                        '{"format_version":1,"version":"1.0.0","targets":{},"delegations":{}}'):
            path.write_text(content)
            with self.assertRaises(ua.UpdateAssetError):
                ua.assemble("1.0.0", self.assets, self.root / "invalid", compatibility_file=self.compatibility,
                            migrations=self.migrations, previous_input=path)

    def test_missing_platform_and_mutated_binary_are_rejected(self):
        path = self.build()
        with self.assertRaises(ua.UpdateAssetError):
            self.assemble("incomplete")
        manifest = json.loads(path.read_text())
        raw = self.assets / f"{manifest['engine']['sha256']}.{Path(manifest['engine']['name']).name}"
        raw.write_bytes(b"KNOWELL_CANARY_TAMPERED")
        with self.assertRaises(ua.UpdateAssetError):
            self.assemble("tampered")

    def test_schema_contract_must_match_migrations_and_ranges(self):
        for malformed in ({**self.contract, "unknown": {}}, {**self.contract, "jobs": {"min": 2, "max": 1}},
                          {**self.contract, "config": {"min": True, "max": 1}},
                          {**self.contract, "schema": {"read_min": 1, "read_max": 2, "write_min": 2, "write_max": 2}}):
            self.compatibility.write_text(json.dumps(malformed))
            with self.assertRaises(ua.UpdateAssetError):
                ua.compatibility(self.compatibility, self.migrations)
        self.compatibility.write_text(json.dumps(self.contract))
        (self.migrations / "0003_gap.sql").write_text("-- synthetic fixture\n")
        with self.assertRaises(ua.UpdateAssetError):
            ua.compatibility(self.compatibility, self.migrations)

    def test_engine_handshake_must_agree_before_artifact_publication(self):
        target = ua.TARGETS[0]
        info = self.root / "engine-info.json"
        valid = {"format_version": 1, "version": "1.0.0", "target": target, "schema": 1,
                 **{name: 1 for name in ua.RANGES}}
        for malformed in ({**valid, "version": "1.1.0"}, {**valid, "schema": 2},
                          {**valid, "jobs": True}, {**valid, "unknown": 1}):
            info.write_text(json.dumps(malformed))
            with self.assertRaises(ua.UpdateAssetError):
                ua.build("1.0.0", target, self.binary, self.launcher, self.assets,
                         self.compatibility, self.migrations, binary_info=info)
            self.assertFalse(self.assets.exists())
        info.write_text(json.dumps(valid))
        ua.build("1.0.0", target, self.binary, self.launcher, self.assets,
                 self.compatibility, self.migrations, binary_info=info)

    def test_truncated_duplicate_and_hostile_manifest(self):
        for contents in ('{"schema":', '{"schema":1,"schema":2}', '[1,2]', '{"config":"KNOWELL_CANARY_HOSTILE"}'):
            self.compatibility.write_text(contents)
            with self.assertRaises(ua.UpdateAssetError):
                ua.compatibility(self.compatibility, self.migrations)
        self.compatibility.write_text(json.dumps(self.contract))
        path = self.build()
        manifest = json.loads(path.read_text())
        manifest["engine"]["name"] = "../../KNOWELL_CANARY_OUTSIDE"
        path.write_text(json.dumps(manifest))
        with self.assertRaises(ua.UpdateAssetError):
            self.assemble("hostile")

    def test_empty_binary_and_reused_output_are_rejected(self):
        self.binary.write_bytes(b"")
        with self.assertRaises(ua.UpdateAssetError):
            self.build()
        self.binary.write_bytes(b"KNOWELL_CANARY_ENGINE")
        self.build()
        with self.assertRaises(ua.UpdateAssetError):
            self.build()


if __name__ == "__main__":
    unittest.main()
