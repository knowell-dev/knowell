#!/usr/bin/env python3
"""Install native release components in an isolated root and run the stable launcher."""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from update_assets import check_version


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--assets", required=True, type=Path)
    args = parser.parse_args()
    check_version(args.version)
    windows = args.target.endswith("windows-msvc")
    executable = "know.exe" if windows else "know"
    extension = ".exe" if windows else ""
    repository = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="knowell-bootstrap-smoke-") as tmp:
        root = Path(tmp)
        release = root / "downloads" / f"v{args.version}"
        release.mkdir(parents=True)
        sums = []
        for component in ("engine", "launcher"):
            suffix = f".knowell-{args.version}-{args.target}-{component}{extension}"
            matches = [p for p in args.assets.iterdir() if p.name.endswith(suffix)]
            if len(matches) != 1:
                raise ValueError("smoke test requires exactly one raw artifact per component")
            source = matches[0]
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
            if source.name != digest + suffix:
                raise ValueError("smoke target digest does not match its immutable name")
            shutil.copyfile(source, release / source.name)
            sums.append(f"{digest}  {source.name}\n")
        (release / "SHA256SUMS").write_text("".join(sums), encoding="utf-8", newline="\n")
        # Fixture processes have isolated state and do not inherit provider credentials.
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "WINDIR", "TMP", "TEMP") if key in os.environ}
        env.update(HOME=str(root / "home"), LOCALAPPDATA=str(root / "local"),
                   KNOWELL_HOME=str(root / "data"), KNOWELL_DOWNLOAD_BASE=(root / "downloads").as_uri(),
                   KNOWELL_NO_PATH="1")
        destination = root / "install"
        if windows:
            # This isolated environment intentionally omits inherited variables.
            # Supply the declared native runner architecture for installer selection.
            env["PROCESSOR_ARCHITECTURE"] = "ARM64" if args.target.startswith("aarch64-") else "AMD64"
            shell = shutil.which("pwsh") or shutil.which("powershell")
            if not shell:
                raise ValueError("native Windows bootstrap smoke test requires PowerShell")
            command = [shell, "-NoProfile", "-File", str(repository / "scripts" / "install.ps1"),
                       "-Version", args.version, "-InstallDir", str(destination), "-Attestation", "skip"]
        else:
            command = ["sh", str(repository / "scripts" / "install.sh"), "--version", args.version,
                       "--install-dir", str(destination), "--attestation", "skip"]
        subprocess.run(command, env=env, check=True, timeout=330)
        result = subprocess.run([str(destination / executable), "--version"], env=env, check=True,
                                capture_output=True, text=True, timeout=30)
        if args.version not in result.stdout:
            raise ValueError("installed launcher dispatched the wrong engine version")
        print("native bootstrap, launcher receipt and pinned engine dispatch verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
