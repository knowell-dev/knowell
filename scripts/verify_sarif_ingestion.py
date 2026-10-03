"""Verify processed Knowell SARIF and its actual GitHub source placement.

The workflow supplies SARIF_ID, SARIF_DIR, GH_TOKEN and GITHUB_SHA. Only the
canonical repository's main-branch smoke analysis is accepted. API response
bodies and credentials never enter diagnostics or the evidence artifact.
"""

from http.client import HTTPException
import json
import os
from pathlib import Path
import re
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import HTTPRedirectHandler, Request, build_opener


API_ROOT = "https://api.github.com/repos/knowell-dev/knowell/code-scanning"
CATEGORY = "knowell-sarif-smoke"
REF = "refs/heads/main"
ANALYSIS_KEY = ".github/workflows/sarif-smoke.yml:ingestion"
ENVIRONMENT = "{}"
MAX_BODY_BYTES = 1024 * 1024
PROCESSING_POLLS = 30
ALERT_POLLS = 10
ALERT_PAGES = 5
PAGE_SIZE = 100
ID_PATTERN = r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"


class VerificationError(Exception):
    """A fixed diagnostic with an optional numeric HTTP status, never a body."""

    def __init__(self, reason, http_status=None):
        super().__init__(reason)
        self.reason = reason
        self.http_status = http_status


