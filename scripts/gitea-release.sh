#!/usr/bin/env bash
# Upload verified packages to a draft only; publication is always manual.
set +x
set -euo pipefail
if (( $# > 1 )); then
    echo "usage: $0 [asset-directory]" >&2
    exit 1
fi
release_tmp=$(mktemp -d /tmp/gitea-release.XXXXXXXX)
trap 'rm -rf -- "$release_tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
python3 - "${1:-dist/release}" "$release_tmp" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from urllib.parse import quote, urlsplit


def fail(message):
    raise ValueError(message)


def positive_id(value):
    if type(value) is not int or value <= 0:
        fail("API returned an invalid id")
    return value


def main():
    token = os.environ.get("GITEA_RELEASE_TOKEN", "")
    if not token or any(ord(c) < 33 or ord(c) > 126 for c in token):
        fail("GITEA_RELEASE_TOKEN is missing or invalid")
    server = os.environ.get("GITHUB_SERVER_URL", "")
    parsed = urlsplit(server)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username
            or parsed.password or parsed.query or parsed.fragment
            or any(c.isspace() for c in server)):
        fail("GITHUB_SERVER_URL must be an HTTPS server URL without credentials or query")
    repo = os.environ.get("GITHUB_REPOSITORY", "").split("/")
    if len(repo) != 2 or any(not part or part in (".", "..") for part in repo):
        fail("GITHUB_REPOSITORY must be owner/repo")
    ref = os.environ.get("GITHUB_REF", "")
    if not ref.startswith("refs/tags/v") or len(ref) <= len("refs/tags/v"):
        fail("GITHUB_REF must be a v* tag")
    tag = ref[len("refs/tags/"):]
    sha = os.environ.get("GITHUB_SHA", "")
    if not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", sha):
        fail("GITHUB_SHA must be a full commit id")
    directory = Path(sys.argv[1])
    if directory.is_symlink() or not directory.is_dir():
        fail("asset directory must be a non-symlink directory")
    files = sorted(directory.iterdir())
    if any(p.is_symlink() or not p.is_file() for p in files):
        fail("asset directory must contain only regular non-symlink files")
    names = {p.name for p in files}
    if "SHA256SUMS" not in names or len(names) < 2:
        fail("SHA256SUMS and at least one package are required")
    # Exclude control characters and sha256sum's escaped filename syntax.
    if any(any(ord(c) < 32 or c == "\\" for c in name) for name in names):
        fail("unsupported control character or backslash in asset name")
    expected = {}
    for line in (directory / "SHA256SUMS").read_text().splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line)
        if not match or match[2] in expected:
            fail("invalid or duplicate SHA256SUMS entry")
        expected[match[2]] = match[1].lower()
    if set(expected) != names - {"SHA256SUMS"}:
        fail("SHA256SUMS must describe exactly all package files")
    for name, digest in expected.items():
        with (directory / name).open("rb") as package:
            actual = hashlib.file_digest(package, "sha256").hexdigest()
        if actual != digest:
            fail("package checksum mismatch")

    base = server.rstrip("/") + "/api/v1/repos/" + "/".join(quote(p, safe="") for p in repo) + "/releases"
    # /tmp is outside the checkout/build context; mkdtemp and the header are private.
    with tempfile.TemporaryDirectory(prefix="gitea-release-", dir=sys.argv[2]) as temp:
        header = Path(temp) / "headers"
        fd = os.open(header, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output:
            output.write("Authorization: token " + token + "\n")
        environment = os.environ.copy()
        environment.pop("GITEA_RELEASE_TOKEN", None)

        not_found = object()

        def diagnostic(text):
            # Redact before truncation and again after stripping control characters.
            text = text.replace(token, "[REDACTED]")
            text = "".join(c for c in text if c.isprintable())
            return text.replace(token, "[REDACTED]")[:512]

        def request(method, url, *, payload=None, package=None, missing=False):
            response = Path(temp) / "response"
            args = ["curl", "--disable", "--silent", "--show-error", "--proto", "=https",
                    "--connect-timeout", "30", "--max-time", "1800",
                    "--request", method, "--header", "@" + str(header),
                    "--output", str(response), "--write-out", "%{http_code}", url]
            if payload is not None:
                body = Path(temp) / "request.json"
                body.write_text(json.dumps(payload))
                args += ["--header", "Content-Type: application/json", "--data-binary", "@" + str(body)]
            if package is not None:
                # Quoted curl form path protects commas/semicolons/quotes in paths.
                path = str(package.resolve()).replace("\\", "\\\\").replace('"', '\\"')
                args += ["--form", 'attachment=@"' + path + '"']
            result = subprocess.run(args, capture_output=True, text=True, env=environment)
            if result.returncode:
                fail(f"Gitea API {method} transport failed (curl {result.returncode}): "
                     + diagnostic(result.stderr))
            if missing and result.stdout == "404":
                return not_found
            if not re.fullmatch(r"2[0-9]{2}", result.stdout):
                message = ""
                try:
                    with response.open() as output:
                        error = json.loads(output.read(8192))
                    if isinstance(error, dict) and isinstance(error.get("message"), str):
                        message = ": " + diagnostic(error["message"])
                except (OSError, ValueError):
                    pass
                fail(f"Gitea API {method} failed (HTTP {diagnostic(result.stdout)})" + message)
            return json.loads(response.read_text()) if response.stat().st_size else None

        release = request("GET", base + "/tags/" + quote(tag, safe=""), missing=True)
        if release is not_found:
            release = request("POST", base, payload={"tag_name": tag, "target_commitish": sha,
                "name": tag, "draft": True, "prerelease": "-" in tag,
                "body": "Packages and SHA256 checksums. Review before publishing."})
        if not isinstance(release, dict) or release.get("tag_name") != tag or release.get("draft") is not True:
            fail("refusing to modify a published or mismatched release")
        assets_url = base + "/" + str(positive_id(release.get("id"))) + "/assets"
        assets = request("GET", assets_url)
        if not isinstance(assets, list):
            fail("API returned an invalid asset list")
        for asset in assets:
            if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
                fail("API returned an invalid asset")
            positive_id(asset.get("id"))
        for package in files:
            for asset in assets:
                if asset["name"] == package.name:
                    request("DELETE", assets_url + "/" + str(asset["id"]))
            uploaded = request("POST", assets_url + "?name=" + quote(package.name, safe=""), package=package)
            if not isinstance(uploaded, dict):
                fail("API returned an invalid uploaded asset")
            positive_id(uploaded.get("id"))
    print("Packages uploaded to draft release " + tag + "; publication remains manual.")


try:
    main()
except (ValueError, OSError, KeyError, TypeError) as error:
    print("gitea-release: " + str(error), file=sys.stderr)
    sys.exit(1)
PY
