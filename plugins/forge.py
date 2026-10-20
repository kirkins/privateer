#!/usr/bin/env python3
"""Forge adapter for GitHub and Forgejo; no credentials or state held here.

Protocol: forge.py <op> with JSON on stdin and JSON reply on stdout.

Token handling (fail closed): the token is read only from the file named by
the PRIVATEER_FORGE_TOKEN_FILE environment variable. A token passed in the
request is refused; PRIVATEER_FORGE_TOKEN, GITHUB_TOKEN and FORGEJO_TOKEN are
ignored. An unset variable, a missing file, an empty file, or a token with
whitespace or control characters is an error before any request is sent, and
error messages never quote the token.

The runtime (review gate task 5) passes `api` and `forge` from forges.json;
the adapter never guesses the forge from the host. `api` must be https
(http is allowed only for loopback hosts so tests can run) and must not carry
credentials, a query, or a fragment.

HTTP redirects are refused outright: urllib's default handler would copy the
Authorization header to the redirect target and turn a redirected POST into
a GET. A 3xx reply is an error, never followed.

Operations:
  - pr.view: reads state, base, head sha, mergeable, and required checks at head
  - review.post: posts review (marker lookup to ensure idempotency)
  - pr.merge: executes merge passing the expected head commit (merge method only)
  - protection.read: reads branch protection and returns configured settings and warnings
"""
import json
import os
from pathlib import Path
import re
import string
import sys
import urllib.error
import urllib.parse
import urllib.request

LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
# The marker is the exact string '<!-- privateer-review <id> -->'; the bare id
# never matches, so a forged or partial mention cannot fake idempotency.
MARKER_RE = re.compile(r"<!-- privateer-review ([^\s<>]+) -->")
PAGE_SIZE = 100  # Forgejo's per-page limit; GitHub accepts it too
MAX_PAGES = 20


class ForgeError(Exception):
    """A caller-facing failure. The message never contains the token."""

    def __init__(self, message, status_code=None):
        super().__init__(message)
        self.status_code = status_code


# ---------------------------------------------------------------------------
# Token
# ---------------------------------------------------------------------------

def load_token():
    """Read the token from the file named by PRIVATEER_FORGE_TOKEN_FILE only."""
    var = "PRIVATEER_FORGE_TOKEN_FILE"
    path = os.environ.get(var)
    if not path:
        raise ForgeError(f"{var} is not set: no token source, refusing to run")
    try:
        raw = Path(path).expanduser().read_text()
    except OSError:
        raise ForgeError(f"cannot read the token file named by {var}: refusing to run")
    token = raw.strip()
    if not token:
        raise ForgeError(f"the token file named by {var} is empty: refusing to run")
    if any(ch in string.whitespace for ch in token):
        raise ForgeError("token contains whitespace: refusing to run")
    if any(ord(ch) < 32 or ord(ch) == 127 for ch in token):
        raise ForgeError("token contains control characters: refusing to run")
    return token


def refuse_request_tokens(request):
    for key in ("token", "token_file"):
        if key in request:
            raise ForgeError(
                f"'{key}' must not be passed in the request: the token comes only "
                "from PRIVATEER_FORGE_TOKEN_FILE"
            )


# ---------------------------------------------------------------------------
# Request validation
# ---------------------------------------------------------------------------

def get_forge_kind(request):
    forge = request.get("forge")
    if not isinstance(forge, str) or not forge.strip():
        raise ForgeError(
            "forge must be given explicitly as 'github' or 'forgejo' "
            "(the runtime passes it from forges.json)"
        )
    forge = forge.strip().lower()
    if forge not in ("github", "forgejo"):
        raise ForgeError(f"unsupported forge: {forge!r}")
    return forge


