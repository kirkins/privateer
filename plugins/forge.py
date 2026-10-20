#!/usr/bin/env python3
"""Forge adapter for GitHub and Forgejo; no credentials or state held here.

Protocol: forge.py <op> with JSON on stdin and JSON reply on stdout.
Token file path in PRIVATEER_FORGE_TOKEN_FILE.
Operations:
  - pr.view: reads state, base, head sha, mergeable, and required checks at head
  - review.post: posts review (with marker lookup to ensure idempotency)
  - pr.merge: executes merge passing the expected head commit
  - protection.read: reads branch protection and returns configured settings and warnings
"""
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


def get_token(request, forge):
    token = request.get("token")
    if token:
        return token
    token_file = os.environ.get("PRIVATEER_FORGE_TOKEN_FILE") or request.get("token_file")
    if token_file:
        p = Path(token_file).expanduser()
        if p.is_file():
            return p.read_text().strip()
    if os.environ.get("PRIVATEER_FORGE_TOKEN"):
        return os.environ.get("PRIVATEER_FORGE_TOKEN").strip()
    if forge == "github" and os.environ.get("GITHUB_TOKEN"):
        return os.environ.get("GITHUB_TOKEN").strip()
    if forge == "forgejo" and os.environ.get("FORGEJO_TOKEN"):
        return os.environ.get("FORGEJO_TOKEN").strip()
    return None


def normalize_repo(repo):
    if not repo or not isinstance(repo, str):
        raise ValueError("invalid repo")
    # Strip leading domain if present (e.g. github.com/owner/repo or git.host/owner/repo)
    parts = repo.split("/")
    if len(parts) >= 3 and ("." in parts[0] or ":" in parts[0]):
        repo = "/".join(parts[1:])
    return repo.strip("/")


def get_forge_and_api(request):
    forge = request.get("forge") or request.get("kind")
    api = request.get("api") or request.get("url")
    repo = request.get("repo", "")

    if not forge:
        if api and ("api.github.com" in api or "github" in api):
            forge = "github"
        elif api and ("forgejo" in api or "gitea" in api):
            forge = "forgejo"
        elif repo and repo.startswith("github.com/"):
            forge = "github"
        else:
            forge = "github"

    forge = forge.lower()
    if forge not in ("github", "forgejo"):
        raise ValueError(f"unsupported forge: {forge}")

    if not api:
        if forge == "github":
            api = "https://api.github.com"
        else:
            api = "https://git.lobbykit.net/api/v1"

    api = api.rstrip("/")
    if forge == "forgejo":
        if not api.endswith("/api/v1") and "/api/v1" not in api:
            api = f"{api}/api/v1"

    return forge, api


def build_headers(forge, token):
    headers = {
        "User-Agent": "privateer-forge-adapter",
    }
    if forge == "github":
        headers["Accept"] = "application/vnd.github+json"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
        if token:
            headers["Authorization"] = f"Bearer {token}"
    elif forge == "forgejo":
        headers["Accept"] = "application/json"
        if token:
            headers["Authorization"] = f"token {token}"
    return headers


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
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            content = resp.read().decode("utf-8")
            status = resp.status
            parsed = json.loads(content) if content else {}
            return status, parsed
    except urllib.error.HTTPError as e:
        err_body = e.read().decode("utf-8", errors="replace")
        e.close()
        try:
            parsed = json.loads(err_body)
        except Exception:
            parsed = {"message": err_body, "raw": err_body}
        return e.code, parsed
    except urllib.error.URLError as e:
        raise RuntimeError(f"network error: {e.reason}")


def pr_view(request):
    forge, api = get_forge_and_api(request)
    repo = normalize_repo(request["repo"])
    pr = request.get("pr") or request.get("pull") or request.get("pull_number") or request.get("index")
    if pr is None:
        raise ValueError("pr is required")
    token = get_token(request, forge)
    headers = build_headers(forge, token)

    # 1. Fetch pull request
    url = f"{api}/repos/{repo}/pulls/{pr}"
    status, pr_data = http_request(url, method="GET", headers=headers)
    if status != 200:
        err_msg = pr_data.get("message") if isinstance(pr_data, dict) else str(pr_data)
        return {
            "ok": False,
            "error": f"failed to fetch PR #{pr}: HTTP {status} {err_msg}",
            "status_code": status,
        }

    state = pr_data.get("state")
    base_ref = pr_data.get("base", {}).get("ref")
    base_sha = pr_data.get("base", {}).get("sha")
    head_sha = pr_data.get("head", {}).get("sha")
    mergeable = pr_data.get("mergeable")
    merged = pr_data.get("merged", False)

    checks = {}
    if forge == "github":
        # Statuses
        status_url = f"{api}/repos/{repo}/commits/{head_sha}/status"
        s_code, s_data = http_request(status_url, method="GET", headers=headers)
        if s_code == 200 and isinstance(s_data, dict):
            for s in s_data.get("statuses", []):
                checks[s.get("context")] = s.get("state")

        # Check runs
        check_runs_url = f"{api}/repos/{repo}/commits/{head_sha}/check-runs"
        c_code, c_data = http_request(check_runs_url, method="GET", headers=headers)
        if c_code == 200 and isinstance(c_data, dict):
            for cr in c_data.get("check_runs", []):
                name = cr.get("name")
                c_status = cr.get("conclusion") if cr.get("status") == "completed" else cr.get("status")
                checks[name] = c_status

    elif forge == "forgejo":
        status_url = f"{api}/repos/{repo}/commits/{head_sha}/statuses"
        s_code, s_data = http_request(status_url, method="GET", headers=headers)
        if s_code == 200:
            statuses = s_data if isinstance(s_data, list) else s_data.get("statuses", [])
            for s in statuses:
                ctx = s.get("context")
                st = s.get("status") or s.get("state")
                if ctx:
                    checks[ctx] = st

    required_checks = request.get("required_checks")
    if required_checks:
        required_checks_ok = all(checks.get(rc) == "success" for rc in required_checks)
    else:
        required_checks_ok = all(st == "success" for st in checks.values()) if checks else True

    return {
        "ok": True,
        "state": state,
        "base": base_ref,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "mergeable": mergeable,
        "merged": merged,
        "checks": checks,
        "required_checks_ok": required_checks_ok,
    }


