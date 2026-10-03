"""Offline regression tests for the real SARIF ingestion verifier."""

from copy import deepcopy
import contextlib
from http.client import IncompleteRead
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlsplit

import verify_sarif_ingestion as verifier


SARIF_ID = "aaaaaaaa-2222-3333-4444-555555555555"
SHA = "a" * 40
FAKE_TOKEN = "KNOWELL_CANARY_FAKE_TOKEN"
EXPECTED = {
    "rule_id": "link.endpoint_without_provider",
    "path": "packs/js-http-client/tests/pos-axios.ts",
    "start_line": 5, "end_line": 5,
}
VALIDATION = {
    "tool": "knowell", "category": verifier.CATEGORY, "commit_sha": SHA,
    "results_count": 11, "expected_result": EXPECTED,
}
ANALYSIS = {
    "id": 42, "sarif_id": SARIF_ID, "commit_sha": SHA, "ref": verifier.REF, "category": verifier.CATEGORY,
    "tool": {"name": "knowell"}, "results_count": 11, "error": "",
    "analysis_key": verifier.ANALYSIS_KEY, "environment": verifier.ENVIRONMENT,
}
COMPLETE = {
    "processing_status": "complete",
    "analyses_url": f"{verifier.API_ROOT}/analyses?sarif_id={SARIF_ID}",
}
ALERT = {
    "number": 7, "state": "open", "tool": {"name": "knowell"},
    "rule": {"id": EXPECTED["rule_id"]},
    "most_recent_instance": {
        "state": "open",
        **{key: ANALYSIS[key] for key in (
            "commit_sha", "ref", "category", "analysis_key", "environment",
        )},
        "location": {
            "path": EXPECTED["path"], "start_line": 5, "end_line": 5,
        },
    },
}


class FakeApi:
    def __init__(self, statuses=None, analyses=None, pages=None):
        self.statuses = deepcopy(statuses if statuses is not None else [COMPLETE])
        self.analyses = deepcopy(analyses if analyses is not None else [ANALYSIS])
        self.pages = deepcopy(pages if pages is not None else {1: [ALERT]})
        self.calls = []

    def __call__(self, url):
        self.calls.append(url)
        if "/sarifs/" in url:
            return self.statuses.pop(0) if len(self.statuses) > 1 else self.statuses[0]
        if "/analyses?" in url:
            return self.analyses
        if "/alerts?" in url:
            page = int(parse_qs(urlsplit(url).query)["page"][0])
            return self.pages.get(page, [])
        raise AssertionError("unexpected synthetic API resource")


class Response:
    def __init__(self, body, url, status=200):
        self.body = body
        self.url = url
        self.status = status
        self.read_sizes = []

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def geturl(self):
        return self.url

    def read(self, size):
        self.read_sizes.append(size)
        return self.body[:size]