def validate_api(api):
    if not isinstance(api, str) or not api.strip():
        raise ForgeError("api is required (the runtime passes it from forges.json)")
    parsed = urllib.parse.urlparse(api)
    if parsed.scheme == "http":
        host = (parsed.hostname or "").lower()
        if host not in LOOPBACK_HOSTS:
            raise ForgeError(
                "http API URLs are allowed only for loopback hosts: use https"
            )
    elif parsed.scheme != "https":
        raise ForgeError("API URL scheme must be https (http only for loopback hosts)")
    if not parsed.hostname:
        raise ForgeError("API URL must include a host")
    if parsed.username or parsed.password:
        raise ForgeError("API URL must not embed credentials")
    if parsed.query or parsed.fragment:
        raise ForgeError("API URL must not include a query or fragment")
    return api.strip().rstrip("/")


def validate_repo(repo):
    if not isinstance(repo, str) or not REPO_RE.match(repo):
        raise ForgeError("repo is required and must look like 'owner/name' "
                         "(letters, digits, '_', '.', '-' only)")
    if any(segment == ".." for segment in repo.split("/")):
        raise ForgeError("repo must not contain '..' segments")
    return repo


def validate_pr(pr):
    if isinstance(pr, bool) or not isinstance(pr, int) or pr < 1:
        raise ForgeError("pr must be a positive integer")
    return pr


def validate_sha(value, field):
    if not isinstance(value, str) or not SHA_RE.match(value.lower()):
        raise ForgeError(f"{field} must be a 40-character hex commit sha")
    return value.lower()


def quote_branch(branch):
    if not isinstance(branch, str) or not branch.strip():
        raise ForgeError("branch is required")
    if any(ord(ch) < 32 or ord(ch) == 127 for ch in branch):
        raise ForgeError("branch must not contain control characters")
    return urllib.parse.quote(branch, safe="")


def validate_required_checks(value, allow_empty=False):
    if not isinstance(value, list):
        raise ForgeError("required_checks must be a list of check names")
    if not value and not allow_empty:
        raise ForgeError("required_checks must be a non-empty list of check names")
    for name in value:
        if not isinstance(name, str) or not name.strip():
            raise ForgeError("required_checks entries must be non-empty strings")
    return list(value)


def first_of(request, keys):
    for key in keys:
        value = request.get(key)
        if value is not None:
            return value
    return None


def pick_pr(request):
    for key in ("pr", "pull", "pull_number", "index"):
        if request.get(key) is not None:
            return validate_pr(request[key])
    raise ForgeError("pr is required")


def begin_request(request):
    """Validate the shared fields and return (forge, api, headers)."""
    refuse_request_tokens(request)
    forge = get_forge_kind(request)
    api = validate_api(request.get("api"))
    if forge == "forgejo" and not api.endswith("/api/v1"):
        api = f"{api}/api/v1"
    token = load_token()
    return forge, api, build_headers(forge, token)


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------

class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse every redirect.

    urllib's default redirect handler copies the Authorization header to any
    redirect target and rewrites a redirected POST as a GET. Neither is ever
    acceptable here, so a redirect is raised as an error instead.
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ForgeError(
            f"redirect refused: the forge replied HTTP {code} with a redirect; "
            f"not following it to {newurl}"
        )


_OPENER = urllib.request.build_opener(_NoRedirectHandler)


def build_headers(forge, token):
    headers = {
        "User-Agent": "privateer-forge-adapter",
    }
    if forge == "github":
        headers["Accept"] = "application/vnd.github+json"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
        headers["Authorization"] = f"Bearer {token}"
    else:  # forgejo
        headers["Accept"] = "application/json"
        headers["Authorization"] = f"token {token}"
    return headers


def _parse_body(content):
    if not content:
        return {}
    try:
        return json.loads(content)
    except ValueError:
        return {"message": content[:400]}


def _body_message(data):
    if isinstance(data, dict):
        return data.get("message", "")
    return str(data)


