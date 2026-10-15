#!/usr/bin/env python3
"""V2 preflight, exact resume and native completion across real subprocesses."""
import json
import copy
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
 if sys.argv[3:4]==['session.message.list']:
  print((root/'projection.json').read_text());raise SystemExit(0)
 data=json.loads((root/'metadata.json').read_text())
 if data is None:raise SystemExit(1)
 print(json.dumps({'data':data}));raise SystemExit(0)
if '--help' in sys.argv:
 print('USAGE opencode [flags]');raise SystemExit(0)
(root/'launched.json').write_text(json.dumps({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('OC_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE'),'pwd':os.getenv('PWD'),'cwd':str(Path.cwd())}))
sys.stdout.buffer.write((root/'stream.jsonl').read_bytes());sys.stdout.buffer.flush()
raise SystemExit(int(os.environ.get('OC_TEST_EXIT','0')))
'''


class OpenCodeV2(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-oc2-test-")
        self.root = Path(self.temp.name).resolve()
        (self.root / "opencode").write_text(SERVER)
        (self.root / "opencode").chmod(0o700)
        self.metadata = {"id":SESSION,"location":{"directory":str(self.root)},"time":{"idle":30},"outcome":"succeeded"}
        self.projection = {"data": []}

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
        (self.root / "projection.json").write_text(json.dumps(self.projection))
        (self.root / "stream.jsonl").write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in (events or [])))
        return subprocess.run([sys.executable,str(ROOT / "plugins/native_launch.py"),"opencode-v2","opencode",
                               "run","--standalone","--format","json",*(["--session",resume] if resume else []),
                               *extra,"--","Literal $(touch forbidden)"],cwd=self.root,capture_output=True,timeout=7,
                              env={**os.environ,"PATH":str(self.root)+os.pathsep+os.environ['PATH'],
                                   "OC_TEST_ROOT":str(self.root),"OC_TEST_VERSION":version,"OC_TEST_EXIT":str(native_exit),
                                   "PRIVATEER_ROLE":"agent","PRIVATEER_ACTOR":"department",
                                   "OC_TEST_ROUTE":"existing-gateway","CLAUDECODE":"foreign","PWD":"/stale-launcher-workspace"})

    def test_completed_turn_preserves_actor_route_literal_prompt_and_counts_authoritative_usage(self):
        run=self.launch(self.complete())
        self.assertEqual(run.returncode,0,run.stdout+run.stderr)
        summary=json.loads(run.stdout.splitlines()[-1])
        self.assertEqual(summary['session_id'],SESSION)
        self.assertEqual(summary['usage'],{'input':4,'output':2,'cacheRead':1})
        record=json.loads((self.root/'launched.json').read_text())
        self.assertEqual(record['role'],'agent');self.assertEqual(record['actor'],'department')
        self.assertEqual(record['route'],'existing-gateway');self.assertIsNone(record['foreign'])
        self.assertEqual(record['pwd'],str(self.root));self.assertEqual(record['cwd'],str(self.root))
        self.assertEqual(record['argv'][-1],'Literal $(touch forbidden)')
        self.assertFalse((self.root/'forbidden').exists())

    def final_projection(self):
        return {"data":[
            {"id":"msg_idle","type":"idle","outcome":"succeeded","time":{"created":30}},
            {"id":"msg_one","type":"assistant","finish":"stop","time":{"created":10,"completed":20},
             "content":[{"type":"text","text":"done"}],
             "tokens":{"input":3,"output":2,"reasoning":1,"cache":{"read":4,"write":0}}},
        ]}

    def test_missing_final_finish_requires_exact_native_success_projection_and_counts_usage_once(self):
        self.projection=self.final_projection()
        events=self.complete('msg_previous',reason='tool-calls')+self.complete()[:-1]
        run=self.launch(events)
        self.assertEqual(run.returncode,0,run.stdout+run.stderr)
        summary=json.loads(run.stdout.splitlines()[-1])
        self.assertEqual(summary['completion_source'],'native-session-projection')
        self.assertEqual(summary['session_id'],SESSION)
        self.assertEqual(summary['usage'],{'input':7,'output':4,'reasoning':1,'cacheRead':5,'cacheWrite':0})
        # Native output is preserved; the missing event is not fabricated in it.
        self.assertEqual(sum(json.loads(l).get('type')=='step_finish' for l in run.stdout.splitlines()),1)
        self.assertEqual(self.launch(self.complete()[:-1],resume=SESSION).returncode,0)
        (self.root/'calls.jsonl').unlink()
        run=self.launch(self.complete())
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertNotIn('completion_source',json.loads(run.stdout.splitlines()[-1]))
        calls=[json.loads(l) for l in (self.root/'calls.jsonl').read_text().splitlines()]
        self.assertFalse(any(c[:1]==['api'] for c in calls),'complete streams need no projection lookup')

    def test_projection_cannot_rescue_failed_truncated_or_terminal_nonstop_stream(self):
        self.projection=self.final_projection()
        bad=[([],0), (self.complete()[:-1],7), (self.complete(reason='length'),0),
             (self.complete()[:-1]+[{'type':'error','sessionID':SESSION,'error':{}}],0),
             (self.complete()[:-1]+[self.event('text',message='msg_other',text='other')],0)]
        for events,native_exit in bad:
            (self.root/'calls.jsonl').unlink(missing_ok=True)
            run=self.launch(events,native_exit=native_exit)
            self.assertNotEqual(run.returncode,0,run.stdout)
            calls=[json.loads(l) for l in (self.root/'calls.jsonl').read_text().splitlines()]
            self.assertFalse(any(c[:1]==['api'] for c in calls))
        truncated=b''.join((json.dumps(e)+'\n').encode() for e in self.complete()[:-1])[:-1]
        self.assertNotEqual(self.launch(raw=truncated).returncode,0)

    def test_final_projection_refuses_stale_foreign_error_unsettled_or_oversized_records(self):
        good=self.final_projection()
        cases=[{}, {'data':[]}, {'data':good['data'][1:]}, {'data':list(reversed(good['data']))}]
        changes=[(0,'type','assistant'),(0,'outcome','failed'),(0,'outcome','interrupted'),(0,'error',{}),
                 (1,'id','msg_other'),(1,'type','user'),(1,'finish','tool-calls'),(1,'error',{}),
                 (1,'retry',{}),(1,'time',{'created':10}), (1,'time',{'created':21,'completed':20}),
                 (0,'time',{'created':19}), (0,'time',{'created':True}), (1,'content',None),
                 (1,'content',[{'type':'tool','state':{'status':'running'}}]),
                 (1,'content',[{'type':'tool','state':{'status':'streaming'}}]),
                 (1,'content',[{'type':'unknown'}]), (1,'content',[{'type':'text','text':'x'*65536}])]
        for index,key,value in changes:
            altered=copy.deepcopy(good);altered['data'][index][key]=value;cases.append(altered)
        for projection in cases:
            self.projection=projection
            self.assertNotEqual(self.launch(self.complete()[:-1]).returncode,0)
        self.projection=good
        base=copy.deepcopy(self.metadata)
        cases=[None,{}]
        for key,value in [('id','ses_other'),('location',{'directory':'/'}),('location',{'directory':'.'}),
                          ('time',{'idle':31}),('outcome','failed'),('outcome','interrupted'),('outcome',None)]:
            cases.append({**base,key:value})
        for metadata in cases:
            self.metadata=metadata
            self.assertNotEqual(self.launch(self.complete()[:-1]).returncode,0)

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

    def test_inventory_probe_cannot_qualify_a_v1_binary_as_v2(self):
        for version, expected in [('1.19.0',1),('2.0.29',0)]:
            calls_path=self.root/'calls.jsonl'
            calls_path.unlink(missing_ok=True)
            run=subprocess.run([sys.executable,str(ROOT/'scripts/harness-probe.py'),'--harness','opencode-v2'],
                cwd=self.root,capture_output=True,timeout=10,
                env={**os.environ,'PATH':str(self.root)+os.pathsep+os.environ['PATH'],
                     'OC_TEST_ROOT':str(self.root),'OC_TEST_VERSION':version})
            self.assertEqual(run.returncode,expected,run.stdout+run.stderr)
            report=json.loads(run.stdout)['results'][0]
            self.assertEqual(report['passed'],version.startswith('2.'))
            calls=[json.loads(x) for x in calls_path.read_text().splitlines()]
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
