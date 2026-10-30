#!/usr/bin/env python3
"""Native permission plans; fixtures only, no accounts, providers or settings writes."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "plugins"))
from native import build, handle
from tool_permissions import attended_argv, policy


class Permissions(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-permission-plans-")
        self.root = Path(self.temp.name).resolve()
        self.policy = {"mode":"yolo", "claude_allow":[]}

    def tearDown(self):
        self.temp.cleanup()

    def request(self, **options):
        return {"method":"command", "context":{"repo":str(self.root), "title":"literal $(touch NEVER)",
            "tool_permissions":self.policy, "control":{"cli_argv":["/bin/privateer","--home",str(self.root)],"writable":[]}}, "options":options}

    def test_explicit_yolo_translations_and_pinned_model(self):
        cases = {"claude":"bypassPermissions", "codex":"--dangerously-bypass-approvals-and-sandbox",
                 "omp":"--approval-mode", "gemini":"yolo", "cursor":"--force", "agy":"--dangerously-skip-permissions",
                 "muse":"never", "devin":"dangerous", "opencode-v2":"--auto"}
        for harness, expected in cases.items():
            with self.subTest(harness=harness):
                _, argv = build(harness, self.request(model="selected-model"))
                text=json.dumps(argv)
                self.assertIn(expected,text)
                self.assertIn("selected-model",text)
                self.assertIn("literal $(touch NEVER)",text)
        _, argv=build("codex",self.request(sandbox="workspace-write"))
        self.assertNotIn("--sandbox",argv)
        self.assertFalse((self.root/"NEVER").exists())

    def test_yolo_keeps_gateway_pi_provider_selection(self):
        _, argv=build("pi",self.request(provider="private-gateway",model="selected-glm"))
        self.assertEqual(argv[argv.index("--provider")+1],"private-gateway")
        self.assertEqual(argv[argv.index("--model")+1],"selected-glm")

    def test_acp_and_msp_approve_offered_tools_without_changing_transport(self):
        for harness in ["rovo","grok","kimi","devin"]:
            _, argv=build(harness,self.request(transport="acp"))
            payload=json.loads(argv[-1])
            self.assertEqual(payload["permissions"],"auto")
        _, argv=build("muse",self.request(transport="msp"))
        payload=json.loads(argv[-1]);self.assertEqual(payload["permissions"],"allow_once")
        self.assertEqual(payload["approval_mode"],"allowAll")

    def test_standard_defaults_and_custom_rules_do_not_grant_yolo(self):
        self.policy={"mode":"standard","claude_allow":["Bash(forge-helper *)"]}
        _, argv=build("claude",self.request())
        for rule in ["Bash(/bin/privateer:*)","Bash(git *)","Bash(gh *)","Bash(forge-helper *)"]:
            self.assertIn(rule,argv)
        self.assertNotIn("bypassPermissions",argv)
        _, argv=build("claude",self.request(permission_mode="plan"))
        self.assertIn("plan",argv)

    def test_project_rules_apply_only_to_claude_crew_and_first_mates_not_reviews_or_errands(self):
        self.policy = {"mode": "standard", "claude_allow": []}
        for role in ["crew", "first_mate"]:
            request = self.request()
            request["context"]["role"] = role
            before = build("codex", request)[1]
            self.policy["project_allow"] = ["Bash(cargo test *)"]
            _, argv = build("claude", request)
            self.assertIn("Bash(cargo test *)", argv)
            self.assertEqual(before, build("codex", request)[1])
            request["context"]["errand"] = True
            self.assertNotIn("Bash(cargo test *)", build("claude", request)[1])
            request["context"]["role"] = "reviewer"
            self.assertNotIn("Bash(cargo test *)", build("claude", request)[1])
            self.policy.pop("project_allow")
        self.policy["project_allow"] = ["Bash(cargo test *)"]
        argv = attended_argv("claude", ["claude"], {**self.policy, "role": "first_mate"})
        self.assertIn("Bash(cargo test *)", argv)
        for invalid in [["Bash(*)"], ["Bash(npm install *)"], [{}], "Bash(cargo test *)"]:
            with self.assertRaises(ValueError):
                policy({**self.policy, "project_allow": invalid})

    def test_claude_reaches_the_captains_browser_only_when_its_route_asks(self):
        _, argv=build("claude",self.request())
        self.assertIn("--no-chrome",argv); self.assertNotIn("--chrome",argv)
        _, argv=build("claude",self.request(chrome=True))
        self.assertIn("--chrome",argv); self.assertNotIn("--no-chrome",argv)

    def test_attended_yolo_overrides_only_permission_selectors_before_prompt(self):
        argv=attended_argv("claude",["claude","--model","chosen","--permission-mode=plan","--","--permission-mode=manual"],self.policy)
        self.assertIn("bypassPermissions",argv)
        self.assertNotIn("--permission-mode=plan",argv)
        self.assertEqual(argv[-2:],["--","--permission-mode=manual"])
        self.assertIn("chosen",argv)
        agy=attended_argv("agy",["agy","--sandbox","--model","chosen"],self.policy)
        self.assertIn("--dangerously-skip-permissions",agy)
        self.assertEqual(agy[-2:],["--model","chosen"])
        with self.assertRaises(ValueError):attended_argv("custom-unqualified",["custom-unqualified"],self.policy)

    def test_opencode_v1_override_is_local_to_the_process(self):
        result=handle("opencode",self.request())
        self.assertIn("--opencode-yolo",result["argv"])
        fake=self.root/"opencode"
        fake.write_text("#!/usr/bin/env python3\nimport os,json\nprint(json.dumps({'permission':os.environ.get('OPENCODE_PERMISSION'),'route':os.environ.get('TEST_MODEL_ROUTE')}))\n")
        fake.chmod(0o700)
        proc=subprocess.run([sys.executable,str(ROOT/"plugins/native_launch.py"),"--opencode-yolo","opencode","opencode"],
            capture_output=True,text=True,timeout=5,env={**os.environ,"PATH":str(self.root)+os.pathsep+os.environ['PATH'],"TEST_MODEL_ROUTE":"pinned"})
        self.assertEqual(proc.returncode,0,proc.stderr)
        self.assertEqual(json.loads(proc.stdout),{"permission":'{"*":"allow"}',"route":"pinned"})

    def test_charter_actions_derive_forge_rules_for_first_mate(self):
        self.policy["forges"] = {
            "forgejo": {
                "publish_pr": "fakeforge prs create *",
                "merge_green": "fakeforge prs merge *",
            }
        }
        req = self.request()
        req["context"]["role"] = "first_mate"
        req["context"]["actions"] = ["inspect", "implement", "test", "publish_pr", "merge_green"]
        req["context"]["forge"] = "forgejo"
        _, argv = build("claude", req)
        self.assertIn("Bash(fakeforge prs merge *)", argv)
        self.assertIn("Bash(fakeforge prs create *)", argv)
        settings_idx = argv.index("--settings")
        settings = json.loads(argv[settings_idx + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertEqual(settings["autoMode"]["environment"][0], "$defaults")
        self.assertTrue(any("merge_green" in r for r in settings["autoMode"]["allow"]))
        self.assertTrue(any("publish_pr" in r for r in settings["autoMode"]["allow"]))

        # GitHub forge
        req["context"]["forge"] = "github"
        _, argv = build("claude", req)
        self.assertIn("Bash(gh pr merge *)", argv)
        self.assertIn("Bash(gh pr create *)", argv)

        # Ship without merge_green
        req["context"]["actions"] = ["inspect", "implement", "test", "publish_pr"]
        req["context"]["forge"] = "forgejo"
        _, argv = build("claude", req)
        self.assertNotIn("Bash(fakeforge prs merge *)", argv)
        self.assertIn("Bash(fakeforge prs create *)", argv)
        settings_idx = argv.index("--settings")
        settings = json.loads(argv[settings_idx + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertFalse(any("merge_green" in r for r in settings["autoMode"]["allow"]))
        self.assertTrue(any("publish_pr" in r for r in settings["autoMode"]["allow"]))

    def test_crew_and_errands_never_receive_charter_rules_or_merge_rules(self):
        self.policy["forges"] = {
            "forgejo": {
                "publish_pr": "fakeforge prs create *",
                "merge_green": "fakeforge prs merge *",
            }
        }
        req = self.request()
        req["context"]["role"] = "crew"
        req["context"]["actions"] = ["inspect", "implement", "test", "publish_pr", "merge_green"]
        req["context"]["forge"] = "forgejo"
        _, argv = build("claude", req)
        self.assertNotIn("Bash(fakeforge prs merge *)", argv)
        self.assertNotIn("Bash(fakeforge prs create *)", argv)
        self.assertNotIn("Bash(gh pr merge *)", argv)
        self.assertOnlyRuntimeAutoMode(argv)

        # Errands also never receive merge or publish rules
        req["context"]["role"] = "first_mate"
        req["context"]["errand"] = True
        _, argv = build("claude", req)
        self.assertNotIn("Bash(fakeforge prs merge *)", argv)
        self.assertNotIn("Bash(fakeforge prs create *)", argv)
        self.assertNotIn("Bash(gh pr merge *)", argv)
        self.assertOnlyRuntimeAutoMode(argv)

    def assertOnlyRuntimeAutoMode(self, argv):
        """Crew and errands learn the Privateer CLI is their own runtime, and nothing the captain granted."""
        auto = json.loads(argv[argv.index("--settings") + 1])["autoMode"]
        self.assertEqual(len(auto["allow"]), 2, auto)
        self.assertIn("Privateer CLI (/bin/privateer)", auto["allow"][1])
        self.assertIn("cancelling", auto["allow"][1])
        self.assertFalse(any("charter" in e.lower() for e in auto["environment"]), auto)

    def test_an_errand_may_manage_its_own_jobs_but_never_carries_captain_grants(self):
        grant = "Making storefront's own Apify actors public in the Apify Store."
        self.policy = {"mode":"standard", "claude_allow":[], "authorized":{"storefront":[grant]},
                       "approved":[self.APPROVED]}
        for role, errand in [("crew", False), ("first_mate", True)]:
            with self.subTest(role=role, errand=errand):
                req = self.request(permission_mode="auto")
                req["context"].update(owner="storefront", role=role, errand=errand, actions=["inspect"])
                _, argv = build("claude", req)
                self.assertOnlyRuntimeAutoMode(argv)
        # A reviewer has no shell, so nothing to tell its classifier.
        req = self.request(permission_mode="auto")
        req["context"].update(owner="storefront", role="reviewer", actions=["inspect"])
        _, argv = build("claude", req)
        self.assertNotIn("--settings", argv)

    def test_publish_review_is_the_runtimes_command_not_an_agent_rule(self):
        # publish_review names the command the runtime posts a review's
        # comment with; it is never the agent's own tool rule.
        self.policy["forges"] = {"forgejo": {"publish_pr": "fakeforge prs create *",
                                             "publish_review": "fakeforge-comment",
                                             "read_pr": "fakeforge-read",
                                             "merge_pr": "fakeforge-merge"}}
        req = self.request()
        req["context"]["role"] = "first_mate"
        req["context"]["actions"] = ["inspect", "publish_pr", "publish_review",
                                     "read_pr", "merge_pr"]
        req["context"]["forge"] = "forgejo"
        _, argv = build("claude", req)
        self.assertIn("Bash(fakeforge prs create *)", argv)
        for command in ("fakeforge-comment", "fakeforge-read", "fakeforge-merge"):
            self.assertFalse(any(command in a for a in argv), argv)

    def test_attended_launch_carries_charter_rules_for_merge_green(self):
        policy_data = dict(self.policy)
        policy_data["actions"] = ["inspect", "publish_pr", "merge_green"]
        policy_data["forge"] = "forgejo"
        policy_data["forges"] = {
            "forgejo": {
                "publish_pr": "fakeforge prs create *",
                "merge_green": "fakeforge prs merge *",
            }
        }
        argv = attended_argv("claude", ["claude"], policy_data)
        self.assertIn("Bash(fakeforge prs merge *)", argv)
        self.assertIn("Bash(fakeforge prs create *)", argv)
        settings_idx = argv.index("--settings")
        settings = json.loads(argv[settings_idx + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertEqual(settings["autoMode"]["environment"][0], "$defaults")
        self.assertTrue(any("merge_green" in r for r in settings["autoMode"]["allow"]))

        # Without merge_green
        policy_data["actions"] = ["inspect", "publish_pr"]
        argv = attended_argv("claude", ["claude"], policy_data)
        self.assertNotIn("Bash(fakeforge prs merge *)", argv)
        self.assertIn("Bash(fakeforge prs create *)", argv)
        settings_idx = argv.index("--settings")
        settings = json.loads(argv[settings_idx + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertFalse(any("merge_green" in r for r in settings["autoMode"]["allow"]))

    def test_preexisting_settings_merges_charter_auto_mode(self):
        policy_data = dict(self.policy)
        policy_data["actions"] = ["inspect", "publish_pr", "merge_green"]
        policy_data["forge"] = "forgejo"
        policy_data["forges"] = {
            "forgejo": {
                "publish_pr": "fakeforge prs create *",
                "merge_green": "fakeforge prs merge *",
            }
        }
        user_settings = {"autoMode": {"allow": ["$defaults", "Bash(user_cmd *)"], "environment": ["$defaults", "user_env"]}}
        argv = attended_argv("claude", ["claude", "--settings", json.dumps(user_settings)], policy_data)
        self.assertEqual(argv.count("--settings"), 1)
        settings = json.loads(argv[argv.index("--settings") + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertIn("Bash(user_cmd *)", settings["autoMode"]["allow"])
        self.assertTrue(any("merge_green" in r for r in settings["autoMode"]["allow"]))
        self.assertEqual(settings["autoMode"]["environment"][0], "$defaults")
        self.assertIn("user_env", settings["autoMode"]["environment"])

    def test_a_ships_standing_authorizations_reach_only_its_first_mates_classifier(self):
        grant = "Making storefront's own Apify actors public in the Apify Store."
        self.policy = {"mode":"standard", "claude_allow":[], "authorized":{"storefront":[grant]}}
        def allow(owner, role):
            req = self.request(permission_mode="auto")
            req["context"].update(owner=owner, role=role, actions=["inspect"])
            _, argv = build("claude", req)
            if "--settings" not in argv:
                return []
            return json.loads(argv[argv.index("--settings") + 1])["autoMode"]["allow"]
        self.assertIn(f"The captain has authorized this: {grant}", allow("storefront", "first_mate"))
        self.assertFalse(any(grant in a for a in allow("bizdev", "first_mate")))
        self.assertFalse(any(grant in a for a in allow("storefront", "crew")))

    APPROVED = ("The captain approved this in reply r-1 to request q-1, for ship storefront only, "
                "on task ship-55, until 2026-10-22 12:00 UTC: Submit build 55 to TestFlight")

    def test_the_captains_approvals_reach_only_a_first_mates_allow(self):
        self.policy = {"mode":"standard", "claude_allow":[], "approved":[self.APPROVED]}
        def settings(role, errand=False, actions=None):
            req = self.request(permission_mode="auto")
            req["context"].update(owner="storefront", role=role, errand=errand, actions=actions)
            _, argv = build("claude", req)
            return json.loads(argv[argv.index("--settings") + 1]) if "--settings" in argv else None
        # An approval alone, with no charter actions, still reaches the wake.
        wake = settings("first_mate")
        self.assertIn(self.APPROVED, wake["autoMode"]["allow"])
        self.assertFalse(any(self.APPROVED in e for e in wake["autoMode"]["environment"]))
        self.assertNotIn("deny", wake["autoMode"])
        self.assertIn(self.APPROVED, settings("first_mate", actions=["inspect"])["autoMode"]["allow"])
        for role, errand in [("crew", False), ("reviewer", False), ("first_mate", True)]:
            with self.subTest(role=role, errand=errand):
                found = settings(role, errand=errand, actions=["inspect"])
                self.assertFalse(found and any(self.APPROVED in a for a in found["autoMode"]["allow"]))

    def test_attended_argv_carries_the_captains_approvals(self):
        value = {"mode":"standard", "claude_allow":[], "ship":"storefront", "approved":[self.APPROVED]}
        argv = attended_argv("claude", ["claude"], value)
        settings = json.loads(argv[argv.index("--settings") + 1])
        self.assertEqual(settings["autoMode"]["allow"][0], "$defaults")
        self.assertIn(self.APPROVED, settings["autoMode"]["allow"])
        self.assertFalse(any(self.APPROVED in e for e in settings["autoMode"]["environment"]))
        # No approval and no actions: only the runtime's own sentence.
        argv = attended_argv("claude", ["claude"], {"mode":"standard", "claude_allow":[], "approved":[]})
        allow = json.loads(argv[argv.index("--settings") + 1])["autoMode"]["allow"]
        self.assertEqual(len(allow), 2)
        self.assertIn("Privateer CLI", allow[1])

    def test_approvals_of_the_wrong_type_are_refused(self):
        for approved in ["one", {"a": "b"}, 1]:
            with self.subTest(approved=approved):
                with self.assertRaises(ValueError):
                    attended_argv("claude", ["claude"], {"mode":"standard", "claude_allow":[], "approved":approved})

    def test_an_invalid_approval_is_dropped_and_the_launch_still_builds(self):
        for bad in [1, "", "x" * 601]:
            with self.subTest(bad=bad):
                value = {"mode":"standard", "claude_allow":[], "ship":"storefront", "approved":[bad, self.APPROVED]}
                with contextlib.redirect_stderr(io.StringIO()) as err:
                    argv = attended_argv("claude", ["claude"], value)
                self.assertIn("dropped 1 invalid captain approval", err.getvalue())
                allow = json.loads(argv[argv.index("--settings") + 1])["autoMode"]["allow"]
                self.assertIn(self.APPROVED, allow)
                self.assertFalse(any(a in (bad, "") for a in allow))
        with contextlib.redirect_stderr(io.StringIO()):
            kept = policy({"mode":"standard", "claude_allow":[], "approved":[f"a{i}" for i in range(9)]})
        self.assertEqual(kept["approved"], [f"a{i}" for i in range(8)])

    def test_reviewer_role_restricts_supported_harnesses(self):
        req = self.request()
        req["context"]["role"] = "reviewer"
        for harness in ["claude", "pi", "pi-signed", "codex"]:
            with self.subTest(harness=harness):
                _, argv = build(harness, req)
                self.assertTrue(len(argv) > 0)
        unsupported = ["gemini", "cursor", "devin", "grok", "kimi", "muse", "omp", "opencode", "opencode-v2", "rovo"]
        for harness in unsupported:
            with self.subTest(harness=harness):
                with self.assertRaises(ValueError) as ctx:
                    build(harness, req)
                self.assertEqual(str(ctx.exception), "reviewer role supports only claude, pi and codex")

    def test_reviewer_role_refuses_session_resume(self):
        for harness in ["claude", "pi", "pi-signed", "codex"]:
            with self.subTest(harness=harness):
                req = self.request(resume="4fa4f4bc-5b2d-4553-8ae1-eb09e501ea63")
                req["context"]["role"] = "reviewer"
                with self.assertRaises(ValueError) as ctx:
                    build(harness, req)
                self.assertEqual(str(ctx.exception), "review requires a fresh session")

    def test_reviewer_role_refuses_interactive_mode(self):
        for harness in ["claude", "pi", "pi-signed", "codex"]:
            with self.subTest(harness=harness):
                req = self.request(mode="interactive")
                req["context"]["role"] = "reviewer"
                with self.assertRaises(ValueError) as ctx:
                    build(harness, req)
                self.assertEqual(str(ctx.exception), "reviewer requires a headless run")
                with self.assertRaises(ValueError) as ctx:
                    handle(harness, req)
                self.assertEqual(str(ctx.exception), "reviewer requires a headless run")

                req_default = self.request()
                req_default["context"]["role"] = "reviewer"
                _, argv_default = build(harness, req_default)
                req_headless = self.request(mode="headless")
                req_headless["context"]["role"] = "reviewer"
                _, argv_headless = build(harness, req_headless)
                self.assertEqual(argv_headless, argv_default)

    def test_claude_reviewer_argv_has_no_bash_no_write_no_control_cli_and_no_resume(self):
        req = self.request()
        req["context"]["role"] = "reviewer"
        req["context"]["actions"] = ["inspect", "implement", "test", "publish_pr", "merge_green"]
        req["context"]["forge"] = "github"
        _, argv = build("claude", req)

        # Permission mode is dontAsk
        self.assertIn("--permission-mode", argv)
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")

        # Allowed tools: strictly Read, Grep, Glob
        self.assertIn("--allowedTools", argv)
        allowed_idx = argv.index("--allowedTools")
        disallowed_idx = argv.index("--disallowedTools")
        allowed = argv[allowed_idx + 1:disallowed_idx]
        self.assertEqual(allowed, ["Read", "Grep", "Glob"])

        # Disallowed tools: includes Bash, Edit, Write, NotebookEdit, WebFetch, WebSearch, CronCreate, ScheduleWakeup, RemoteTrigger, Agent, Task, Workflow, EnterWorktree
        end_idx = argv.index("--")
        disallowed = argv[disallowed_idx + 1:end_idx]
        for tool in ["Bash", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch",
                     "CronCreate", "ScheduleWakeup", "RemoteTrigger", "Agent", "Task", "Workflow", "EnterWorktree"]:
            self.assertIn(tool, disallowed)

        # No Bash, no Write, no Edit in allowed tools
        for tool in allowed:
            self.assertNotIn("Bash", tool)
            self.assertNotIn("Write", tool)
            self.assertNotIn("Edit", tool)

        # No control CLI in argv
        self.assertNotIn("/bin/privateer", argv)
        for arg in argv:
            self.assertNotIn("privateer", arg)

        # No resume in argv
        self.assertNotIn("--resume", argv)

        # No network / chrome
        self.assertIn("--no-chrome", argv)
        self.assertNotIn("--chrome", argv)

        # Built-in tools, setting sources, MCP config, and safe mode
        self.assertIn("--tools", argv)
        self.assertEqual(argv[argv.index("--tools") + 1], "Read,Grep,Glob")
        self.assertIn("--setting-sources", argv)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "user")
        self.assertIn("--strict-mcp-config", argv)
        self.assertIn("--safe-mode", argv)

        # No settings or add-dir for operational home
        self.assertNotIn("--settings", argv)
        self.assertNotIn("--add-dir", argv)

        # Even with fleet yolo policy, reviewer permissions are unaffected
        self.policy = {"mode": "yolo", "claude_allow": ["Bash(something *)"]}
        req = self.request()
        req["context"]["role"] = "reviewer"
        _, argv_yolo = build("claude", req)
        self.assertIn("dontAsk", argv_yolo)
        self.assertNotIn("bypassPermissions", argv_yolo)
        self.assertNotIn("Bash(something *)", argv_yolo)
        self.assertIn("--tools", argv_yolo)
        self.assertEqual(argv_yolo[argv_yolo.index("--tools") + 1], "Read,Grep,Glob")
        self.assertIn("--setting-sources", argv_yolo)
        self.assertEqual(argv_yolo[argv_yolo.index("--setting-sources") + 1], "user")
        self.assertIn("--strict-mcp-config", argv_yolo)
        self.assertIn("--safe-mode", argv_yolo)
        allowed_yolo = argv_yolo[argv_yolo.index("--allowedTools") + 1:argv_yolo.index("--disallowedTools")]
        self.assertEqual(allowed_yolo, ["Read", "Grep", "Glob"])

    def test_pi_reviewer_argv_has_no_bash_no_write_no_control_cli_and_no_resume(self):
        for harness in ["pi", "pi-signed"]:
            with self.subTest(harness=harness):
                req = self.request()
                req["context"]["role"] = "reviewer"
                _, argv = build(harness, req)

                # Bare flags present
                for bare_flag in ["--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
                                  "--no-context-files", "--no-themes"]:
                    self.assertIn(bare_flag, argv)

                # Tools flag: read-only tools
                self.assertIn("--tools", argv)
                tools = argv[argv.index("--tools") + 1]
                self.assertEqual(tools, "read,grep,find,ls")
                self.assertNotIn("bash", tools)
                self.assertNotIn("write", tools)
                self.assertNotIn("edit", tools)

                # Ephemeral flag
                self.assertIn("--no-session", argv)

                # No control CLI or resume
                self.assertNotIn("/bin/privateer", argv)
                self.assertNotIn("--resume", argv)
                self.assertNotIn("--session", argv)

                # Even with fleet yolo policy
                self.policy = {"mode": "yolo"}
                req_yolo = self.request()
                req_yolo["context"]["role"] = "reviewer"
                _, argv_yolo = build(harness, req_yolo)
                self.assertIn("--no-session", argv_yolo)
                self.assertIn("read,grep,find,ls", argv_yolo)

    def test_codex_reviewer_argv_has_no_bash_no_write_no_control_cli_and_no_resume(self):
        req = self.request()
        req["context"]["role"] = "reviewer"
        _, argv = build("codex", req)

        # Sandbox is read-only
        self.assertIn("--sandbox", argv)
        self.assertEqual(argv[argv.index("--sandbox") + 1], "read-only")

        # Disable web search, hooks, and ignore user config / rules
        self.assertIn("--ignore-user-config", argv)
        self.assertIn("--ignore-rules", argv)
        self.assertIn("--disable", argv)
        self.assertEqual(argv[argv.index("--disable") + 1], "hooks")
        self.assertIn('web_search="disabled"', argv)
        ws_idx = argv.index('web_search="disabled"')
        self.assertEqual(argv[ws_idx - 1], "-c")

        # No bypass, no add-dir, no control CLI, no resume
        self.assertNotIn("--dangerously-bypass-approvals-and-sandbox", argv)
        self.assertNotIn("--add-dir", argv)
        self.assertNotIn("/bin/privateer", argv)
        self.assertNotIn("resume", argv)

        # Even with explicit yolo or sandbox options or fleet yolo policy
        self.policy = {"mode": "yolo"}
        req_yolo = self.request(sandbox="danger-full-access", yolo=True)
        req_yolo["context"]["role"] = "reviewer"
        _, argv_yolo = build("codex", req_yolo)
        self.assertIn("--sandbox", argv_yolo)
        self.assertEqual(argv_yolo[argv_yolo.index("--sandbox") + 1], "read-only")
        self.assertIn("--ignore-user-config", argv_yolo)
        self.assertIn("--ignore-rules", argv_yolo)
        self.assertIn("--disable", argv_yolo)
        self.assertEqual(argv_yolo[argv_yolo.index("--disable") + 1], "hooks")
        self.assertIn('web_search="disabled"', argv_yolo)
        ws_idx_yolo = argv_yolo.index('web_search="disabled"')
        self.assertEqual(argv_yolo[ws_idx_yolo - 1], "-c")
        self.assertNotIn("--dangerously-bypass-approvals-and-sandbox", argv_yolo)
        self.assertNotIn("--add-dir", argv_yolo)
        self.assertNotIn("/bin/privateer", argv_yolo)

    def test_codex_route_web_search_option_emits_exec_config_and_reviewers_stay_disabled(self):
        session = "46a81234-1234-4567-8123-123456789abc"
        for value in ["live", "cached", "disabled"]:
            with self.subTest(value=value):
                flag = "web_search=" + json.dumps(value)
                _, argv = build("codex", self.request(web_search=value))
                self.assertIn(flag, argv)
                idx = argv.index(flag)
                self.assertEqual(argv[idx - 1], "-c")
                self.assertLess(argv.index("exec"), idx)
                _, argv = build("codex", self.request(web_search=value, resume=session))
                idx = argv.index(flag)
                self.assertEqual(argv[idx - 1], "-c")
                self.assertLess(idx, argv.index("resume"))
                self.assertEqual(argv[argv.index("resume") + 1], session)

        # Absent option: nothing emitted, so native config applies
        _, argv = build("codex", self.request())
        self.assertEqual([a for a in argv if a.startswith("web_search=")], [])

        # Only the three selector values are accepted
        for invalid in ["always", "Live", True]:
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValueError):
                    build("codex", self.request(web_search=invalid))

        # A reviewer stays disabled whatever the route asks
        for policy_mode in ["standard", "yolo"]:
            with self.subTest(policy_mode=policy_mode):
                self.policy = {"mode": policy_mode, "claude_allow": []}
                req = self.request(web_search="live", sandbox="danger-full-access")
                req["context"]["role"] = "reviewer"
                _, argv = build("codex", req)
                flags = [a for a in argv if a.startswith("web_search=")]
                self.assertEqual(flags, ['web_search="disabled"'])
                self.assertEqual(argv[argv.index(flags[0]) - 1], "-c")

    def test_reviewer_refuses_tool_bundles_for_claude_and_pi(self):
        for harness in ["claude", "pi"]:
            with self.subTest(harness=harness):
                req = self.request()
                req["context"]["role"] = "reviewer"
                req["context"]["tool_bundles"] = {"skills": [], "mcp_servers": []}
                with self.assertRaises(ValueError) as ctx:
                    handle(harness, req)
                self.assertEqual(str(ctx.exception), "reviewer does not support tool bundles")

    def test_claude_reviewer_route_options_bypass_and_chrome_yield_dontask_and_no_chrome(self):
        for policy_mode in ["standard", "yolo"]:
            with self.subTest(policy_mode=policy_mode):
                self.policy = {"mode": policy_mode, "claude_allow": []}
                req = self.request(permission_mode="bypassPermissions", chrome=True)
                req["context"]["role"] = "reviewer"
                _, argv = build("claude", req)
                self.assertIn("--permission-mode", argv)
                self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")
                self.assertNotIn("bypassPermissions", argv)
                self.assertIn("--no-chrome", argv)
                self.assertNotIn("--chrome", argv)


if __name__=="__main__":unittest.main()