def http_request(url, method="GET", headers=None, data=None, timeout=30):
    if headers is None:
        headers = {}
    body = None
    if data is not None:
        body = json.dumps(data).encode("utf-8")
        headers = dict(headers)
        headers["Content-Type"] = "application/json"

    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with _OPENER.open(req, timeout=timeout) as resp:
            content = resp.read().decode("utf-8", errors="replace")
            status = resp.status
    except ForgeError:
        raise
    except urllib.error.HTTPError as e:
        try:
            content = e.read().decode("utf-8", errors="replace")
        finally:
            e.close()
        return e.code, _parse_body(content)
    except urllib.error.URLError as e:
        raise ForgeError(f"network error contacting the forge API: {type(e.reason).__name__}")
    except (OSError, ValueError):
        # http.client failures here can quote header values (the token) in
        # their message text; never let that text reach the reply.
        raise ForgeError("request failed before reaching the forge: "
                         "transport or header error")
    return status, _parse_body(content)


def _page_items(data):
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        for key in ("check_runs", "statuses"):
            value = data.get(key)
            if isinstance(value, list):
                return value
    return None


def get_pages(url, headers):
    """GET url following per_page=100 pagination; returns each (status, parsed) page."""
    pages = []
    page = 1
    while True:
        sep = "&" if "?" in url else "?"
        code, data = http_request(f"{url}{sep}per_page={PAGE_SIZE}&page={page}",
                                  headers=headers)
        pages.append((code, data))
        items = _page_items(data)
        if code != 200 or not isinstance(items, list) or len(items) < PAGE_SIZE:
            return pages
        page += 1
        if page > MAX_PAGES:
            raise ForgeError(f"forge response pagination exceeded {MAX_PAGES} pages")


# ---------------------------------------------------------------------------
# Review events
# ---------------------------------------------------------------------------

# name -> (event sent to the forge API, state recorded on a review)
EVENTS = {
    "github": {
        "APPROVE": ("APPROVE", "APPROVED"),
        "REQUEST_CHANGES": ("REQUEST_CHANGES", "CHANGES_REQUESTED"),
        "COMMENT": ("COMMENT", "COMMENTED"),
    },
    "forgejo": {
        "APPROVE": ("APPROVED", "APPROVED"),
        "REQUEST_CHANGES": ("REQUEST_CHANGES", "REQUEST_CHANGES"),
        "COMMENT": ("COMMENT", "COMMENT"),
    },
}
EVENT_ALIASES = {
    "APPROVED": "APPROVE",
    "CHANGES_REQUESTED": "REQUEST_CHANGES",
    "COMMENTED": "COMMENT",
}


def normalize_event(event, forge):
    if not isinstance(event, str) or not event.strip():
        raise ForgeError("event must be a non-empty string")
    name = event.strip().upper()
    name = EVENT_ALIASES.get(name, name)
    table = EVENTS[forge]
    if name not in table:
        raise ForgeError(f"unsupported review event: {event!r}")
    return table[name]


def token_identity(api, headers, forge):
    """Read the login the token itself speaks for, never a caller-supplied one."""
    code, data = http_request(f"{api}/user", headers=headers)
    if code != 200 or not isinstance(data, dict):
        raise ForgeError(f"failed to read the token identity: HTTP {code}", code)
    login = data.get("login") or data.get("username")
    if not isinstance(login, str) or not login.strip():
        raise ForgeError("the token identity reply did not include a login")
    return login


# ---------------------------------------------------------------------------
# Operations
# ---------------------------------------------------------------------------

