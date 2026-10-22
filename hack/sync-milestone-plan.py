#!/usr/bin/env python3
"""Preview or apply a tightly scoped GitHub milestone plan.

The default is a read-only preview.  Pass ``--apply`` only after reviewing it.
Input is a JSON file (or stdin), or use ``--hackmd NOTE_ID_OR_URL`` to fetch a
HackMD note using the token in ~/.config/hackmd.  See ``--help`` for details.
"""

import argparse
import datetime as dt
import json
import pathlib
import re
import subprocess
import sys
import urllib.request

TIMEOUT_SECONDS = 30
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$")
NOTE_RE = re.compile(r"^(?:https://hackmd\.io/)?([A-Za-z0-9_-]+)(?:[/?#].*)?$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
PLAN_HEADING = "Machine-readable milestone plan"
SAFETY_FLAGS = {
    "scope": "milestone_assignments",
    "explicit_invocation_required": True,
    "resolve_existing_milestones_by": "title",
    "do_not_clear_unrelated_assignments": True,
    "do_not_create_close_or_reopen_issues_or_milestones": True,
    "sync_draft_dates": False,
    "draft_dates_require_separate_approval": True,
}


class PlanError(ValueError):
    """An input plan cannot safely be applied."""


class GhError(RuntimeError):
    """A GitHub CLI request failed."""


def unique_json_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise PlanError(f"duplicate JSON field {key!r}")
        value[key] = item
    return value


def response_object(value, context):
    """Validate the small portion of a GitHub response that we consume."""
    if not isinstance(value, dict):
        raise GhError(f"GitHub {context} response was not an object")
    return value


def issue_details(value, context):
    issue = response_object(value, context)
    kind = "pull_request" if "pull_request" in issue else "issue"
    milestone = issue.get("milestone")
    if milestone is None:
        return issue, kind, None, None
    if not isinstance(milestone, dict) or not is_positive_int(milestone.get("number")):
        raise GhError(f"GitHub {context} response had an invalid milestone")
    return issue, kind, milestone["number"], milestone.get("title")


def json_value(raw, source):
    try:
        value = json.loads(raw, object_pairs_hook=unique_json_object)
    except json.JSONDecodeError as e:
        raise PlanError(f"invalid JSON from {source}: {e}") from e
    if not isinstance(value, dict):
        raise PlanError("plan must be a JSON object")
    return value


def extract_hackmd_plan(markdown):
    """Return the sole json fence in the named H2 section, or fail closed."""
    heading = re.compile(rf"^##\s+{re.escape(PLAN_HEADING)}\s*$", re.MULTILINE)
    match = heading.search(markdown)
    if not match:
        raise PlanError(f"HackMD note lacks a '{PLAN_HEADING}' section")
    next_heading = re.compile(r"^#{1,2}\s+", re.MULTILINE).search(markdown, match.end())
    section = markdown[
        match.end() : next_heading.start() if next_heading else len(markdown)
    ]
    blocks = re.findall(
        r"^```json\s*\n(.*?)^```\s*$", section, re.MULTILINE | re.DOTALL
    )
    if len(blocks) != 1:
        raise PlanError("HackMD plan section must contain exactly one ```json block")
    return json_value(blocks[0], "HackMD plan block")


def hackmd_token():
    path = pathlib.Path.home() / ".config" / "hackmd"
    try:
        text = path.read_text(encoding="utf-8").strip()
    except OSError as e:
        raise PlanError(f"cannot read HackMD credentials at {path}: {e}") from e
    if not text:
        raise PlanError(f"HackMD credentials at {path} are empty")
    # Accept the documented one-token file and conventional token=VALUE files.
    for line in text.splitlines():
        key, sep, value = line.partition("=")
        if sep and key.strip().lower() in {"token", "api_token", "hackmd_token"}:
            token = value.strip()
            if token:
                return token
            raise PlanError(f"HackMD credentials at {path} contain an empty token")
    if len(text.splitlines()) != 1:
        raise PlanError(f"HackMD credentials at {path} must contain one token")
    return text


def fetch_hackmd(note):
    match = NOTE_RE.fullmatch(note)
    if not match:
        raise PlanError("--hackmd must be a HackMD note ID or https://hackmd.io/ URL")
    request = urllib.request.Request(
        f"https://api.hackmd.io/v1/notes/{match.group(1)}",
        headers={"Authorization": f"Bearer {hackmd_token()}"},
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            note_json = json.load(response)
    except Exception as e:
        raise PlanError(f"could not fetch HackMD note: {e}") from e
    if not isinstance(note_json, dict):
        raise PlanError("HackMD response was not an object")
    content = note_json.get("content")
    if not isinstance(content, str):
        raise PlanError("HackMD response has no textual content")
    return extract_hackmd_plan(content)


def require_keys(value, keys, context):
    if set(value) != set(keys):
        raise PlanError(f"{context} has unexpected or missing fields")


def is_positive_int(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def validate_plan(plan):
    require_keys(
        plan,
        {
            "schema_version",
            "status",
            "draft",
            "dates_provisional",
            "repository",
            "sync",
            "milestones",
        },
        "plan",
    )
    if (
        plan["schema_version"] != 1
        or plan["status"] != "draft"
        or plan["draft"] is not True
        or plan["dates_provisional"] is not True
    ):
        raise PlanError("plan must be the schema version 1 provisional draft")
    if not isinstance(plan["repository"], str) or not REPOSITORY_RE.fullmatch(
        plan["repository"]
    ):
        raise PlanError("repository must be a valid owner/name")
    if not isinstance(plan["sync"], dict) or plan["sync"] != SAFETY_FLAGS:
        raise PlanError(
            "plan safety flags do not exactly permit milestone assignment only"
        )
    milestones = plan["milestones"]
    if not isinstance(milestones, list) or not milestones:
        raise PlanError("milestones must be a non-empty list")
    titles, numbers = set(), set()
    for milestone in milestones:
        if not isinstance(milestone, dict):
            raise PlanError("each milestone must be an object")
        require_keys(milestone, {"title", "draft_due_on", "items"}, "milestone")
        title, due = milestone["title"], milestone["draft_due_on"]
        if not isinstance(title, str) or not title.strip() or title in titles:
            raise PlanError("milestone titles must be non-empty and unique")
        try:
            if not isinstance(due, str) or not DATE_RE.fullmatch(due):
                raise ValueError
            dt.date.fromisoformat(due)
        except (TypeError, ValueError) as e:
            raise PlanError(f"milestone {title!r} has invalid draft_due_on") from e
        if not isinstance(milestone["items"], list) or not milestone["items"]:
            raise PlanError(f"milestone {title!r} has no items")
        titles.add(title)
        for item in milestone["items"]:
            if not isinstance(item, dict):
                raise PlanError("each item must be an object")
            require_keys(item, {"number", "kind"}, "item")
            if not is_positive_int(item["number"]) or item["kind"] not in {
                "issue",
                "pull_request",
            }:
                raise PlanError(
                    "items need a positive number and issue or pull_request kind"
                )
            if item["number"] in numbers:
                raise PlanError(f"issue/PR #{item['number']} is listed more than once")
            numbers.add(item["number"])
    return plan


class GhClient:
    def request(self, method, endpoint, fields=None):
        command = ["gh", "api", "--method", method, endpoint]
        for key, value in (fields or {}).items():
            command.extend(["-f", f"{key}={value}"])
        try:
            result = subprocess.run(
                command,
                check=True,
                capture_output=True,
                text=True,
                timeout=TIMEOUT_SECONDS,
            )
        except (OSError, subprocess.SubprocessError) as e:
            raise GhError(f"gh api {method} {endpoint} failed: {e}") from e
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as e:
            raise GhError(f"gh api {method} {endpoint} returned invalid JSON") from e


def milestones_by_title(gh, repo):
    page, found = 1, []
    while True:
        batch = gh.request(
            "GET", f"repos/{repo}/milestones?state=all&per_page=100&page={page}"
        )
        if not isinstance(batch, list):
            raise GhError("GitHub milestones response was not a list")
        if not all(isinstance(milestone, dict) for milestone in batch):
            raise GhError("GitHub milestones response contained a non-object")
        found.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    return found


def preflight(gh, plan):
    repo = plan["repository"]
    repository = response_object(gh.request("GET", f"repos/{repo}"), "repository")
    if repository.get("full_name") != repo:
        raise GhError(
            f"GitHub resolved repository to {repository.get('full_name')!r}, not {repo!r}"
        )
    all_milestones = milestones_by_title(gh, repo)
    resolved = {}
    for entry in plan["milestones"]:
        matches = [m for m in all_milestones if m.get("title") == entry["title"]]
        if (
            len(matches) != 1
            or matches[0].get("state") != "open"
            or not is_positive_int(matches[0].get("number"))
        ):
            raise GhError(
                f"milestone {entry['title']!r} must resolve to exactly one open existing milestone"
            )
        resolved[entry["title"]] = matches[0]
    changes = []
    for entry in plan["milestones"]:
        target = resolved[entry["title"]]
        for item in entry["items"]:
            issue, actual_kind, old_number, old_title = issue_details(
                gh.request("GET", f"repos/{repo}/issues/{item['number']}"),
                f"issue #{item['number']}",
            )
            if issue.get("state") != "open" or actual_kind != item["kind"]:
                raise GhError(f"#{item['number']} is not an open {item['kind']}")
            changes.append((item["number"], target, old_number, old_title, item["kind"]))
    return changes


def execute(gh, plan, apply):
    changes = preflight(gh, plan)
    todo = [change for change in changes if change[2] != change[1]["number"]]
    for number, target, old_number, old_title, _kind in changes:
        old = f"#{old_number} ({old_title})" if old_number else "none"
        print(f"#{number}: {old} -> #{target['number']} ({target['title']})")
    if not todo:
        print("All listed items already match; no writes needed.")
        return 0
    if not apply:
        print(
            f"Dry run: {len(todo)} assignment(s) would be updated; use --apply to write."
        )
        return 0
    failures = []
    for number, target, old_number, _old_title, expected_kind in todo:
        try:
            # Do not overwrite an assignment changed after the all-read preflight.
            issue, actual_kind, current_number, _current_title = issue_details(
                gh.request("GET", f"repos/{plan['repository']}/issues/{number}"),
                f"issue #{number}",
            )
            if issue.get("state") != "open" or actual_kind != expected_kind:
                raise GhError(f"#{number} is no longer an open {expected_kind}")
            if current_number != old_number:
                raise GhError(f"#{number} milestone changed after preflight")
            response = gh.request(
                "PATCH",
                f"repos/{plan['repository']}/issues/{number}",
                {"milestone": target["number"]},
            )
            _, _kind, response_number, _title = issue_details(
                response, f"PATCH issue #{number}"
            )
            if response_number != target["number"]:
                raise GhError("PATCH response did not contain the requested milestone")
            _, _kind, readback_number, _title = issue_details(
                gh.request("GET", f"repos/{plan['repository']}/issues/{number}"),
                f"issue #{number} readback",
            )
            if readback_number != target["number"]:
                raise GhError("readback did not contain the requested milestone")
            print(f"Applied #{number} -> {target['title']}")
        except GhError as e:
            failures.append(f"#{number}: {e}")
    if failures:
        print("Partial apply: " + "; ".join(failures), file=sys.stderr)
        return 1
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Safely preview or assign existing GitHub milestones from a JSON plan."
    )
    parser.add_argument(
        "plan", nargs="?", help="pure JSON plan file; omit to read stdin"
    )
    parser.add_argument(
        "--hackmd", metavar="NOTE", help="fetch plan from a HackMD note ID or URL"
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="perform assignments after successful preflight (default: dry run)",
    )
    args = parser.parse_args(argv)
    if args.plan and args.hackmd:
        parser.error("PLAN and --hackmd cannot be used together")
    try:
        if args.hackmd:
            plan = fetch_hackmd(args.hackmd)
        else:
            raw = (
                pathlib.Path(args.plan).read_text(encoding="utf-8")
                if args.plan
                else sys.stdin.read()
            )
            plan = json_value(raw, args.plan or "stdin")
        return execute(GhClient(), validate_plan(plan), args.apply)
    except (PlanError, GhError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
