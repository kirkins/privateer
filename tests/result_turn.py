#!/usr/bin/env python3
"""Gemini/Cursor terminal facts across real native launcher subprocesses."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = "46a81234-1234-4567-8123-123456789abc"
OTHER = "46a81234-1234-4567-8123-123456789abd"
SERVER = r'''#!/usr/bin/env python3
import json,os,subprocess,sys
from pathlib import Path
harness='cursor' if Path(sys.argv[0]).name=='cursor-agent' else 'gemini'
sid=sys.argv[sys.argv.index('--resume')+1] if '--resume' in sys.argv else '46a81234-1234-4567-8123-123456789abc'
if os.environ.get('RESULT_TEST_STREAM'):
    Path(os.environ['RESULT_TEST_ARGV']).write_text(json.dumps({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('RESULT_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')}))
    sys.stdout.buffer.write(Path(os.environ['RESULT_TEST_STREAM']).read_bytes());sys.stdout.buffer.flush()
    raise SystemExit(int(os.environ['RESULT_TEST_EXIT']))
print(json.dumps({'type':'system','subtype':'init','session_id':sid,'cwd':str(Path.cwd()),'model':'Auto'} if harness=='cursor' else {'type':'init','session_id':sid,'model':'auto'}),flush=True)
assert os.environ['PRIVATEER_ROLE']=='agent' and os.environ['PRIVATEER_ACTOR']=='department'
def call(command,args):
    result=subprocess.run([os.environ['STDIO_TEST_CLI'],'--home',os.environ['STDIO_TEST_HOME'],command,'--json',json.dumps(args)],capture_output=True,text=True)
    assert result.returncode==0,command
    return json.loads(result.stdout)
if not os.environ.get('RESULT_TEST_FAILURE'):
    inbox=call('mail.inbox',{'address':'department'})
    for mail in inbox['messages']:
        if mail['kind']=='request' and mail['state']=='open':
            call('mail.send',{'id':'reply-'+mail['id'],'from':'department','to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'handled through native result fixture'})
    call('mail.ack',{'address':'department','through_seq':max(m['seq'] for m in inbox['messages'])})
    print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid} if harness=='cursor' else {'type':'result','status':'success'}),flush=True)
else:
    print(json.dumps({'type':'result','subtype':'error','is_error':True,'session_id':sid} if harness=='cursor' else {'type':'result','status':'error'}),flush=True)
'''


class ResultTurn(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-result-turn-")
        self.root = Path(self.temp.name).resolve()
        self.record = self.root / "argv.json"
        for binary in ("gemini", "cursor-agent"):
            path = self.root / binary
            path.write_text(SERVER)
            path.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def init(self, harness, **fields):
        event = {"type":"init", "session_id":IDENTITY, "model":"auto"}
        if harness == "cursor":
            event.update(type="system", subtype="init", cwd=str(self.root), model="Auto")
        return {**event, **fields}

    def result(self, harness, **fields):
        event = ({"type":"result", "session_id":IDENTITY, "subtype":"success", "is_error":False}
                 if harness == "cursor" else {"type":"result", "status":"success"})
        return {**event, **fields}

    def run_stream(self, harness, events, *, code=0, raw=None, resume=None, interactive=False):
        stream = self.root / "stream.jsonl"
        stream.write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in events))
        binary = "cursor-agent" if harness == "cursor" else harness
        flags = [] if interactive else ["--output-format","stream-json", "--prompt" if harness == "gemini" else "--print"]
        if resume:
            flags += ["--resume", resume]
        return subprocess.run([sys.executable,str(ROOT / "plugins/native_launch.py"),harness,binary,
                               *flags,"--","Literal $(touch forbidden)"],cwd=self.root,capture_output=True,timeout=5,
                              env={**os.environ,"PATH":str(self.root)+os.pathsep+os.environ['PATH'],
                                   "RESULT_TEST_STREAM":str(stream),"RESULT_TEST_ARGV":str(self.record),
                                   "RESULT_TEST_EXIT":str(code),"PRIVATEER_ROLE":"agent","PRIVATEER_ACTOR":"department",
                                   "RESULT_TEST_ROUTE":"unchanged","CLAUDECODE":"foreign"})

    def test_native_results_preserve_identity_route_and_literal_prompt(self):
        for harness in ("gemini", "cursor"):
            run = self.run_stream(harness,[self.init(harness),self.result(harness)],resume=IDENTITY)
            self.assertEqual(run.returncode,0,run.stdout)
            summary = json.loads(run.stdout.splitlines()[-1])
            self.assertEqual(summary['type'],'turn.completed')
            self.assertEqual(summary['session_id'],IDENTITY)
            self.assertNotIn('usage',summary)
            record = json.loads(self.record.read_text())
            self.assertEqual((record['role'],record['actor'],record['route']),('agent','department','unchanged'))
            self.assertIsNone(record['foreign'])
            self.assertEqual(record['argv'][-1],'Literal $(touch forbidden)')
            self.assertFalse((self.root/'forbidden').exists())

    def test_exit_zero_cannot_hide_failure_missing_results_or_trailing_work(self):
        for harness in ("gemini", "cursor"):
            init, result = self.init(harness), self.result(harness)
            failure = self.result(harness,**({'status':'error'} if harness=='gemini' else {'is_error':True}))
            for events in ([init,failure],[init],[result],[init,init,result],[init,result,result],
                           [init,result,{'type':'message'}],[{'type':'message'},init,result],
                           [init,{'type':'error','severity':'error','session_id':IDENTITY},result]):
                with self.subTest(harness=harness,events=events):
                    self.assertNotEqual(self.run_stream(harness,events).returncode,0)
            run = self.run_stream(harness,[init,result],code=7)
            self.assertNotEqual(run.returncode,0)
            self.assertEqual(json.loads(run.stdout.splitlines()[-1])['native_exit_code'],7)

    def test_resume_and_every_projected_identity_must_match(self):
        for harness in ("gemini", "cursor"):
            for events in ([self.init(harness,session_id=OTHER),self.result(harness)],
                           [self.init(harness),self.result(harness,session_id=OTHER)],
                           [self.init(harness,session_id='not-a-uuid'),self.result(harness)]):
                self.assertNotEqual(self.run_stream(harness,events,resume=IDENTITY).returncode,0)
        self.assertNotEqual(self.run_stream('cursor',[self.init('cursor',cwd='/'),self.result('cursor')]).returncode,0)

    def test_tools_must_settle_but_a_failed_tool_can_recover(self):
        for harness in ('gemini','cursor'):
            start = ({'type':'tool_use','tool_id':'tool-1'} if harness=='gemini' else
                     {'type':'tool_call','subtype':'started','call_id':'tool-1','session_id':IDENTITY})
            finish = ({'type':'tool_result','tool_id':'tool-1','status':'error'} if harness=='gemini' else
                      {'type':'tool_call','subtype':'completed','call_id':'tool-1','session_id':IDENTITY,'tool_call':{'error':True}})
            init,result = self.init(harness),self.result(harness)
            self.assertEqual(self.run_stream(harness,[init,start,finish,result]).returncode,0)
            for middle in ([start],[finish],[start,start],[start,{**finish,('tool_id' if harness=='gemini' else 'call_id'):'other'}]):
                self.assertNotEqual(self.run_stream(harness,[init,*middle,result]).returncode,0)

    def test_usage_is_only_the_terminal_aggregate_and_warnings_can_recover(self):
        stats = {'input_tokens':11,'output_tokens':3,'cached':5,'total_tokens':14,'models':{'model':{'input_tokens':11}}}
        run = self.run_stream('gemini',[self.init('gemini'),{'type':'error','severity':'warning'},
            {'type':'message','usage':{'input':999}},self.result('gemini',stats=stats)])
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(json.loads(run.stdout.splitlines()[-1])['usage'],{'input':11,'output':3,'cacheRead':5,'totalTokens':14})
        run = self.run_stream('cursor',[self.init('cursor'),{'type':'assistant','session_id':IDENTITY,'usage':{'inputTokens':999}},
            self.result('cursor',usage={'inputTokens':11,'outputTokens':3,'cacheReadTokens':5,'cacheWriteTokens':2})])
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(json.loads(run.stdout.splitlines()[-1])['usage'],{'input':11,'output':3,'cacheRead':5,'cacheWrite':2})

    def test_malformed_truncated_and_oversized_streams_fail_but_interactive_is_native(self):
        for harness in ('gemini','cursor'):
            good = b''.join((json.dumps(e)+'\n').encode() for e in [self.init(harness),self.result(harness)])
            for raw in (good[:-1],good+b'not-json\n',good+b'x'*(1024*1024+1)):
                self.assertNotEqual(self.run_stream(harness,[],raw=raw).returncode,0)
            # Normal interactive stdout is never parsed as worker JSON.
            run = self.run_stream(harness,[],raw=b'native interactive text',interactive=True)
            self.assertEqual(run.returncode,0)
            self.assertEqual(run.stdout,b'native interactive text')


if __name__ == '__main__':
    unittest.main()