def pr_view(request):
    forge, api, headers = begin_request(request)
    repo = validate_repo(request.get("repo"))
    pr = pick_pr(request)
    required = validate_required_checks(request.get("required_checks"))

    status, pr_data = http_request(f"{api}/repos/{repo}/pulls/{pr}", headers=headers)
    if status != 200:
        raise ForgeError(
            f"failed to fetch PR #{pr}: HTTP {status} {_body_message(pr_data)}", status
        )
    if not isinstance(pr_data, dict):
        raise ForgeError("PR reply was not a JSON object")
    head_sha = validate_sha((pr_data.get("head") or {}).get("sha"), "PR head sha")

    checks = {}
    if forge == "github":
        # Combined status (latest per context) plus check runs, paginated.
        code, data = http_request(
            f"{api}/repos/{repo}/commits/{head_sha}/status", headers=headers)
        if code != 200 or not isinstance(data, dict):
            raise ForgeError(
                f"failed to read the combined status at head: HTTP {code}", code)
        for entry in data.get("statuses") or []:
            context = entry.get("context")
            if context:
                checks[context] = entry.get("state")

        for page_code, page in get_pages(
                f"{api}/repos/{repo}/commits/{head_sha}/check-runs", headers):
            if page_code != 200 or not isinstance(page, dict):
                raise ForgeError(
                    f"failed to read check runs at head: HTTP {page_code}", page_code)
            for run in page.get("check_runs") or []:
                name = run.get("name")
                if name:
                    checks[name] = (run.get("conclusion")
                                    if run.get("status") == "completed"
                                    else run.get("status"))
    else:
        # Forgejo combined status: latest status per context, not the flat list.
        code, data = http_request(
            f"{api}/repos/{repo}/commits/{head_sha}/status", headers=headers)
        if code != 200 or not isinstance(data, dict):
            raise ForgeError(
                f"failed to read the combined status at head: HTTP {code}", code)
        for entry in data.get("statuses") or []:
            context = entry.get("context")
            if context:
                checks[context] = entry.get("status") or entry.get("state")

    required_checks_ok = all(checks.get(name) == "success" for name in required)
    return {
        "ok": True,
        "state": pr_data.get("state"),
        "base": (pr_data.get("base") or {}).get("ref"),
        "base_sha": (pr_data.get("base") or {}).get("sha"),
        "head_sha": head_sha,
        "mergeable": pr_data.get("mergeable"),
        "merged": pr_data.get("merged", False),
        "checks": checks,
        "required_checks": required,
        "required_checks_ok": required_checks_ok,
        "checks_not_success": [n for n in required if checks.get(n) != "success"],
    }


def review_post(request):
    forge, api, headers = begin_request(request)
    repo = validate_repo(request.get("repo"))
    pr = pick_pr(request)
    commit_id = validate_sha(
        first_of(request, ("commit_id", "head_sha", "sha")), "commit_id")
    body = request.get("body")
    if not isinstance(body, str) or not body.strip():
        raise ForgeError("body is required")
    event, match_state = normalize_event(request.get("event", "APPROVE"), forge)

    # The reviewer is whoever the token speaks for; a caller-supplied claim
    # that disagrees is refused.
    identity = token_identity(api, headers, forge)
    claimed = request.get("reviewer") or request.get("account")
    if claimed and claimed != identity:
        raise ForgeError(
            f"reviewer '{claimed}' does not match the login the token speaks "
            f"for ('{identity}')"
        )

    marker_match = MARKER_RE.search(body)
    if not marker_match:
        raise ForgeError(
            "review body must contain a '<!-- privateer-review <id> -->' marker"
        )
    marker = marker_match.group(0)

    reviews_url = f"{api}/repos/{repo}/pulls/{pr}/reviews"
    reviews = []
    for code, data in get_pages(reviews_url, headers):
        if code != 200 or not isinstance(data, list):
            raise ForgeError(f"failed to read existing reviews: HTTP {code}", code)
        reviews.extend(data)

    for existing in reviews:
        user = existing.get("user") or {}
        login = user.get("login") or user.get("username")
        if login != identity:
            continue  # a marker posted by anyone else does not count
        if marker not in (existing.get("body") or ""):
            continue  # the bare id or a partial marker does not count
        if (existing.get("commit_id") or "") != commit_id:
            continue  # the review must be for the commit being approved
        state = existing.get("state")
        if state == "DISMISSED":
            continue
        if state != match_state:
            continue
        return {
            "ok": True,
            "posted": False,
            "already_exists": True,
            "review_id": existing.get("id"),
            "state": state,
        }

    payload = {"commit_id": commit_id, "event": event, "body": body}
    status, post_data = http_request(reviews_url, method="POST",
                                     headers=headers, data=payload)
    if status not in (200, 201):
        raise ForgeError(
            f"failed to post review: HTTP {status} {_body_message(post_data)}", status)
    return {
        "ok": True,
        "posted": True,
        "already_exists": False,
        "review_id": post_data.get("id"),
        "state": post_data.get("state"),
    }


