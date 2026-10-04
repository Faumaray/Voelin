#!/usr/bin/env python3
"""Offline regression tests of the real release script using a curl fixture."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("gitea-release.sh").resolve()
FAKE_CURL = r'''#!/usr/bin/env python3
import json, os, pathlib, stat, sys
args = sys.argv[1:]
def option(name):
    return args[args.index(name) + 1]
header = pathlib.Path(option('--header')[1:])
assert stat.S_IMODE(header.stat().st_mode) == 0o600
assert header.read_text() == 'Authorization: token fixture-secret\n'
assert 'GITEA_RELEASE_TOKEN' not in os.environ
assert not any('fixture-secret' in arg for arg in args)
assert '--location' not in args and '-L' not in args
assert args[0] == '--disable'
method = option('--request')
url = args[args.index('--write-out') + 2]
record = {'method': method, 'url': url, 'header': str(header)}
if '--data-binary' in args:
    record['payload'] = json.loads(pathlib.Path(option('--data-binary')[1:]).read_text())
if '--form' in args:
    record['form'] = option('--form')
with open(os.environ['SPY'], 'a') as spy:
    spy.write(json.dumps(record) + '\n')
mode = os.environ.get('MODE', 'create')
if mode == 'transport-fail':
    sys.stderr.write('curl: (60) TLS certificate failed: fixture-secret\n\x1b' + 'x' * 1000)
    sys.exit(60)
status, body = '200', None
release = {'id': 7, 'tag_name': os.environ['GITHUB_REF'][10:], 'draft': mode != 'published'}
if mode == 'mismatched': release['tag_name'] = 'v-other'
if mode == 'invalid-release-id': release['id'] = True
if method == 'GET' and '/tags/' in url:
    status, body = ('404', {}) if mode == 'create' else ('200', release)
    if mode in ('null-lookup', 'empty-lookup'): body = None
    if mode == 'http-error':
        status, body = '403', {'message': 'Access denied: fixture-secret\n\x1b' + 'y' * 1000}
elif method == 'POST' and url.endswith('/releases'):
    status, body = '201', release
elif method == 'GET' and url.endswith('/assets'):
    body = [{'id': 9, 'name': 'package +#.zip'}, {'id': 10, 'name': 'unrelated.zip'}]
    if mode == 'invalid-id': body[0]['id'] = '../bad'
elif method == 'DELETE':
    status = '500' if mode == 'delete-fail' else '204'
elif method == 'POST' and '/assets?' in url:
    status, body = ('500', {}) if mode == 'upload-fail' else ('201', {'id': 11})
else:
    raise AssertionError(record)
pathlib.Path(option('--output')).write_text(json.dumps(body) if body is not None or mode == 'null-lookup' else '')
sys.stdout.write(status)
'''


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.package = self.assets / "package +#.zip"
        self.package.write_bytes(b"package")
        self.checksums()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        curl = self.bin / "curl"
        curl.write_text(FAKE_CURL)
        curl.chmod(0o755)
        self.spy = self.root / "spy"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        SPY=str(self.spy), MODE="create", GITEA_RELEASE_TOKEN="fixture-secret",
                        GITHUB_SERVER_URL="https://git.faumaray.ru", GITHUB_REPOSITORY="owner/repo",
                        GITHUB_REF="refs/tags/v1.0+test", GITHUB_SHA="a" * 40)

    def checksums(self):
        (self.assets / "SHA256SUMS").write_text(
            hashlib.sha256(self.package.read_bytes()).hexdigest() + "  " + self.package.name + "\n")

    def run_script(self, success=True, pre_api=False):
        result = subprocess.run(["bash", "-x", str(SCRIPT), str(self.assets)],
                                env=self.env, text=True, capture_output=True)
        self.last_result = result
        self.assertEqual(result.returncode == 0, success, result.stderr)
        self.assertNotIn("fixture-secret", result.stdout + result.stderr)
        calls = [json.loads(line) for line in self.spy.read_text().splitlines()] if self.spy.exists() else []
        if pre_api:
            self.assertEqual(calls, [])
        for call in calls:
            self.assertFalse(Path(call['header']).exists(), "credential file leaked")
            self.assertNotEqual(call['method'], 'PATCH', "must never publish")
        return calls

    def test_create_draft_and_encoded_upload(self):
        calls = self.run_script()
        create = next(c for c in calls if 'payload' in c)
        self.assertTrue(create['payload']['draft'])
        self.assertEqual(create['payload']['target_commitish'], 'a' * 40)
        self.assertTrue(calls[0]['url'].endswith('/tags/v1.0%2Btest'))
        self.assertTrue(any(c['url'].endswith('?name=package%20%2B%23.zip') for c in calls))
        self.assertEqual(len([c for c in calls if 'form' in c]), 2)

    def test_invalid_successful_lookup_never_creates_release(self):
        for mode in ('null-lookup', 'empty-lookup'):
            with self.subTest(mode=mode):
                self.spy.unlink(missing_ok=True)
                self.env['MODE'] = mode
                calls = self.run_script(False)
                self.assertEqual([c['method'] for c in calls], ['GET'])

    def test_transport_error_is_bounded_and_redacted(self):
        self.env['MODE'] = 'transport-fail'
        calls = self.run_script(False)
        self.assertEqual([c['method'] for c in calls], ['GET'])
        stderr = self.last_result.stderr
        self.assertIn('GET transport failed (curl 60)', stderr)
        self.assertIn('TLS certificate failed: [REDACTED]', stderr)
        self.assertNotIn('\x1b', stderr)
        self.assertLess(len(stderr), 650)

    def test_http_error_is_bounded_and_redacted(self):
        self.env['MODE'] = 'http-error'
        calls = self.run_script(False)
        self.assertEqual([c['method'] for c in calls], ['GET'])
        stderr = self.last_result.stderr
        self.assertIn('GET failed (HTTP 403)', stderr)
        self.assertIn('Access denied: [REDACTED]', stderr)
        self.assertNotIn('\x1b', stderr)
        self.assertLess(len(stderr), 650)

    def test_retry_replaces_only_matching_assets(self):
        self.env['MODE'] = 'draft'
        calls = self.run_script()
        self.assertFalse(any('payload' in c for c in calls))
        self.assertEqual([c['url'].split('/')[-1] for c in calls if c['method'] == 'DELETE'], ['9'])

    def test_published_release_is_untouched(self):
        self.env['MODE'] = 'published'
        calls = self.run_script(False)
        self.assertEqual([c['method'] for c in calls], ['GET'])

    def test_mismatched_release_is_untouched(self):
        self.env['MODE'] = 'mismatched'
        calls = self.run_script(False)
        self.assertEqual([c['method'] for c in calls], ['GET'])

    def test_invalid_release_id_is_untouched(self):
        self.env['MODE'] = 'invalid-release-id'
        calls = self.run_script(False)
        self.assertEqual([c['method'] for c in calls], ['GET'])

    def test_form_path_and_url_encoding(self):
        renamed = self.package.with_name('quote";,+#.zip')
        self.package.rename(renamed)
        self.package = renamed
        self.checksums()
        self.env['GITHUB_REF'] = 'refs/tags/v1/preview'
        calls = self.run_script()
        self.assertTrue(calls[0]['url'].endswith('/tags/v1%2Fpreview'))
        upload = next(c for c in calls if c['url'].endswith('?name=quote%22%3B%2C%2B%23.zip'))
        self.assertIn('quote\\";,+#.zip', upload['form'])
        self.assertTrue(upload['form'].endswith('"'))

    def test_invalid_environment_before_api(self):
        for key, value in [('GITEA_RELEASE_TOKEN', ''), ('GITHUB_REF', 'refs/heads/main'),
                           ('GITHUB_SERVER_URL', 'http://git.faumaray.ru'),
                           ('GITHUB_SERVER_URL', 'https://user:pass@git.faumaray.ru'),
                           ('GITHUB_SERVER_URL', 'https://git.faumaray.ru?query'),
                           ('GITHUB_REPOSITORY', '../repo'), ('GITHUB_SHA', 'main')]:
            with self.subTest(key=key, value=value):
                old = self.env[key]
                self.env[key] = value
                self.run_script(False, pre_api=True)
                self.env[key] = old

    def test_checksum_failure_before_api(self):
        self.package.write_bytes(b'corrupted')
        self.run_script(False, pre_api=True)

    def test_stale_file_before_api(self):
        (self.assets / 'stale.zip').write_bytes(b'stale')
        self.run_script(False, pre_api=True)

    def test_directory_and_symlink_before_api(self):
        extra = self.assets / 'extra'
        extra.mkdir()
        self.run_script(False, pre_api=True)
        extra.rmdir()
        extra.symlink_to(self.package)
        self.run_script(False, pre_api=True)

    def test_missing_manifest_before_api(self):
        (self.assets / 'SHA256SUMS').unlink()
        self.run_script(False, pre_api=True)

    def test_upload_failure_stays_draft_and_cleans_token(self):
        self.env['MODE'] = 'upload-fail'
        calls = self.run_script(False)
        self.assertEqual(len([c for c in calls if 'form' in c]), 1)

    def test_delete_failure_stops_before_replacement(self):
        self.env['MODE'] = 'delete-fail'
        calls = self.run_script(False)
        self.assertEqual(calls[-1]['method'], 'DELETE')
        self.assertFalse(any('package%20' in c['url'] for c in calls))

    def test_invalid_api_id_stops_before_mutation(self):
        self.env['MODE'] = 'invalid-id'
        calls = self.run_script(False)
        self.assertTrue(all(c['method'] == 'GET' for c in calls))


if __name__ == '__main__':
    unittest.main()
