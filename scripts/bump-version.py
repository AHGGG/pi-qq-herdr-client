"""Bump the release version without changing dependency versions."""

import argparse
import os
from pathlib import Path
import re
import tomllib


def replace_once(text: str, pattern: str, replacement: str) -> str:
    updated, count = re.subn(pattern, lambda _: replacement, text, flags=re.MULTILINE)
    if count != 1:
        raise ValueError(f"Expected one version field matching {pattern!r}, found {count}.")
    return updated


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bump", choices=("patch", "minor", "major"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    manifest_path = root / "Cargo.toml"
    lock_path = root / "Cargo.lock"
    resource_path = root / "crates/desktop/assets/app.rc"
    manifest = manifest_path.read_text(encoding="utf-8")
    current = tomllib.loads(manifest)["workspace"]["package"]["version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", current):
        raise ValueError(f"Expected a stable major.minor.patch version, found {current!r}.")
    parts = [int(part) for part in current.split(".")]
    index = {"major": 0, "minor": 1, "patch": 2}[args.bump]
    parts[index] += 1
    parts[index + 1 :] = [0] * (2 - index)
    if any(part > 65535 for part in parts):
        raise ValueError("Windows version components must be at most 65535.")
    version = ".".join(str(part) for part in parts)
    tag = f"v{version}"
    section = re.search(r"(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", manifest)
    if section is None:
        raise ValueError("Missing [workspace.package] section.")
    package = replace_once(
        section[1], rf'^version\s*=\s*"{re.escape(current)}"', f'version = "{version}"'
    )
    manifest = manifest[: section.start(1)] + package + manifest[section.end(1) :]

    lock = lock_path.read_text(encoding="utf-8")
    packages = tomllib.loads(lock)["package"]
    for name in ("pi-qq-client-core", "pi-qq-herdr-client"):
        matches = [p for p in packages if p["name"] == name and "source" not in p]
        if len(matches) != 1 or matches[0]["version"] != current:
            raise ValueError(f"Cargo.lock version for {name} does not match {current}.")
        lock = replace_once(
            lock,
            rf'^name = "{name}"\nversion = "{re.escape(current)}"',
            f'name = "{name}"\nversion = "{version}"',
        )

    resource = resource_path.read_text(encoding="utf-8")
    for field in ("FILEVERSION", "PRODUCTVERSION"):
        resource = replace_once(
            resource,
            rf"^{field} {re.escape(current.replace('.', ','))},0$",
            f"{field} {','.join(str(part) for part in parts)},0",
        )
    for field in ("FileVersion", "ProductVersion"):
        resource = replace_once(
            resource,
            rf'^[ \t]*VALUE "{field}", "{re.escape(current)}\\0"$',
            f'            VALUE "{field}", "{version}\\0"',
        )

    # Validate every input before writing any file.
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    lock_path.write_text(lock, encoding="utf-8", newline="\n")
    resource_path.write_text(resource, encoding="utf-8", newline="\n")
    print(f"Version: {current} -> {version} ({tag})")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            stream.write(f"version={version}\ntag={tag}\n")


if __name__ == "__main__":
    main()
