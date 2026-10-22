#!/usr/bin/env python3
"""Unit tests for sync-milestone-plan.py; no GitHub or HackMD access."""

import copy
import importlib.util
import pathlib
import subprocess
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "sync", pathlib.Path(__file__).with_name("sync-milestone-plan.py")
)
sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync)

PLAN = {
    "schema_version": 1,
    "status": "draft",
    "draft": True,
    "dates_provisional": True,
    "repository": "owner/repo",
    "sync": sync.SAFETY_FLAGS,
    "milestones": [
        {
            "title": "1.17",
            "draft_due_on": "2026-10-30",
            "items": [{"number": 7, "kind": "issue"}],
        }
    ],
}


class FakeGh:
    def __init__(self, issue=None, milestone=None, fail_patch=False):
        self.calls, self.issue, self.fail_patch = (
            [],
            issue or {"state": "open", "milestone": None},
            fail_patch,
        )
        self.milestone = milestone or {"number": 10, "title": "1.17", "state": "open"}

    def request(self, method, endpoint, fields=None):
        self.calls.append((method, endpoint, fields))
        if endpoint == "repos/owner/repo":
            return {"full_name": "owner/repo"}
        if "milestones?" in endpoint:
            return [self.milestone]
        if method == "PATCH":
            if self.fail_patch:
                raise sync.GhError("write failed")
            self.issue["milestone"] = self.milestone
            return self.issue
        return self.issue


class TestPlan(unittest.TestCase):
    def test_validation_table(self):
        cases = [
            (
                "duplicate",
                lambda p: p["milestones"][0]["items"].append(
                    {"number": 7, "kind": "issue"}
                ),
            ),
            ("unsafe", lambda p: p["sync"].update(sync_draft_dates=True)),
            ("bad repo", lambda p: p.update(repository="--bad/repo")),
            ("bad date", lambda p: p["milestones"][0].update(draft_due_on="tomorrow")),
        ]
        for name, mutate in cases:
            with self.subTest(name):
                plan = copy.deepcopy(PLAN)
                mutate(plan)
                with self.assertRaises(sync.PlanError):
                    sync.validate_plan(plan)

    def test_markdown_ambiguity(self):
        text = "## Machine-readable milestone plan\n```json\n{}\n```\n```json\n{}\n```"
        with self.assertRaises(sync.PlanError):
            sync.extract_hackmd_plan(text)

    def test_json_duplicate_field_is_rejected(self):
        with self.assertRaises(sync.PlanError):
            sync.json_value('{"schema_version": 1, "schema_version": 1}', "test")

    def test_hackmd_credentials_must_contain_a_token(self):
        with patch.object(
            pathlib.Path, "read_text", return_value="token=\n"
        ), self.assertRaises(sync.PlanError):
            sync.hackmd_token()

    def test_dry_run_and_idempotency(self):
        gh = FakeGh()
        self.assertEqual(
            sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), False), 0
        )
        self.assertFalse(any(c[0] == "PATCH" for c in gh.calls))
        gh = FakeGh(
            issue={"state": "open", "milestone": {"number": 10, "title": "1.17"}}
        )
        self.assertEqual(
            sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), True), 0
        )
        self.assertFalse(any(c[0] == "PATCH" for c in gh.calls))

    def test_preflight_failures_do_not_write(self):
        for name, kwargs in [
            (
                "missing",
                {"milestone": {"number": 10, "title": "wrong", "state": "open"}},
            ),
            ("closed", {"issue": {"state": "closed", "milestone": None}}),
        ]:
            with self.subTest(name):
                gh = FakeGh(**kwargs)
                with self.assertRaises(sync.GhError):
                    sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), True)
                self.assertFalse(any(c[0] == "PATCH" for c in gh.calls))

    def test_apply_and_verification_failure(self):
        gh = FakeGh()
        self.assertEqual(
            sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), True), 0
        )
        self.assertTrue(any(c[0] == "PATCH" for c in gh.calls))
        gh = FakeGh(fail_patch=True)
        self.assertEqual(
            sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), True), 1
        )

        class BadReadbackGh(FakeGh):
            def request(self, method, endpoint, fields=None):
                result = super().request(method, endpoint, fields)
                if (
                    method == "GET"
                    and endpoint.endswith("/issues/7")
                    and any(c[0] == "PATCH" for c in self.calls)
                ):
                    return {"state": "open", "milestone": None}
                return result

        self.assertEqual(
            sync.execute(
                BadReadbackGh(), sync.validate_plan(copy.deepcopy(PLAN)), True
            ),
            1,
        )

    def test_apply_does_not_overwrite_stale_assignment(self):
        class StaleAssignmentGh(FakeGh):
            def __init__(self):
                super().__init__()
                self.issue_reads = 0

            def request(self, method, endpoint, fields=None):
                if method == "GET" and endpoint.endswith("/issues/7"):
                    self.issue_reads += 1
                    if self.issue_reads == 2:
                        self.issue["milestone"] = {"number": 99, "title": "other"}
                return super().request(method, endpoint, fields)

        gh = StaleAssignmentGh()
        self.assertEqual(
            sync.execute(gh, sync.validate_plan(copy.deepcopy(PLAN)), True), 1
        )
        self.assertFalse(any(call[0] == "PATCH" for call in gh.calls))

    def test_gh_client_uses_argument_list(self):
        result = subprocess.CompletedProcess([], 0, "{}", "")
        with patch.object(subprocess, "run", return_value=result) as run:
            sync.GhClient().request(
                "PATCH", "repos/owner/repo/issues/7", {"milestone": 10}
            )
        command = run.call_args.args[0]
        self.assertEqual(
            command[:5], ["gh", "api", "--method", "PATCH", "repos/owner/repo/issues/7"]
        )
        self.assertIn("milestone=10", command)


if __name__ == "__main__":
    unittest.main()