class NoRedirect(HTTPRedirectHandler):
    """Refuse redirects before an authenticated request can leave its endpoint."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON member")
        result[key] = value
    return result


def _reject_constant(value):
    raise ValueError("non-JSON numeric constant")


def _json(body):
    return json.loads(body, object_pairs_hook=_object, parse_constant=_reject_constant)


def _integer(value, minimum=0):
    return type(value) is int and value >= minimum


def _allowed_url(url):
    prefix = re.escape(API_ROOT)
    endpoints = (
        rf"/sarifs/{ID_PATTERN}",
        rf"/analyses\?sarif_id={ID_PATTERN}",
        rf"/alerts\?ref=refs%2Fheads%2Fmain&tool_name=knowell&per_page={PAGE_SIZE}&page=[1-{ALERT_PAGES}]",
    )
    return isinstance(url, str) and any(
        re.fullmatch(prefix + endpoint, url) for endpoint in endpoints
    )


def fetch_json(url, token):
    """Fetch one canonical API resource; timeout is 5 s and body limit is 1 MiB."""
    if not _allowed_url(url):
        raise VerificationError("the API endpoint is not an allowed canonical resource")
    if not isinstance(token, str) or not token or "\r" in token or "\n" in token:
        raise VerificationError("a valid GitHub token is required")
    request = Request(
        url,
        headers={
            "Authorization": "Bearer " + token,
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2026-03-10",
        },
    )
    try:
        with build_opener(NoRedirect()).open(request, timeout=5) as response:
            if response.geturl() != url:
                raise VerificationError("the API response changed the requested endpoint")
            if response.status != 200:
                status = response.status
                if type(status) is not int or not 100 <= status <= 599:
                    raise VerificationError("the API returned an invalid HTTP status")
                raise VerificationError("the GitHub API lookup failed", status)
            body = response.read(MAX_BODY_BYTES + 1)
        if len(body) > MAX_BODY_BYTES:
            raise VerificationError("the API response exceeded the 1 MiB limit")
        return _json(body)
    except HTTPError as error:
        status = error.code if type(error.code) is int and 100 <= error.code <= 599 else None
        error.close()
        raise VerificationError("the GitHub API lookup failed", status) from None
    except (URLError, OSError, HTTPException, ValueError, RecursionError):
        raise VerificationError("the API lookup failed or returned invalid JSON") from None


def read_validation(directory):
    """Read only the bounded validation artifact, without returning raw errors."""
    try:
        with (directory / "validation.json").open("rb") as source:
            body = source.read(MAX_BODY_BYTES + 1)
        if len(body) > MAX_BODY_BYTES:
            raise VerificationError("the validation artifact exceeded the 1 MiB limit")
        return _json(body)
    except (OSError, ValueError, RecursionError):
        raise VerificationError("the validation artifact is missing or invalid") from None


def _expected(validation, sha):
    if not isinstance(validation, dict):
        raise VerificationError("the validation artifact is not an object")
    if (validation.get("tool") != "knowell"
            or validation.get("category") != CATEGORY
            or validation.get("commit_sha") != sha
            or not _integer(validation.get("results_count"), 1)):
        raise VerificationError("the validation identity or finding count is invalid")
    expected = validation.get("expected_result")
    if not isinstance(expected, dict):
        raise VerificationError("the expected source result is missing")
    rule = expected.get("rule_id")
    path = expected.get("path")
    start = expected.get("start_line")
    end = expected.get("end_line")
    if not isinstance(rule, str) or not re.fullmatch(r"[a-z][a-z0-9_.]{0,127}", rule):
        raise VerificationError("the expected rule identifier is invalid")
    if (not isinstance(path, str) or not 1 <= len(path) <= 4096
            or "\\" in path or ":" in path
            or any(ord(character) < 32 for character in path)
            or any(component in ("", ".", "..") for component in path.split("/"))):
        raise VerificationError("the expected source path is not repository relative")
    if not _integer(start, 1) or not _integer(end, start):
        raise VerificationError("the expected inclusive line range is invalid")
    return {"rule_id": rule, "path": path, "start_line": start, "end_line": end}


def _get(fetch, url):
    try:
        return fetch(url)
    except VerificationError:
        raise
    except Exception:
        # Injected fetches follow the same secret-free error boundary as the transport.
        raise VerificationError("the GitHub API lookup failed") from None


def _analysis(value, sarif_id, sha, count):
    if not isinstance(value, list) or len(value) != 1 or not isinstance(value[0], dict):
        raise VerificationError("the upload must produce exactly one analysis")
    analysis = value[0]
    tool = analysis.get("tool")
    if (not _integer(analysis.get("id"), 1)
            or analysis.get("sarif_id") != sarif_id
            or analysis.get("commit_sha") != sha
            or analysis.get("ref") != REF
            or analysis.get("category") != CATEGORY
            or not isinstance(tool, dict) or tool.get("name") != "knowell"
            or not _integer(analysis.get("results_count"), 1)
            or analysis.get("results_count") != count
            or analysis.get("error") != ""
            or analysis.get("analysis_key") != ANALYSIS_KEY
            or analysis.get("environment") != ENVIRONMENT):
        raise VerificationError("the processed analysis identity, count or error state is invalid")
    return analysis


def _matching_alert(alert, analysis, expected):
    if not isinstance(alert, dict):
        raise VerificationError("the alert response contains an invalid item")
    instance = alert.get("most_recent_instance")
    rule = alert.get("rule")
    if not isinstance(instance, dict) or not isinstance(rule, dict):
        raise VerificationError("the alert response is missing its instance or rule")
    location = instance.get("location")
    if not isinstance(location, dict):
        raise VerificationError("the alert response is missing its source location")
    identity = ("commit_sha", "ref", "category", "analysis_key", "environment")
    return (
        all(instance.get(field) == analysis[field] for field in identity)
        and instance.get("state") == "open"
        and alert.get("state") == "open"
        and isinstance(alert.get("tool"), dict)
        and alert["tool"].get("name") == "knowell"
        and rule.get("id") == expected["rule_id"]
        and location.get("path") == expected["path"]
        and type(location.get("start_line")) is int
        and type(location.get("end_line")) is int
        and location.get("start_line") == expected["start_line"]
        and location.get("end_line") == expected["end_line"]
    )


def verify(sarif_id, sha, validation, fetch, sleep=time.sleep, record=None):
    """Verify ingestion with bounded polling and return whitelisted evidence.

    ``fetch(url)`` and ``sleep(seconds)`` can be replaced for offline tests.
    ``record(evidence)`` receives progress and final evidence, including failures.
    """
    evidence = {"category": CATEGORY, "verification_status": "pending"}
    try:
        if not isinstance(sarif_id, str) or not re.fullmatch(ID_PATTERN, sarif_id):
            raise VerificationError("the upload did not provide a canonical SARIF UUID")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise VerificationError("the workflow did not provide a canonical commit SHA")
        evidence.update({"sarif_id": sarif_id, "commit_sha": sha})
        expected = _expected(validation, sha)
        count = validation["results_count"]
        analyses_url = f"{API_ROOT}/analyses?sarif_id={sarif_id}"
        for attempt in range(PROCESSING_POLLS):
            evidence["poll_attempts"] = attempt + 1
            status = _get(fetch, f"{API_ROOT}/sarifs/{sarif_id}")
            if not isinstance(status, dict):
                raise VerificationError("the processing response is not an object")
            processing = status.get("processing_status")
            if processing not in ("pending", "complete", "failed"):
                raise VerificationError("the processing status is unknown or missing")
            evidence["processing_status"] = processing
            if record:
                record(evidence)
            if processing == "failed":
                raise VerificationError("GitHub failed to process the SARIF upload")
            if processing == "complete":
                if status.get("analyses_url") != analyses_url:
                    raise VerificationError("the analysis lookup URL does not match the upload")
                evidence["analyses_url"] = analyses_url
                break
            if attempt + 1 < PROCESSING_POLLS:
                sleep(5)
        else:
            raise VerificationError("SARIF processing did not complete within the polling limit")

        analysis = _analysis(_get(fetch, analyses_url), sarif_id, sha, count)
        evidence.update({
            "analysis_id": analysis["id"], "ref": REF,
            "analysis_key": ANALYSIS_KEY, "environment": ENVIRONMENT,
            "results_count": count,
        })
        pages_fetched = 0
        for attempt in range(ALERT_POLLS):
            evidence["alert_poll_attempts"] = attempt + 1
            for page in range(1, ALERT_PAGES + 1):
                query = urlencode({
                    "ref": REF, "tool_name": "knowell", "per_page": PAGE_SIZE, "page": page,
                })
                alerts = _get(fetch, f"{API_ROOT}/alerts?{query}")
                pages_fetched += 1
                evidence["alert_pages_fetched"] = pages_fetched
                if not isinstance(alerts, list) or len(alerts) > PAGE_SIZE:
                    raise VerificationError("the alert page has an invalid shape or size")
                for alert in alerts:
                    if _matching_alert(alert, analysis, expected):
                        if not _integer(alert.get("number"), 1):
                            raise VerificationError("the matching alert has an invalid identifier")
                        evidence.update({
                            "alert_number": alert["number"], "expected_result": expected,
                            "verification_status": "complete",
                        })
                        return evidence
                if len(alerts) < PAGE_SIZE:
                    break
                if page == ALERT_PAGES:
                    raise VerificationError("the alert lookup exceeded the five-page limit")
            if record:
                record(evidence)
            if attempt + 1 < ALERT_POLLS:
                sleep(2)
        raise VerificationError("the expected source alert was not found within the polling limit")
    except VerificationError as error:
        evidence["verification_status"] = "failed"
        evidence["failure_reason"] = error.reason
        if error.http_status is not None:
            evidence["http_status"] = error.http_status
        raise
    finally:
        if record:
            record(evidence)


def main():
    """Write processing.json in the supplied artifact directory; return an exit code."""
    directory = os.environ.get("SARIF_DIR")
    if not directory:
        print("SARIF_DIR is required for verification evidence", file=sys.stderr)
        return 1
    destination = Path(directory) / "processing.json"
    latest = {"category": CATEGORY, "verification_status": "pending"}

    def record(evidence):
        latest.clear()
        latest.update(evidence.copy())
        try:
            destination.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
        except (OSError, ValueError):
            raise VerificationError("the processing evidence could not be written") from None

    try:
        record(latest.copy())
        validation = read_validation(Path(directory))
        token = os.environ.get("GH_TOKEN", "")
        if not token or "\r" in token or "\n" in token:
            raise VerificationError("a valid GitHub token is required")
        verify(
            os.environ.get("SARIF_ID"), os.environ.get("GITHUB_SHA"), validation,
            lambda url: fetch_json(url, token), record=record,
        )
    except VerificationError as error:
        try:
            # Pre-verification failures replace any evidence from a previous invocation.
            failed = {**latest, "verification_status": "failed", "failure_reason": error.reason}
            if error.http_status is not None:
                failed["http_status"] = error.http_status
            record(failed)
        except (VerificationError, OSError):
            pass
        print("SARIF verification failed: " + error.reason, file=sys.stderr)
        return 1
    print("GitHub SARIF processing, analysis identity and source placement are verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
