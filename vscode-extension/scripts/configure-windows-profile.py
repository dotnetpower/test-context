#!/usr/bin/env python3
"""Configure the Windows profile associated with the current WSL distribution."""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import tempfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--user-data", type=Path, required=True)
    parser.add_argument("--distribution", required=True)
    parser.add_argument("--executable", required=True)
    args = parser.parse_args()

    database = args.user_data / "globalStorage" / "state.vscdb"
    with sqlite3.connect(database) as connection:
        row = connection.execute(
            "SELECT value FROM ItemTable WHERE key = 'associatedRemoteProfiles'"
        ).fetchone()
    if row is None:
        raise RuntimeError("VS Code has no associatedRemoteProfiles state")

    associations = json.loads(row[0])
    profile_ids = associations.get(f"wsl+{args.distribution}", [])
    if len(profile_ids) != 1:
        raise RuntimeError(
            f"expected one VS Code profile for WSL {args.distribution}, got {profile_ids}"
        )

    configured = []
    base_settings = args.user_data / "settings.json"
    configure_settings(base_settings, args.distribution, args.executable)
    configured.append(str(base_settings))

    settings_path = args.user_data / "profiles" / profile_ids[0] / "settings.json"
    configure_settings(settings_path, args.distribution, args.executable)
    configured.append(str(settings_path))

    print(
        f"configured Windows VS Code base and profile {profile_ids[0]}: "
        + ", ".join(configured)
    )
    return 0


def configure_settings(
    settings_path: Path,
    distribution: str,
    executable: str,
) -> None:
    settings = json.loads(settings_path.read_text(encoding="utf-8"))
    extension_kinds = settings.setdefault("remote.extensionKind", {})
    if not isinstance(extension_kinds, dict):
        raise RuntimeError("remote.extensionKind must be a JSON object")
    extension_kinds["moonchoi.test-context-native"] = ["ui"]
    settings["testContextNative.wslDistribution"] = distribution
    settings["testContextNative.wslExecutablePath"] = executable

    fd, temporary_name = tempfile.mkstemp(
        prefix=f"{settings_path.name}.",
        dir=settings_path.parent,
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temporary:
            json.dump(settings, temporary, indent=4, ensure_ascii=False)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, settings_path)
    except BaseException:
        Path(temporary_name).unlink(missing_ok=True)
        raise


if __name__ == "__main__":
    raise SystemExit(main())