class VerifierTests(unittest.TestCase):
    def run_verifier(self, api=None, validation=None, sarif_id=SARIF_ID, sha=SHA):
        self.evidence = []
        self.sleeps = []
        return verifier.verify(
            sarif_id, sha, deepcopy(validation if validation is not None else VALIDATION),
            api if api is not None else FakeApi(), sleep=self.sleeps.append,
            record=lambda value: self.evidence.append(deepcopy(value)),
        )

    def assert_failure(self, api=None, **kwargs):
        with self.assertRaises(verifier.VerificationError) as caught:
            self.run_verifier(api, **kwargs)
        self.assertEqual(self.evidence[-1]["verification_status"], "failed")
        self.assertNotIn(FAKE_TOKEN, str(caught.exception))
        self.assertNotIn(FAKE_TOKEN, json.dumps(self.evidence))
        return caught.exception

    def test_success_records_only_verified_metadata(self):
        api = FakeApi()
        api.analyses[0]["untrusted"] = FAKE_TOKEN
        api.statuses[0]["untrusted"] = FAKE_TOKEN
        api.pages[1][0]["untrusted"] = FAKE_TOKEN
        evidence = self.run_verifier(api)
        self.assertEqual(evidence["verification_status"], "complete")
        self.assertEqual(evidence["analysis_id"], 42)
        self.assertEqual(evidence["alert_number"], 7)
        self.assertEqual(evidence["expected_result"], EXPECTED)
        self.assertEqual(evidence["results_count"], 11)
        self.assertEqual(self.sleeps, [])
        self.assertNotIn(FAKE_TOKEN, json.dumps(self.evidence))
        self.assertEqual(len(api.calls), 3)

    def test_pending_then_complete(self):
        evidence = self.run_verifier(FakeApi(statuses=[{"processing_status": "pending"}, COMPLETE]))
        self.assertEqual(evidence["poll_attempts"], 2)
        self.assertEqual(self.sleeps, [5])

    def test_processing_failures_and_unknown_or_missing_status(self):
        for status in ["failed", "unexpected", None, [], True]:
            with self.subTest(status=status):
                self.assert_failure(FakeApi(statuses=[{"processing_status": status}]))
                self.assertEqual(self.sleeps, [])
        for response in [[], None, "unexpected"]:
            with self.subTest(response=response):
                api = FakeApi()
                api.statuses = [response]
                self.assert_failure(api)

    def test_processing_polling_exhaustion(self):
        api = FakeApi(statuses=[{"processing_status": "pending"}])
        self.assert_failure(api)
        self.assertEqual(len(api.calls), 30)
        self.assertEqual(self.sleeps, [5] * 29)
        self.assertEqual(self.evidence[-1]["poll_attempts"], 30)
        self.assertEqual(self.evidence[-1]["processing_status"], "pending")

    def test_unexpected_analysis_urls_are_never_followed(self):
        for url in [None, "https://example.invalid/" + FAKE_TOKEN,
                    COMPLETE["analyses_url"].replace("knowell-dev/knowell", "other/repo"),
                    COMPLETE["analyses_url"] + "&page=1"]:
            with self.subTest(url=url):
                status = {**COMPLETE, "analyses_url": url}
                api = FakeApi(statuses=[status])
                self.assert_failure(api)
                self.assertEqual(len(api.calls), 1)

    def test_invalid_ids_and_shas_do_not_reach_the_api_or_evidence(self):
        for sarif_id in [None, "", "../" + FAKE_TOKEN, FAKE_TOKEN, SARIF_ID.upper(), SARIF_ID + "?x=1"]:
            with self.subTest(sarif_id=sarif_id):
                api = FakeApi()
                self.assert_failure(api, sarif_id=sarif_id)
                self.assertEqual(api.calls, [])
                self.assertNotIn("sarif_id", self.evidence[-1])
        for sha in [None, "", "A" * 40, "a" * 39, "a" * 64, FAKE_TOKEN]:
            with self.subTest(sha=sha):
                api = FakeApi()
                self.assert_failure(api, sha=sha)
                self.assertEqual(api.calls, [])
                self.assertNotIn("commit_sha", self.evidence[-1])

    def test_validation_identity_and_expected_result_validation(self):
        changes = [
            ("tool", "other"), ("category", "other"), ("commit_sha", "b" * 40),
            ("results_count", 0), ("results_count", True), ("results_count", 11.0),
            ("expected_result", None),
        ]
        for key, value in changes:
            with self.subTest(key=key, value=value):
                validation = {**VALIDATION, key: value}
                self.assert_failure(validation=validation)
        for key, value in [
            ("rule_id", ""), ("rule_id", FAKE_TOKEN),
            ("path", ""), ("path", "/absolute.ts"), ("path", "../outside.ts"),
            ("path", "src/../outside.ts"), ("path", "C:\\private.ts"),
            ("path", "https://example.invalid/"), ("path", "src//file.ts"),
            ("path", "src/\x00.ts"), ("start_line", 0), ("start_line", True),
            ("end_line", 4), ("end_line", 5.0),
        ]:
            with self.subTest(key=key, value=value):
                validation = {**VALIDATION, "expected_result": {**EXPECTED, key: value}}
                self.assert_failure(validation=validation)

    def test_analysis_identity_count_and_error_mismatches_are_fatal(self):
        changes = [
            ("id", True), ("id", 0), ("commit_sha", "b" * 40),
            ("sarif_id", None), ("sarif_id", "bbbbbbbb-2222-3333-4444-555555555555"),
            ("ref", "refs/heads/other"), ("category", "other"),
            ("tool", {"name": "other"}), ("tool", None),
            ("results_count", 10), ("results_count", True), ("results_count", 11.0),
            ("error", FAKE_TOKEN), ("error", None),
            ("analysis_key", "other"), ("environment", '{"other":true}'),
        ]
        for key, value in changes:
            with self.subTest(key=key, value=value):
                api = FakeApi(analyses=[{**ANALYSIS, key: value}])
                self.assert_failure(api)
                self.assertEqual(len(api.calls), 2)
        for value in [[], [ANALYSIS, ANALYSIS], {}, [None]]:
            with self.subTest(value=value):
                self.assert_failure(FakeApi(analyses=value))

    def test_wrong_alert_source_or_identity_never_certifies_placement(self):
        changes = [
            ("commit_sha", "b" * 40), ("ref", "refs/heads/other"),
            ("state", "fixed"), ("state", "dismissed"), ("state", None),
            ("category", "other"), ("analysis_key", "other"), ("environment", "other"),
        ]
        for key, value in changes:
            with self.subTest(key=key):
                alert = deepcopy(ALERT)
                alert["most_recent_instance"][key] = value
                self.assert_failure(FakeApi(pages={1: [alert]}))
                self.assertEqual(self.sleeps, [2] * 9)
        for key, value in [
            ("path", "pos-axios.ts"), ("path", "other/pos-axios.ts"),
            ("start_line", 4), ("end_line", 6), ("start_line", 5.0),
            ("end_line", True),
        ]:
            with self.subTest(key=key, value=value):
                alert = deepcopy(ALERT)
                alert["most_recent_instance"]["location"][key] = value
                self.assert_failure(FakeApi(pages={1: [alert]}))
        alert = deepcopy(ALERT)
        alert["rule"]["id"] = "link.other"
        self.assert_failure(FakeApi(pages={1: [alert]}))
        for key, value in [("state", "fixed"), ("state", "dismissed"),
                           ("tool", {"name": "other"}), ("tool", None)]:
            with self.subTest(key=key, value=value):
                alert = {**deepcopy(ALERT), key: value}
                self.assert_failure(FakeApi(pages={1: [alert]}))

    def test_alert_eventual_consistency(self):
        api = FakeApi()
        polls = 0

        def fetch(url):
            nonlocal polls
            if "/alerts?" in url:
                polls += 1
                return [] if polls == 1 else [ALERT]
            return api(url)

        evidence = self.run_verifier(fetch)
        self.assertEqual(evidence["alert_poll_attempts"], 2)
        self.assertEqual(evidence["alert_pages_fetched"], 2)
        self.assertEqual(self.sleeps, [2])

    def test_alert_pagination_is_numeric_and_bounded(self):
        older = deepcopy(ALERT)
        older["most_recent_instance"]["commit_sha"] = "b" * 40
        api = FakeApi(pages={1: [older] * 100, 2: [ALERT]})
        evidence = self.run_verifier(api)
        self.assertEqual(evidence["alert_pages_fetched"], 2)
        self.assertTrue(api.calls[-1].endswith("&page=2"))
        self.assertEqual(self.sleeps, [])
        api = FakeApi(pages={page: [older] * 100 for page in range(1, 6)})
        self.assert_failure(api)
        self.assertEqual(len([url for url in api.calls if "/alerts?" in url]), 5)
        self.assertEqual(self.sleeps, [])

    def test_alert_empty_polling_exhaustion(self):
        api = FakeApi(pages={})
        self.assert_failure(api)
        self.assertEqual(len(api.calls), 12)
        self.assertEqual(self.evidence[-1]["alert_poll_attempts"], 10)
        self.assertEqual(self.sleeps, [2] * 9)

    def test_malformed_alert_pages_or_items_are_fatal(self):
        for page in [{}, [ALERT] * 101, [None], [{}],
                     [{**ALERT, "most_recent_instance": None}],
                     [{**ALERT, "rule": None}],
                     [{**ALERT, "most_recent_instance": {"location": None}}],
                     [{**ALERT, "number": True}]]:
            with self.subTest(page=page):
                self.assert_failure(FakeApi(pages={1: page}))
                self.assertEqual(self.sleeps, [])

    def test_fetch_failure_does_not_echo_exception_contents(self):
        def fetch(url):
            raise RuntimeError(FAKE_TOKEN)

        self.assert_failure(fetch)

    def test_api_failures_at_each_stage_record_only_safe_status(self):
        for stage in ["/sarifs/", "/analyses?", "/alerts?"]:
            with self.subTest(stage=stage):
                api = FakeApi()

                def fetch(url):
                    if stage in url:
                        raise verifier.VerificationError("the GitHub API lookup failed", 429)
                    return api(url)

                failure = self.assert_failure(fetch)
                self.assertEqual(failure.http_status, 429)
                self.assertEqual(self.evidence[-1]["http_status"], 429)
                self.assertEqual(self.sleeps, [])


