#!/usr/bin/env python3
"""Real pipe/process regressions for the optional ACP turn adapter; no models."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SERVER = r'''#!/usr/bin/env python3
import json,os,sys,time
mode=os.environ['ACP_TEST_MODE']
record=os.environ['ACP_TEST_RECORD']
session='native-session-1'
def send(value): print(json.dumps(value),flush=True)
def reply(request,result): send({'jsonrpc':'2.0','id':request['id'],'result':result})
if mode=='silent': time.sleep(30)
for line in sys.stdin:
    request=json.loads(line)
    with open(record,'a') as stream: stream.write(json.dumps(request)+'\n')
    method=request.get('method')
    if method=='initialize':
        if mode=='wrong-id':
            send({'jsonrpc':'2.0','id':'unrelated','result':{}})
        elif mode=='oversize': print('x'*(1024*1024+1),flush=True)
        else: reply(request,{'protocolVersion':1,'agentCapabilities':{'loadSession':mode!='no-resume'}})
    elif method=='session/new':
        if mode in ('startup-update','startup-other'):
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'other' if mode=='startup-other' else session,'update':{'sessionUpdate':'agent_message_chunk'}}})
        result={'sessionId':session}
        if mode=='legacy-model':result['models']={'availableModels':[{'modelId':'fixture-model'}]}
        elif mode!='no-model':result['configOptions']=[{'id':'engine','category':'model','type':'select','currentValue':'default','options':[{'group':'models','options':[{'value':'fixture-model','name':'Fixture'}]}]}]
        reply(request,result)
        if mode=='blocked-write':time.sleep(30)
    elif method=='session/load':
        session=request['params']['sessionId']
        reply(request,{'sessionId':'changed-session'} if mode=='changed-resume' else {})
    elif method=='session/set_model': reply(request,{})
    elif method=='session/set_config_option':
        reply(request,{'configOptions':[{'id':'engine','currentValue':'default' if mode=='ignored-model' else request['params']['value']}]})
    elif method=='session/prompt':
        if mode=='wrong-session':
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'other','update':{'sessionUpdate':'agent_message_chunk'}}})
            continue
        send({'jsonrpc':'2.0','id':'permission','method':'session/request_permission',
              'params':{'sessionId':session,'toolCall':{'toolCallId':'tool-1'},
                        'options':[{'optionId':'forever','kind':'allow_always'}]+([] if mode=='only-persistent' else [{'optionId':'once','kind':'allow_once'}])}})
        response=json.loads(sys.stdin.readline())
        with open(record,'a') as stream: stream.write(json.dumps(response)+'\n')
        send({'jsonrpc':'2.0','id':'unsupported-tool','method':'terminal/create',
              'params':{'sessionId':session,'command':'touch','args':[os.environ['ACP_TEST_FORBIDDEN']]}})
        response=json.loads(sys.stdin.readline())
        with open(record,'a') as stream: stream.write(json.dumps(response)+'\n')
        reply(request,{'stopReason':'max_tokens' if mode=='incomplete' else 'end_turn'})
'''


class Bridge(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-acp-contract-")
        self.root = Path(self.temp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        executable = self.bin / "rovo"
        executable.write_text(SERVER)
        executable.chmod(0o700)
        self.record = self.root / "requests.jsonl"
        self.forbidden = self.root / "must-not-exist"

    def tearDown(self):
        self.temp.cleanup()

    def invoke(self, mode="happy", **options):
        request = {"binary": "rovo", "cwd": str(self.root), "prompt": "Literal $(touch never) --help",
                   "permissions": "auto", "timeout": 1, **options}
        run = subprocess.run([sys.executable, str(ROOT / "plugins/acp.py"), "--request", json.dumps(request)],
                             capture_output=True, text=True, timeout=5,
                             env={**os.environ, "PATH": str(self.bin)+os.pathsep+os.environ["PATH"],
                                  "ACP_TEST_MODE": mode, "ACP_TEST_RECORD": str(self.record),
                                  "ACP_TEST_FORBIDDEN": str(self.forbidden)})
        self.assertFalse(self.forbidden.exists(), "an unadvertised client method executed a command")
        events = [json.loads(line) for line in run.stdout.splitlines()]
        records = [json.loads(line) for line in self.record.read_text().splitlines()] if self.record.exists() else []
        return run, events, records

    def test_real_correlated_turn_preserves_literal_prompt_and_uses_only_once_permission(self):
        run, events, records = self.invoke(model="fixture-model")
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual(events[-1]["type"], "turn.completed")
        prompt = next(r for r in records if r.get("method") == "session/prompt")
        self.assertEqual(prompt["params"]["prompt"][0]["text"], "Literal $(touch never) --help")
        permission = next(r for r in records if r.get("id") == "permission" and "result" in r)
        self.assertEqual(permission["result"]["outcome"]["optionId"], "once")
        self.assertTrue(any(r.get("method") == "session/set_config_option" for r in records))
        unsupported = next(r for r in records if r.get("id") == "unsupported-tool" and "error" in r)
        self.assertEqual(unsupported["error"]["code"], -32601)

    def test_model_selection_requires_advertisement_and_confirmation(self):
        run, _, records = self.invoke('legacy-model', model='fixture-model')
        self.assertEqual(run.returncode, 0)
        self.assertTrue(any(r.get('method') == 'session/set_model' for r in records))
        for mode, model in [('no-model','fixture-model'), ('happy','unknown'), ('ignored-model','fixture-model')]:
            self.record.unlink()
            with self.subTest(mode=mode):
                run, events, records = self.invoke(mode, model=model)
                self.assertNotEqual(run.returncode, 0)
                self.assertFalse(any(r.get('method') == 'session/prompt' for r in records))
                self.assertFalse(any(e['type'] == 'turn.completed' for e in events))

    def test_resume_is_capability_gated_and_never_falls_back_to_new(self):
        run, events, records = self.invoke(resume="saved-session")
        self.assertEqual(run.returncode, 0)
        self.assertEqual(events[0]["session_id"], "saved-session")
        self.assertFalse(any(r.get("method") == "session/new" for r in records))
        self.record.unlink()
        run, events, records = self.invoke("no-resume", resume="saved-session")
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual([r["method"] for r in records], ["initialize"])
        self.assertEqual(events[-1]["type"], "turn.failed")

    def test_persistent_permission_is_not_substituted_for_a_one_turn_grant(self):
        for mode, options in [("only-persistent", {}), ("happy", {"permissions":"ask"})]:
            with self.subTest(mode=mode):
                run, events, _ = self.invoke(mode, **options)
                self.assertNotEqual(run.returncode, 0)
                self.assertFalse(any(e["type"] == "turn.completed" for e in events))

    def test_wrong_ids_sessions_partial_turns_and_oversize_fail_without_a_done_fact(self):
        for mode in ["wrong-id", "wrong-session", "incomplete", "oversize", "changed-resume", "startup-other"]:
            with self.subTest(mode=mode):
                run, events, _ = self.invoke(mode, **({"resume":"saved-session"} if mode=="changed-resume" else {}))
                self.assertNotEqual(run.returncode, 0)
                self.assertFalse(any(e["type"] == "turn.completed" for e in events))

    def test_startup_updates_are_bounded_and_verified_against_the_new_session(self):
        run, events, _ = self.invoke("startup-update")
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertTrue(any(e["type"] == "acp.update" and e["session_id"] == "native-session-1" for e in events))

    def test_stalled_stdin_or_reply_cannot_hold_the_bridge_open(self):
        before = time.monotonic()
        run, events, _ = self.invoke("blocked-write", timeout=0.2, prompt="x"*32768)
        self.assertNotEqual(run.returncode, 0)
        self.assertLess(time.monotonic()-before, 3)
        self.assertFalse(any(e["type"] == "turn.completed" for e in events))

    def test_silent_native_process_has_a_deadline_and_invalid_deadline_starts_nothing(self):
        before = time.monotonic()
        run, _, _ = self.invoke("silent", timeout=0.1)
        self.assertNotEqual(run.returncode, 0)
        self.assertLess(time.monotonic()-before, 3)
        run, _, records = self.invoke(timeout=float("nan"))
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual(records, [])


if __name__ == "__main__":
    unittest.main()