def pr_merge(request):
    forge, api, headers = begin_request(request)
    repo = validate_repo(request.get("repo"))
    pr = pick_pr(request)
    head_sha = validate_sha(
        first_of(request, ("head_sha", "sha", "head_commit_id")), "head_sha")
    method = request.get("method") or request.get("merge_method") or "merge"
    if method != "merge":
        raise ForgeError(f"merge method {method!r} is not allowed: only 'merge' is supported")

    merge_url = f"{api}/repos/{repo}/pulls/{pr}/merge"
    if forge == "github":
        payload = {"sha": head_sha, "merge_method": "merge"}
        if request.get("commit_title"):
            payload["commit_title"] = request["commit_title"]
        if request.get("commit_message"):
            payload["commit_message"] = request["commit_message"]
        status, res_data = http_request(merge_url, method="PUT",
                                        headers=headers, data=payload)
    else:  # forgejo
        payload = {"Do": "merge", "head_commit_id": head_sha}
        if request.get("commit_title"):
            payload["MergeTitleField"] = request["commit_title"]
        if request.get("commit_message"):
            payload["MergeMessageField"] = request["commit_message"]
        status, res_data = http_request(merge_url, method="POST",
                                        headers=headers, data=payload)

    if status != 200:
        raise ForgeError(
            f"merge failed: HTTP {status} {_body_message(res_data)}", status)
    merge_sha = res_data.get("sha") if isinstance(res_data, dict) else None
    return {
        "ok": True,
        "merged": res_data.get("merged", True) if isinstance(res_data, dict) else True,
        "merge_sha": merge_sha,
        "sha": merge_sha,
        "message": res_data.get("message", "merged") if isinstance(res_data, dict) else "merged",
    }


