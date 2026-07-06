#!/usr/bin/env python3
"""Detect and remove malware-link spam comments on GitHub issues.

Targets the common drive-by campaign: a brand-new account posts a comment
whose entire body is a link to ``github.com/<other-repo>/releases/download/
.../<something>.zip`` (e.g. ``critical_patch_2026.zip``).

Modes:
    event   read the issue_comment payload from ``GITHUB_EVENT_PATH`` and
            act on that single comment (used by the workflow trigger)
    scan    page through every issue comment in the repository and act on
            all matches (used by ``workflow_dispatch`` to clean up backlog)

Action on a match is controlled by ``SPAM_GUARD_ACTION``:
    delete      remove the comment (default)
    minimize    hide the comment as spam, keep it for evidence
    report-only log matches without touching anything

Requires ``GITHUB_TOKEN`` with ``issues: write`` and ``GITHUB_REPOSITORY``.
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request

API_ROOT = os.environ.get("GITHUB_API_URL", "https://api.github.com")

# Comment authors with these associations are never touched.
TRUSTED_ASSOCIATIONS = {"OWNER", "MEMBER", "COLLABORATOR"}

URL_RE = re.compile(r"https?://\S+")
ARCHIVE_EXT_RE = re.compile(r"\.(zip|rar|7z|exe|msi|scr|bat)([?#].*)?$", re.IGNORECASE)
RELEASE_DOWNLOAD_RE = re.compile(
    r"^https?://(?:www\.)?github\.com/([^/\s]+)/([^/\s]+)/releases/download/",
    re.IGNORECASE,
)
LURE_KEYWORD_RE = re.compile(
    r"(patch|hotfix|fix|update|release|install|setup|crack|keygen|password)",
    re.IGNORECASE,
)


def _strip_urls(body: str) -> str:
    """Return the comment text with URLs and link punctuation removed."""
    rest = URL_RE.sub("", body)
    return re.sub(r"[\s()\[\]<>`*_]+", "", rest)


def classify(body: str, author_association: str, repo_owner: str) -> list[str]:
    """Return the list of spam signals; non-empty means the comment is spam."""
    if author_association.upper() in TRUSTED_ASSOCIATIONS:
        return []

    urls = URL_RE.findall(body or "")
    if not urls:
        return []
    if _strip_urls(body):
        # There is real prose around the link; do not touch it.
        return []

    reasons: list[str] = []
    for raw in urls:
        url = raw.rstrip(").,;>]")
        release = RELEASE_DOWNLOAD_RE.match(url)
        is_archive = bool(ARCHIVE_EXT_RE.search(url))
        if release and is_archive:
            reasons.append(f"link-only comment with release archive: {url}")
            if release.group(1).lower() != repo_owner.lower():
                reasons.append("archive is hosted on an unrelated repository")
        elif is_archive and LURE_KEYWORD_RE.search(url.rsplit("/", 1)[-1]):
            reasons.append(f"link-only comment with lure-named archive: {url}")
    return reasons


def _request(method: str, url: str, token: str, body: dict | None = None) -> dict | list | None:
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req) as resp:
        payload = resp.read()
    return json.loads(payload) if payload else None


def _minimize(node_id: str, token: str) -> None:
    query = (
        "mutation($id: ID!) {"
        " minimizeComment(input: {subjectId: $id, classifier: SPAM})"
        " { minimizedComment { isMinimized } } }"
    )
    _request("POST", f"{API_ROOT}/graphql", token, {"query": query, "variables": {"id": node_id}})


def act_on_comment(comment: dict, repo: str, token: str, action: str) -> bool:
    """Check one REST comment object; act on it if spam. Returns True on match."""
    owner = repo.split("/")[0]
    reasons = classify(
        comment.get("body") or "",
        comment.get("author_association") or "NONE",
        owner,
    )
    if not reasons:
        return False

    login = (comment.get("user") or {}).get("login", "<unknown>")
    print(f"SPAM comment {comment['id']} by {login}: {comment.get('html_url', '')}")
    for reason in reasons:
        print(f"  - {reason}")

    if action == "report-only":
        print("  action: report-only (no change made)")
    elif action == "minimize":
        _minimize(comment["node_id"], token)
        print("  action: minimized as spam")
    else:
        _request("DELETE", f"{API_ROOT}/repos/{repo}/issues/comments/{comment['id']}", token)
        print("  action: deleted")
    return True


def run_event(repo: str, token: str, action: str) -> int:
    with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as fh:
        event = json.load(fh)
    comment = event.get("comment")
    if not comment:
        print("no comment in event payload; nothing to do")
        return 0
    matched = act_on_comment(comment, repo, token, action)
    print("1 spam comment handled" if matched else "comment looks fine")
    return 0


def run_scan(repo: str, token: str, action: str) -> int:
    matched = 0
    page = 1
    while True:
        comments = _request(
            "GET",
            f"{API_ROOT}/repos/{repo}/issues/comments?per_page=100&page={page}",
            token,
        )
        if not comments:
            break
        for comment in comments:
            if act_on_comment(comment, repo, token, action):
                matched += 1
        page += 1
    print(f"scan complete: {matched} spam comment(s) handled")
    return 0


def main(argv: list[str]) -> int:
    mode = argv[1] if len(argv) > 1 else "event"
    repo = os.environ["GITHUB_REPOSITORY"]
    token = os.environ["GITHUB_TOKEN"]
    action = os.environ.get("SPAM_GUARD_ACTION", "delete")
    if action not in {"delete", "minimize", "report-only"}:
        print(f"unknown SPAM_GUARD_ACTION {action!r}", file=sys.stderr)
        return 2
    if mode == "scan":
        return run_scan(repo, token, action)
    if mode == "event":
        return run_event(repo, token, action)
    print(f"unknown mode {mode!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