def review_post(request):
    forge, api = get_forge_and_api(request)
    repo = normalize_repo(request["repo"])
    pr = request.get("pr") or request.get("pull") or request.get("pull_number") or request.get("index")
    if pr is None:
        raise ValueError("pr is required")
    commit_id = request.get("commit_id") or request.get("head_sha") or request.get("sha")
    if not commit_id:
        raise ValueError("commit_id is required")
    body = request.get("body", "")
    event = request.get("event", "APPROVE")
    reviewer = request.get("reviewer") or request.get("account")

    token = get_token(request, forge)
    headers = build_headers(forge, token)

    # Determine marker
    marker = request.get("marker")
    review_id = None
    if not marker:
        match = re.search(r"<!--\s*privateer-review\s+([^\s>]+)\s*-->", body)
        if match:
            marker = match.group(0)
            review_id = match.group(1)

    # 1. Marker lookup across existing reviews
    reviews_url = f"{api}/repos/{repo}/pulls/{pr}/reviews"
    r_code, r_data = http_request(reviews_url, method="GET", headers=headers)
    if r_code == 200 and isinstance(r_data, list):
        for existing in r_data:
            existing_user = existing.get("user", {})
            existing_username = existing_user.get("username") or existing_user.get("login")
            if reviewer and existing_username and existing_username != reviewer:
                continue

            existing_body = existing.get("body", "")
            if marker and (marker in existing_body or (review_id and review_id in existing_body)):
                return {
                    "ok": True,
                    "posted": False,
                    "already_exists": True,
                    "review_id": existing.get("id"),
                    "id": existing.get("id"),
                    "state": existing.get("state"),
                }

    # 2. Normalize event
    event_upper = event.upper()
    if forge == "forgejo":
        if event_upper in ("APPROVE", "APPROVED"):
            target_event = "APPROVED"
        elif event_upper in ("REQUEST_CHANGES", "CHANGES_REQUESTED"):
            target_event = "REQUEST_CHANGES"
        else:
            target_event = event_upper
    else:  # github
        if event_upper in ("APPROVE", "APPROVED"):
            target_event = "APPROVE"
        elif event_upper in ("REQUEST_CHANGES", "CHANGES_REQUESTED"):
            target_event = "REQUEST_CHANGES"
        else:
            target_event = event_upper

    # 3. Post review
    payload = {
        "commit_id": commit_id,
        "event": target_event,
        "body": body,
    }
    status, post_data = http_request(reviews_url, method="POST", headers=headers, data=payload)
    if status not in (200, 201):
        err_msg = post_data.get("message") if isinstance(post_data, dict) else str(post_data)
        return {
            "ok": False,
            "posted": False,
            "error": f"failed to post review: HTTP {status} {err_msg}",
            "status_code": status,
        }

    return {
        "ok": True,
        "posted": True,
        "already_exists": False,
        "review_id": post_data.get("id"),
        "id": post_data.get("id"),
        "state": post_data.get("state"),
    }


def pr_merge(request):
    forge, api = get_forge_and_api(request)
    repo = normalize_repo(request["repo"])
    pr = request.get("pr") or request.get("pull") or request.get("pull_number") or request.get("index")
    if pr is None:
        raise ValueError("pr is required")
    head_sha = request.get("head_sha") or request.get("sha") or request.get("head_commit_id")
    if not head_sha:
        raise ValueError("head_sha is required")
    method = request.get("method") or request.get("merge_method") or "merge"
    commit_title = request.get("commit_title")
    commit_message = request.get("commit_message")

    token = get_token(request, forge)
    headers = build_headers(forge, token)

    merge_url = f"{api}/repos/{repo}/pulls/{pr}/merge"

    if forge == "github":
        payload = {
            "sha": head_sha,
            "merge_method": method,
        }
        if commit_title:
            payload["commit_title"] = commit_title
        if commit_message:
            payload["commit_message"] = commit_message
        status, res_data = http_request(merge_url, method="PUT", headers=headers, data=payload)
    else:  # forgejo
        payload = {
            "Do": method,
            "head_commit_id": head_sha,
        }
        if commit_title:
            payload["MergeTitleField"] = commit_title
        if commit_message:
            payload["MergeMessageField"] = commit_message
        status, res_data = http_request(merge_url, method="POST", headers=headers, data=payload)

    if status != 200:
        err_msg = res_data.get("message") if isinstance(res_data, dict) else str(res_data)
        return {
            "ok": False,
            "merged": False,
            "error": f"merge failed: HTTP {status} {err_msg}",
            "status_code": status,
        }

    merge_sha = res_data.get("sha")
    return {
        "ok": True,
        "merged": res_data.get("merged", True),
        "merge_sha": merge_sha,
        "sha": merge_sha,
        "message": res_data.get("message", "merged"),
    }