def protection_read(request):
    forge, api, headers = begin_request(request)
    repo = validate_repo(request.get("repo"))
    branch = request.get("branch", "main")
    branch_q = quote_branch(branch)
    reviewer = request.get("reviewer") or request.get("account")
    merger = request.get("merger")
    required_checks = validate_required_checks(
        request.get("required_checks", []), allow_empty=True)

    warnings = []

    if forge == "github":
        url = f"{api}/repos/{repo}/branches/{branch_q}/protection"
        status, data = http_request(url, headers=headers)
        if status == 404:
            message = _body_message(data)
            if isinstance(message, str) and message.strip().lower() == "branch not protected":
                return {
                    "ok": True,
                    "protected": False,
                    "branch": branch,
                    "warnings": [f"branch '{branch}' is not protected"],
                }
            raise ForgeError(
                f"failed to read branch protection: HTTP 404 {message}", 404)
        if status != 200:
            raise ForgeError(
                f"failed to read branch protection: HTTP {status} "
                f"{_body_message(data)}", status)

        enforce_admins = (data.get("enforce_admins") or {}).get("enabled", False)
        reviews_obj = data.get("required_pull_request_reviews")
        if reviews_obj:
            required_approvals = reviews_obj.get("required_approving_review_count", 0)
            require_code_owner = reviews_obj.get("require_code_owner_reviews", False)
            dismiss_stale = reviews_obj.get("dismiss_stale_reviews", False)
            require_last_push = reviews_obj.get("require_last_push_approval", False)
        else:
            required_approvals = 0
            require_code_owner = False
            dismiss_stale = False
            require_last_push = False

        status_obj = data.get("required_status_checks") or {}
        contexts = list(status_obj.get("contexts", []))
        for check in status_obj.get("checks", []):
            if check.get("context") and check["context"] not in contexts:
                contexts.append(check["context"])

        allow_force_pushes = (data.get("allow_force_pushes") or {}).get("enabled", False)
        allow_deletions = (data.get("allow_deletions") or {}).get("enabled", False)

        if not enforce_admins:
            warnings.append("enforce_admins is not enabled")
        if not reviews_obj:
            warnings.append("pull request reviews are not required")
        else:
            if required_approvals < 1:
                warnings.append("required approving review count is less than 1")
            if not require_code_owner:
                warnings.append("require code owner reviews is not enabled")
            if not dismiss_stale:
                warnings.append("dismiss stale reviews is not enabled")
            if not require_last_push:
                warnings.append("require approval of most recent push is not enabled")
        if not contexts:
            warnings.append("no required status checks are configured")
        if allow_force_pushes:
            warnings.append("force pushes are allowed")
        if allow_deletions:
            warnings.append("branch deletions are allowed")
        for rc in required_checks:
            if rc not in contexts:
                warnings.append(f"required status check '{rc}' is not in required contexts")

        return {
            "ok": True,
            "protected": True,
            "branch": branch,
            "enforce_admins": enforce_admins,
            "required_approvals": required_approvals,
            "require_code_owner_reviews": require_code_owner,
            "dismiss_stale_approvals": dismiss_stale,
            "require_last_push_approval": require_last_push,
            "allow_force_pushes": allow_force_pushes,
            "allow_deletions": allow_deletions,
            "status_checks": contexts,
            "warnings": warnings,
        }

    # forgejo
    if not reviewer:
        raise ForgeError(
            "reviewer is required for forgejo protection.read: the approvals "
            "whitelist cannot be checked without it")
    if not merger:
        raise ForgeError(
            "merger is required for forgejo protection.read: the merge "
            "whitelist cannot be checked without it")

    url = f"{api}/repos/{repo}/branch_protections/{branch_q}"
    status, data = http_request(url, headers=headers)
    if status == 404:
        list_url = f"{api}/repos/{repo}/branch_protections"
        l_status, l_data = http_request(list_url, headers=headers)
        if l_status != 200 or not isinstance(l_data, list):
            raise ForgeError(
                f"failed to list branch protections: HTTP {l_status} "
                f"{_body_message(l_data)}", l_status)
        data = next(
            (item for item in l_data if isinstance(item, dict) and (
                item.get("branch_name") == branch or item.get("rule_name") == branch)),
            None)
        if data is None:
            return {
                "ok": True,
                "protected": False,
                "branch": branch,
                "warnings": [f"branch '{branch}' has no branch protection"],
            }
    elif status != 200:
        raise ForgeError(
            f"failed to read branch protection: HTTP {status} {_body_message(data)}",
            status)

    enable_push = data.get("enable_push", True)
    apply_to_admins = data.get("apply_to_admins", False)
    required_approvals = data.get("required_approvals", 0)
    enable_approvals_whitelist = data.get("enable_approvals_whitelist", False)
    approvals_whitelist = data.get("approvals_whitelist_usernames", [])
    enable_merge_whitelist = data.get("enable_merge_whitelist", False)
    merge_whitelist = data.get("merge_whitelist_usernames", [])
    dismiss_stale = data.get("dismiss_stale_approvals", False)
    block_on_rejected = data.get("block_on_rejected_reviews", False)
    enable_status_check = data.get("enable_status_check", False)
    status_checks = data.get("status_check_contexts", [])
    enable_force_push = data.get("enable_force_push", False)

    if enable_push:
        warnings.append("enable_push is not false")
    if not apply_to_admins:
        warnings.append("apply_to_admins is not true")
    if required_approvals < 1:
        warnings.append("required_approvals is less than 1")
    if not enable_approvals_whitelist:
        warnings.append("enable_approvals_whitelist is not true")
    if reviewer not in approvals_whitelist:
        warnings.append(f"reviewer '{reviewer}' not in approvals_whitelist_usernames")
    if enable_approvals_whitelist:
        for user in approvals_whitelist:
            if user != reviewer:
                warnings.append(
                    f"approvals whitelist contains a user other than the reviewer: '{user}'")
    if not enable_merge_whitelist:
        warnings.append("enable_merge_whitelist is not true")
    if merger not in merge_whitelist:
        warnings.append(f"merger '{merger}' not in merge_whitelist_usernames")
    if enable_merge_whitelist:
        for user in merge_whitelist:
            if user != merger:
                warnings.append(
                    f"merge whitelist contains a user other than the merger: '{user}'")
    if enable_force_push:
        warnings.append("force pushes are enabled (enable_force_push)")
    if not dismiss_stale:
        warnings.append("dismiss_stale_approvals is not true")
    if not block_on_rejected:
        warnings.append("block_on_rejected_reviews is not true")
    if not enable_status_check:
        warnings.append("enable_status_check is not true")
    for rc in required_checks:
        if rc not in status_checks:
            warnings.append(f"required status check '{rc}' is not in status_check_contexts")

    return {
        "ok": True,
        "protected": True,
        "branch": branch,
        "enable_push": enable_push,
        "apply_to_admins": apply_to_admins,
        "required_approvals": required_approvals,
        "enable_approvals_whitelist": enable_approvals_whitelist,
        "approvals_whitelist": approvals_whitelist,
        "enable_merge_whitelist": enable_merge_whitelist,
        "merge_whitelist": merge_whitelist,
        "enable_force_push": enable_force_push,
        "dismiss_stale_approvals": dismiss_stale,
        "block_on_rejected_reviews": block_on_rejected,
        "enable_status_check": enable_status_check,
        "status_checks": status_checks,
        "warnings": warnings,
    }


