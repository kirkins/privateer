#!/usr/bin/env python3
"""V2 preflight, exact resume and native completion across real subprocesses."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins"))
from native import build
from opencode_v2 import inspect

SESSION = "ses_fixture123"
SERVER = r'''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
root=Path(os.environ['OC_TEST_ROOT'])
with (root/'calls.jsonl').open('a') as f:f.write(json.dumps(sys.argv[1:])+'\n')
if '--version' in sys.argv:
 print(os.environ.get('OC_TEST_VERSION','2.0.29'));raise SystemExit(0)
if sys.argv[1:2]==['api']:
 data=json.loads((root/'metadata.json').read_text())
 if data is None:raise SystemExit(1)
 print(json.dumps({'data':data}));raise SystemExit(0)
if '--help' in sys.argv:
 print('USAGE opencode [flags]');raise SystemExit(0)
(root/'launched.json').write_text(json.dumps({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('OC_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')}))
sys.stdout.buffer.write((root/'stream.jsonl').read_bytes());sys.stdout.buffer.flush()
raise SystemExit(int(os.environ.get('OC_TEST_EXIT','0')))
'''


class OpenCodeV2(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-oc2-test-")
        self.root = Path(self.temp.name).resolve()
        (self.root / "opencode").write_text(SERVER)
        (self.root / "opencode").chmod(0o700)
        self.metadata = {"id":SESSION,"location":{"directory":str(self.root)}}

    def tearDown(self):
        self.temp.cleanup()

    def event(self, kind, message="msg_one", **part):
        value = {"type":kind,"sessionID":SESSION,"part":{
            "id":"prt_"+kind+message,"messageID":message,"sessionID":SESSION,
            "type":{"step_start":"step-start","step_finish":"step-finish"}.get(kind,kind),**part}}
        return value

    def complete(self, message="msg_one", reason="stop"):
        return [self.event("step_start",message),self.event("text",message,text="done"),
                self.event("step_finish",message,reason=reason,tokens={"input":4,"output":2,"cache":{"read":1}})]

    def launch(self, events=None, resume=None, extra=(), native_exit=0, version="2.0.29", raw=None):
        (self.root / "metadata.json").write_text(json.dumps(self.metadata))
        (self.root / "stream.jsonl").write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in (events or [])))
        return subprocess.run([sys.executable,str(ROOT / "plugins/native_launch.py"),"opencode-v2","opencode",
                               "run","--standalone","--format","json",*(["--session",resume] if resume else []),
                               *extra,"--","Literal $(touch forbidden)"],cwd=self.root,capture_output=True,timeout=7,
                              env={**os.environ,"PATH":str(self.root)+os.pathsep+os.environ['PATH'],
                                   "OC_TEST_ROOT":str(self.root),"OC_TEST_VERSION":version,"OC_TEST_EXIT":str(native_exit),
                                   "PRIVATEER_ROLE":"agent","PRIVATEER_ACTOR":"department",
                                   "OC_TEST_ROUTE":"existing-gateway","CLAUDECODE":"foreign"})

    def test_completed_turn_preserves_actor_route_literal_prompt_and_counts_authoritative_usage(self):
        run=self.launch(self.complete())
        self.assertEqual(run.returncode,0,run.stdout+run.stderr)
        summary=json.loads(run.stdout.splitlines()[-1])
        self.assertEqual(summary['session_id'],SESSION)
        self.assertEqual(summary['usage'],{'input':4,'output':2,'cacheRead':1})
        record=json.loads((self.root/'launched.json').read_text())
        self.assertEqual(record['role'],'agent');self.assertEqual(record['actor'],'department')
        self.assertEqual(record['route'],'existing-gateway');self.assertIsNone(record['foreign'])
        self.assertEqual(record['argv'][-1],'Literal $(touch forbidden)')
        self.assertFalse((self.root/'forbidden').exists())

    def test_multiple_tool_steps_need_a_final_stop_and_usage_is_not_double_counted(self):
        run=self.launch(self.complete(reason='tool-calls')+self.complete('msg_two'))
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(json.loads(run.stdout.splitlines()[-1])['usage']['input'],8)
        self.assertNotEqual(self.launch(self.complete(reason='tool-calls')).returncode,0)
        self.assertNotEqual(self.launch(self.complete(reason='length')).returncode,0)

    def test_native_retry_starts_do_not_abort_the_harness_or_count_extra_usage(self):
        run=self.launch([self.event('step_start'),self.event('step_start')]+self.complete())
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(json.loads(run.stdout.splitlines()[-1])['usage']['input'],4)
        self.assertNotEqual(self.launch([self.event('step_start'),self.event('step_start','msg_changed')]).returncode,0)

    def test_exact_resume_checks_saved_workspace_before_any_prompt_is_sent(self):
        self.assertEqual(self.launch(self.complete(),resume=SESSION).returncode,0)
        for metadata in [None,{}, {'id':'ses_other','location':{'directory':str(self.root)}},
                         {'id':SESSION,'location':{'directory':'/'}},
                         {'id':SESSION,'location':{'directory':'.'}}, {'id':SESSION}]:
            with self.subTest(metadata=metadata):
                (self.root/'launched.json').unlink(missing_ok=True)
                self.metadata=metadata
                run=self.launch(self.complete(),resume=SESSION)
                self.assertNotEqual(run.returncode,0)
                self.assertFalse((self.root/'launched.json').exists())

    def test_v1_and_ambiguous_or_shared_resume_options_fail_before_prompt(self):
        for options in [{'version':'1.19.0'},{'resume':'ses_!!!'}, {'extra':['--continue']},
                        {'extra':['-s',SESSION]}, {'extra':['--session='+SESSION]},
                        {'extra':['--server','http://localhost:4096']},
                        {'extra':['--server=http://localhost:4096']},
                        {'extra':['--fork']},{'extra':['--session',SESSION,'--session',SESSION]}]:
            with self.subTest(options=options):
                self.assertNotEqual(self.launch(self.complete(),**options).returncode,0)
                self.assertFalse((self.root/'launched.json').exists())

    def test_error_zero_exit_incomplete_duplicate_or_mixed_session_frames_fail(self):
        error={'type':'error','sessionID':SESSION,'error':{'type':'provider.auth'}}
        cases=[[],[error],[self.event('step_start')],self.complete()+[error],
               self.complete()+[self.event('step_finish',reason='stop')],
               [self.event('text',text='unframed')],self.complete()+[self.event('text',text='late')]]
        wrong=self.complete();wrong[-1]['sessionID']='ses_other';cases.append(wrong)
        wrong=self.complete();wrong[-1]['part']['sessionID']='ses_other';cases.append(wrong)
        wrong=self.complete();wrong[-1]['part']['messageID']='msg_other';cases.append(wrong)
        for events in cases:
            with self.subTest(events=events):self.assertNotEqual(self.launch(events).returncode,0)
        self.assertNotEqual(self.launch(self.complete(),native_exit=7).returncode,0)

    def test_truncated_malformed_and_oversized_streams_fail(self):
        good=b''.join((json.dumps(e)+'\n').encode() for e in self.complete())
        for raw in [good[:-1],good+b'not-json\n',b'x'*(1024*1024+1)]:
            self.assertNotEqual(self.launch(raw=raw).returncode,0)

    def test_help_probe_checks_version_but_never_resumes_a_sample_session(self):
        run=self.launch(resume=SESSION,extra=['--help'])
        self.assertEqual(run.returncode,0,run.stdout)
        calls=[json.loads(x) for x in (self.root/'calls.jsonl').read_text().splitlines()]
        self.assertFalse(any(x[:1]==['api'] for x in calls))
        self.assertFalse((self.root/'launched.json').exists())

    def test_inspection_bounds_and_timeout_reap_helpers(self):
        with self.assertRaises(ValueError):inspect([sys.executable,'-c','print("x"*100)'],limit=32)
        with self.assertRaises(ValueError):inspect([sys.executable,'-c','import time;time.sleep(10)'],seconds=.1)
        # A grandchild inheriting the pipe must also be killed on timeout.
        child_code="import os,time; time.sleep(10)"
        code="import subprocess,sys,time; from pathlib import Path; p=subprocess.Popen([sys.executable,'-c',"+repr(child_code)+"]); Path("+repr(str(self.root/'pid'))+").write_text(str(p.pid)); time.sleep(10)"
        with self.assertRaises(ValueError):inspect([sys.executable,'-c',code],seconds=.3)
        pid=int((self.root/'pid').read_text())
        # Linux may retain a dead adopted zombie until init reaps it.
        end=time.monotonic()+2
        while True:
            proc=Path('/proc')/str(pid)/'stat'
            try:
                if proc.exists() and proc.read_text().split()[2]=='Z':break
                os.kill(pid,0)
            except (ProcessLookupError,FileNotFoundError):break
            self.assertLess(time.monotonic(),end,'inspection descendant survived cleanup')
            time.sleep(.01)

    def test_v2_plans_use_private_server_cwd_and_variant_not_v1_flags(self):
        request={'context':{'repo':str(self.root),'title':'work'},'options':{'model':'provider/model','effort':'high'}}
        cwd,argv=build('opencode-v2',request)
        self.assertEqual(cwd,str(self.root))
        self.assertIn('provider/model#high',argv)
        self.assertIn('--standalone',argv)
        self.assertNotIn('--dir',argv);self.assertNotIn('--variant',argv)
        for options in [{'mode':'interactive','model':'provider/model'},{'effort':'high'},
                        {'model':'provider/model#high','effort':'low'}]:
            with self.assertRaises(ValueError):build('opencode-v2',{**request,'options':options})


if __name__=='__main__':unittest.main()
