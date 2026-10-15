#!/usr/bin/env python3
"""AGY's native event envelope, terminal result and subprocess regressions."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = "46a81234-1234-4567-8123-123456789abc"
SERVER = r'''#!/usr/bin/env python3
import json,os,subprocess,sys
from pathlib import Path
if os.environ.get('AGY_TEST_STREAM'):
    Path(os.environ['AGY_TEST_ARGV']).write_text(json.dumps({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('AGY_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')}))
    sys.stdout.buffer.write(Path(os.environ['AGY_TEST_STREAM']).read_bytes());sys.stdout.buffer.flush()
    raise SystemExit(int(os.environ['AGY_TEST_EXIT']))
sid=sys.argv[sys.argv.index('--conversation')+1] if '--conversation' in sys.argv else '46a81234-1234-4567-8123-123456789abc'
print(json.dumps({'event':'init','conversation_id':sid,'init':{'cwd':str(Path.cwd()),'model':'fixture-model'}}),flush=True)
assert os.environ['PRIVATEER_ROLE']=='agent' and os.environ['PRIVATEER_ACTOR']=='department'
def call(command,args):
    result=subprocess.run([os.environ['STDIO_TEST_CLI'],'--home',os.environ['STDIO_TEST_HOME'],command,'--json',json.dumps(args)],capture_output=True,text=True)
    assert result.returncode==0,command
    return json.loads(result.stdout)
inbox=call('mail.inbox',{'address':'department'})
for mail in inbox['messages']:
    if mail['kind']=='request' and mail['state']=='open':
        call('mail.send',{'id':'reply-'+mail['id'],'from':'department','to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'handled through AGY event fixture'})
call('mail.ack',{'address':'department','through_seq':max(m['seq'] for m in inbox['messages'])})
print(json.dumps({'event':'result','result':{'conversation_id':sid,'status':'SUCCESS','usage':{'input_tokens':4,'output_tokens':2}}}),flush=True)
'''


class AgyTurn(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-agy-events-")
        self.root = Path(self.temp.name).resolve()
        self.record = self.root / "argv.json"
        native = self.root / "agy"
        native.write_text(SERVER)
        native.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def init(self, **fields):
        return {"event":"init","conversation_id":IDENTITY,"init":{"cwd":str(self.root),"model":"fixture-model"},**fields}

    def result(self, **fields):
        return {"event":"result","result":{"conversation_id":IDENTITY,"status":"SUCCESS",**fields}}

    def run_stream(self, events, code=0, raw=None, resume=None):
        stream = self.root / "stream.jsonl"
        stream.write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in events))
        argv=[sys.executable,str(ROOT / "plugins/native_launch.py"),"agy","agy","--output-format","stream-json",
              "--model","fixture-model",*(["--conversation",resume] if resume else []),"--print","Literal $(touch forbidden)"]
        return subprocess.run(argv,cwd=self.root,capture_output=True,timeout=5,
                              env={**os.environ,"PATH":str(self.root)+os.pathsep+os.environ['PATH'],
                                   "AGY_TEST_STREAM":str(stream),"AGY_TEST_ARGV":str(self.record),"AGY_TEST_EXIT":str(code),
                                   "PRIVATEER_ROLE":"agent","PRIVATEER_ACTOR":"department","AGY_TEST_ROUTE":"unchanged","CLAUDECODE":"foreign"})

    def summary(self, run):
        return json.loads(run.stdout.splitlines()[-1])

    def test_complete_native_result_counts_final_usage_and_preserves_role_route_and_literal_prompt(self):
        run=self.run_stream([self.init(),{'event':'step_update','step_update':{'conversation_id':IDENTITY,'usage':{'input_tokens':999}}},
                             self.result(usage={'input_tokens':11,'output_tokens':3,'thinking_tokens':2,'cache_read_tokens':5,'total_tokens':14})])
        self.assertEqual(run.returncode,0,run.stderr)
        self.assertEqual(self.summary(run)['usage'],{'input':11,'output':3,'reasoning':2,'cacheRead':5,'totalTokens':14})
        record=json.loads(self.record.read_text())
        self.assertEqual(record['role'],'agent');self.assertEqual(record['actor'],'department')
        self.assertEqual(record['route'],'unchanged');self.assertIsNone(record['foreign'])
        self.assertEqual(record['argv'][-1],'Literal $(touch forbidden)')
        self.assertFalse((self.root/'forbidden').exists())

    def test_zero_exit_does_not_hide_failed_cancelled_missing_or_partial_results(self):
        for status in ['ERROR','CANCELLED','INTERRUPTED','TIMEOUT','UNKNOWN']:
            run=self.run_stream([self.init(),self.result(status=status)])
            self.assertNotEqual(run.returncode,0)
            self.assertEqual(self.summary(run)['native_exit_code'],0)
        for events in [[self.init()],[self.result()],
                       [self.init(),self.result(),self.result()],
                       [self.init(),self.result(),{'event':'step_update'}],
                       [self.init(),{'event':'other'}]]:
            self.assertNotEqual(self.run_stream(events).returncode,0)

    def test_resume_workspace_model_and_result_identity_are_exact(self):
        self.assertEqual(self.run_stream([self.init(),self.result()],resume=IDENTITY).returncode,0)
        for events,options in [
            ([self.init(conversation_id='other'),self.result()],{'resume':IDENTITY}),
            ([self.init(),self.result(conversation_id='other')],{}),
            ([self.init(init={'cwd':'/','model':'fixture-model'}),self.result()],{}),
            ([self.init(init={'cwd':str(self.root),'model':'different'}),self.result()],{}),
            ([self.init(),self.init(),self.result()],{}),
            ([self.init(),{'event':'step_update','step_update':{'conversation_id':'other'}},self.result()],{}),
            ([{'event':'step_update'},self.init(),self.result()],{}),
        ]:
            self.assertNotEqual(self.run_stream(events,**options).returncode,0)

    def test_native_exit_failure_malformed_oversized_and_unterminated_frames_fail(self):
        good=b"".join((json.dumps(e)+'\n').encode() for e in [self.init(),self.result()])
        self.assertNotEqual(self.run_stream([self.init(),self.result()],code=7).returncode,0)
        for raw in [good[:-1],good+b'not-json\n',good+b'x'*(1024*1024+1)]:
            self.assertNotEqual(self.run_stream([],raw=raw).returncode,0)


if __name__ == '__main__':
    unittest.main()