OPERATIONS = {
    "pr.view": pr_view,
    "review.post": review_post,
    "pr.merge": pr_merge,
    "protection.read": protection_read,
}


def handle(op, request):
    if not op or op not in OPERATIONS:
        raise ForgeError(f"unsupported forge operation: {op}")
    return OPERATIONS[op](request)


def main():
    request = None
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            raise ForgeError("empty request on stdin")
        try:
            request = json.loads(raw)
        except ValueError:
            raise ForgeError("invalid JSON on stdin")
        if not isinstance(request, dict):
            raise ForgeError("request must be a JSON object")

        op = sys.argv[1] if len(sys.argv) > 1 else (
            request.get("op") or request.get("method") or request.get("command"))
        result = handle(op, request)

        response = {
            "ok": True,
            "version": request.get("version", 1),
            **result,
        }
        # Echo the caller's id under its own key; a result's 'id' (a review
        # id, for instance) is never overwritten by it.
        if "id" in request:
            response["request_id"] = request["id"]
    except ForgeError as e:
        response = {
            "ok": False,
            "version": request.get("version", 1) if isinstance(request, dict) else 1,
            "error": str(e),
        }
        if e.status_code is not None:
            response["status_code"] = e.status_code
        if isinstance(request, dict) and "id" in request:
            response["request_id"] = request["id"]
    except Exception:
        # A raw exception message can quote header values (the token); only
        # the exception class is ever reported.
        response = {"ok": False, "error": "internal error"}
        if isinstance(request, dict):
            response["version"] = request.get("version", 1)
            if "id" in request:
                response["request_id"] = request["id"]
    print(json.dumps(response, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