class TransportTests(unittest.TestCase):
    URL = f"{verifier.API_ROOT}/sarifs/{SARIF_ID}"

    def call_transport(self, body=None, error=None, url=None, status=200):
        response = Response(body if body is not None else json.dumps(COMPLETE).encode(),
                            url if url is not None else self.URL, status)

        class Opener:
            def open(inner, request, timeout):
                self.assertEqual(request.full_url, self.URL)
                self.assertEqual(request.get_header("Authorization"), "Bearer " + FAKE_TOKEN)
                self.assertEqual(timeout, 5)
                if error:
                    raise error
                return response

        def opener(handler):
            self.assertIsInstance(handler, verifier.NoRedirect)
            self.assertIsNone(handler.redirect_request(None, None, 302, "synthetic", {},
                                                        "https://example.invalid/"))
            return Opener()

        with patch.object(verifier, "build_opener", side_effect=opener):
            result = verifier.fetch_json(self.URL, FAKE_TOKEN)
        self.assertEqual(response.read_sizes, [verifier.MAX_BODY_BYTES + 1])
        return result

    def assert_transport_failure(self, **kwargs):
        with self.assertRaises(verifier.VerificationError) as caught:
            self.call_transport(**kwargs)
        self.assertNotIn(FAKE_TOKEN, str(caught.exception))
        return caught.exception

    def test_bounded_authenticated_success(self):
        self.assertEqual(self.call_transport(), COMPLETE)

    def test_malformed_truncated_duplicate_or_non_json_responses(self):
        for body in [b'{', b'\xff', b'{"value":NaN}', b'{"value":1,"value":2}',
                     (FAKE_TOKEN + " invalid").encode()]:
            with self.subTest(body=body):
                self.assert_transport_failure(body=body)

    def test_decoder_recursion_failure_has_a_safe_diagnostic(self):
        with patch.object(verifier, "_json", side_effect=RecursionError(FAKE_TOKEN)):
            self.assert_transport_failure(body=b"[]")

    def test_oversized_response(self):
        self.assert_transport_failure(body=b" " * (verifier.MAX_BODY_BYTES + 1))

    def test_http_error_redirect_network_and_timeout_are_fatal(self):
        for status in [302, 307, 308, 403, 404, 429, 500]:
            with self.subTest(status=status):
                error = HTTPError(self.URL, status, FAKE_TOKEN, {"Location": FAKE_TOKEN}, None)
                failure = self.assert_transport_failure(error=error)
                self.assertEqual(failure.http_status, status)
        for error in [URLError(FAKE_TOKEN), TimeoutError(FAKE_TOKEN), OSError(FAKE_TOKEN),
                      IncompleteRead(FAKE_TOKEN.encode(), 100)]:
            with self.subTest(error=type(error).__name__):
                self.assert_transport_failure(error=error)

    def test_unexpected_response_endpoint_and_status(self):
        self.assert_transport_failure(url="https://example.invalid/" + FAKE_TOKEN)
        self.assert_transport_failure(status=302)
        self.assert_transport_failure(status=True)

    def test_wrong_repo_or_arbitrary_urls_never_open_a_transport(self):
        with patch.object(verifier, "build_opener") as opener:
            for url in [self.URL.replace("knowell-dev/knowell", "other/repo"),
                        self.URL.replace("https://", "http://"),
                        self.URL + "#" + FAKE_TOKEN, self.URL + "?token=" + FAKE_TOKEN,
                        "https://api.github.com@evil.example/", verifier.API_ROOT + "/../other",
                        verifier.API_ROOT + "/alerts?ref=refs%2Fheads%2Fmain&tool_name=knowell&per_page=100&page=6"]:
                with self.subTest(url=url), self.assertRaises(verifier.VerificationError):
                    verifier.fetch_json(url, FAKE_TOKEN)
            opener.assert_not_called()

    def test_missing_or_header_injecting_token_is_rejected(self):
        with patch.object(verifier, "build_opener") as opener:
            for token in [None, "", FAKE_TOKEN + "\r\nother: value"]:
                with self.subTest(token=token), self.assertRaises(verifier.VerificationError):
                    verifier.fetch_json(self.URL, token)
            opener.assert_not_called()