def protection_read(request):
    forge, api = get_forge_and_api(request)
    repo = normalize_repo(request["repo"])
    branch = request.get("branch", "main")
    reviewer = request.get("reviewer") or request.get("account")
    merger = request.get("merger")
    required_checks = request.get("required_checks", [])

    token = get_token(request, forge)
    headers = build_headers(forge, token)

    warnings = []

    if forge == "github":
        url = f"{api}/repos/{repo}/branches/{branch}/protection"
        status, data = http_request(url, method="GET", headers=headers)
        if status == 404:
            return {
                "ok": True,
                "protected": False,
                "branch": branch,
                "warnings": [f"branch '{branch}' is not protected"],
            }
        if status != 200:
            err_msg = data.get("message") if isinstance(data, dict) else str(data)
            return {
                "ok": False,
                "error": f"failed to read branch protection: HTTP {status} {err_msg}",
                "status_code": status,
            }

        enforce_admins = data.get("enforce_admins", {}).get("enabled", False)
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
        for c in status_obj.get("checks", []):
            if c.get("context") and c.get("context") not in contexts:
                contexts.append(c["context"])

        allow_force_pushes = data.get("allow_force_pushes", {}).get("enabled", False)
        allow_deletions = data.get("allow_deletions", {}).get("enabled", False)

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
            "raw": data,
        }

    else:  # forgejo
        url = f"{api}/repos/{repo}/branch_protections/{branch}"
        status, data = http_request(url, method="GET", headers=headers)
        if status == 404:
            # Fallback to listing branch protections
            list_url = f"{api}/repos/{repo}/branch_protections"
            l_status, l_data = http_request(list_url, method="GET", headers=headers)
            if l_status == 200 and isinstance(l_data, list):
                for item in l_data:
                    if item.get("branch_name") == branch or item.get("rule_name") == branch:
                        status = 200
                        data = item
                        break

        if status == 404:
            return {
                "ok": True,
                "protected": False,
                "branch": branch,
                "warnings": [f"branch '{branch}' has no branch protection"],
            }
        if status != 200:
            err_msg = data.get("message") if isinstance(data, dict) else str(data)
            return {
                "ok": False,
                "error": f"failed to read branch protection: HTTP {status} {err_msg}",
                "status_code": status,
            }

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

        if enable_push:
            warnings.append("enable_push is not false")
        if not apply_to_admins:
            warnings.append("apply_to_admins is not true")
        if required_approvals < 1:
            warnings.append("required_approvals is less than 1")
        if not enable_approvals_whitelist:
            warnings.append("enable_approvals_whitelist is not true")
        if reviewer and reviewer not in approvals_whitelist:
            warnings.append(f"reviewer '{reviewer}' not in approvals_whitelist_usernames")
        if not enable_merge_whitelist:
            warnings.append("enable_merge_whitelist is not true")
        if merger and merger not in merge_whitelist:
            warnings.append(f"merger '{merger}' not in merge_whitelist_usernames")
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
            "dismiss_stale_approvals": dismiss_stale,
            "block_on_rejected_reviews": block_on_rejected,
            "enable_status_check": enable_status_check,
            "status_checks": status_checks,
            "warnings": warnings,
            "raw": data,
        }


OPERATIONS = {
    "pr.view": pr_view,
    "review.post": review_post,
    "pr.merge": pr_merge,
    "protection.read": protection_read,
}


def handle(op, request):
    if not op or op not in OPERATIONS:
        raise ValueError(f"unsupported forge operation: {op}")
    return OPERATIONS[op](request)


def main():
    request = None
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            raise ValueError("empty request on stdin")
        request = json.loads(raw)
        if not isinstance(request, dict):
            raise ValueError("request must be a JSON object")

        op = sys.argv[1] if len(sys.argv) > 1 else request.get("op") or request.get("method") or request.get("command")
        result = handle(op, request)

        response = {
            "ok": True,
            "version": request.get("version", 1),
            **result,
        }
        if "id" in request:
            response["id"] = request["id"]
        print(json.dumps(response, separators=(",", ":")), flush=True)

    except Exception as e:
        response = {
            "ok": False,
            "version": request.get("version", 1) if isinstance(request, dict) else 1,
            "error": str(e),
        }
        if isinstance(request, dict) and "id" in request:
            response["id"] = request["id"]
        print(json.dumps(response, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
