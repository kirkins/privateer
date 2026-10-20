#!/usr/bin/env python3
"""Native permission plans; fixtures only, no accounts, providers or settings writes."""
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
from tool_permissions import attended_argv


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
        self.assertNotIn("--settings", argv)

        # Errands also never receive merge or publish rules
        req["context"]["role"] = "first_mate"
        req["context"]["errand"] = True
        _, argv = build("claude", req)
        self.assertNotIn("Bash(fakeforge prs merge *)", argv)
        self.assertNotIn("Bash(fakeforge prs create *)", argv)
        self.assertNotIn("Bash(gh pr merge *)", argv)
        self.assertNotIn("--settings", argv)

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
        self.assertNotIn("--dangerously-bypass-approvals-and-sandbox", argv_yolo)
        self.assertNotIn("--add-dir", argv_yolo)
        self.assertNotIn("/bin/privateer", argv_yolo)


if __name__=="__main__":unittest.main()