class MainTests(unittest.TestCase):
    def test_main_success_and_failure_evidence_use_no_real_environment(self):
        with tempfile.TemporaryDirectory(prefix="knowell-sarif-verifier-") as temporary:
            directory = Path(temporary)
            (directory / "validation.json").write_text(json.dumps(VALIDATION), encoding="utf-8")
            environment = {"SARIF_DIR": temporary, "SARIF_ID": SARIF_ID,
                           "GITHUB_SHA": SHA, "GH_TOKEN": FAKE_TOKEN}
            stdout, stderr = io.StringIO(), io.StringIO()
            with patch.dict("os.environ", environment, clear=True), patch.object(
                    verifier, "fetch_json", side_effect=lambda url, token: FakeApi()(url)), \
                    contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.assertEqual(verifier.main(), 0)
            evidence = json.loads((directory / "processing.json").read_text(encoding="utf-8"))
            self.assertEqual(evidence["verification_status"], "complete")
            self.assertNotIn(FAKE_TOKEN, stdout.getvalue() + stderr.getvalue() + json.dumps(evidence))
            (directory / "processing.json").unlink()
            environment["SARIF_ID"] = FAKE_TOKEN
            with patch.dict("os.environ", environment, clear=True), contextlib.redirect_stderr(stderr):
                self.assertEqual(verifier.main(), 1)
            evidence = json.loads((directory / "processing.json").read_text(encoding="utf-8"))
            self.assertEqual(evidence["verification_status"], "failed")
            self.assertNotIn(FAKE_TOKEN, stderr.getvalue() + json.dumps(evidence))

    def test_pre_verification_failure_replaces_stale_completed_evidence(self):
        with tempfile.TemporaryDirectory(prefix="knowell-sarif-verifier-") as temporary:
            directory = Path(temporary)
            destination = directory / "processing.json"
            destination.write_text(json.dumps({
                "verification_status": "complete", "untrusted": FAKE_TOKEN,
            }), encoding="utf-8")
            with patch.dict("os.environ", {"SARIF_DIR": temporary}, clear=True), \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(verifier.main(), 1)
            evidence = json.loads(destination.read_text(encoding="utf-8"))
            self.assertEqual(evidence["verification_status"], "failed")
            self.assertNotIn(FAKE_TOKEN, json.dumps(evidence))

    def test_invalid_missing_or_oversized_validation_still_leaves_safe_evidence(self):
        for body in [None, b"{", b" " * (verifier.MAX_BODY_BYTES + 1),
                     b'[' * 2000 + b'0' + b']' * 2000]:
            with self.subTest(body_size=None if body is None else len(body)), \
                    tempfile.TemporaryDirectory(prefix="knowell-sarif-verifier-") as temporary:
                directory = Path(temporary)
                if body is not None:
                    (directory / "validation.json").write_bytes(body)
                with patch.dict("os.environ", {"SARIF_DIR": temporary}, clear=True), \
                        contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(verifier.main(), 1)
                evidence = json.loads((directory / "processing.json").read_text(encoding="utf-8"))
                self.assertEqual(evidence["verification_status"], "failed")


if __name__ == "__main__":
    unittest.main()
