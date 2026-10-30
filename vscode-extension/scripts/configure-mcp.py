#!/usr/bin/env python3
"""Reconcile installer-owned MCP entries without changing other servers."""

from __future__ import annotations

import argparse
import json
import os
import tempfile
from pathlib import Path


def configure_mcp(
    path: Path, executable: str, distribution: str | None, enable: bool
) -> bool:
    source = path.read_bytes() if path.exists() else None
    try:
        document = json.loads(source.decode("utf-8-sig")) if source is not None else {}
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{path}: expected JSON; merge JSONC manually") from error
    if not isinstance(document, dict):
        raise RuntimeError(f"{path}: MCP configuration must be an object")
    servers = document.get("servers", {})
    if not isinstance(servers, dict):
        raise RuntimeError(f"{path}: servers must be an object")

    desired = {"type": "stdio", "command": executable, "args": ["shared-mcp"]}
    if distribution:
        desired = {
            "type": "stdio",
            "command": "wsl.exe",
            "args": ["--distribution", distribution, "--exec", executable, "shared-mcp"],
        }
    current = servers.get("test-context")
    managed = isinstance(current, dict) and (
        (current.get("command") == executable and current.get("args") == ["shared-mcp"])
        or (
            distribution is not None
            and current.get("command") == desired["command"]
            and current.get("args") == desired["args"]
        )
    )
    if enable:
        if current is not None and not managed:
            raise RuntimeError(f"{path}: refusing to replace a custom test-context server")
        if current == desired:
            return False
        servers["test-context"] = desired
        document["servers"] = servers
    elif managed:
        del servers["test-context"]
    else:
        return False

    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f"{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temporary:
            json.dump(document, temporary, indent=2, ensure_ascii=False)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        live_source = path.read_bytes() if path.exists() else None
        if live_source != source:
            raise RuntimeError(f"{path}: configuration changed concurrently; retry")
        os.replace(temporary_name, path)
    except BaseException:
        Path(temporary_name).unlink(missing_ok=True)
        raise
    return True


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", type=Path, required=True)
    parser.add_argument("--executable", required=True)
    parser.add_argument("--distribution")
    parser.add_argument("--enable", action="store_true")
    args = parser.parse_args()
    changed = configure_mcp(args.file, args.executable, args.distribution, args.enable)
    print(f"MCP configuration {'updated' if changed else 'unchanged'}: {args.file}")


if __name__ == "__main__":
    main()
