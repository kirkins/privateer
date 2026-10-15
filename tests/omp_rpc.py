#!/usr/bin/env python3
"""OMP RPC process regressions; synthetic protocol, no provider calls."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SESSION = "46a81234-1234-4567-8123-123456789abc"
SERVER = r'''#!/usr/bin/env python3
import json,os,subprocess,sys,time
from pathlib import Path
mode=os.environ.get('OMP_TEST_MODE','happy')
record=os.environ.get('OMP_TEST_RECORD')
sid='46a81234-1234-4567-8123-123456789abc'
path=Path.cwd()/'native-session.jsonl'
header={'type':'session','id':sid,'cwd':str(Path.cwd())}
path.write_text(json.dumps({'type':'title','v':1,'title':'Fixture'})+'\n'+json.dumps(header)+'\n')
prompted=False
state={'sessionId':sid,'sessionFile':str(path),'isStreaming':False,'isCompacting':False,'isSettled':True,'hasPendingAsyncWork':False,'queuedMessageCount':0,'messageCount':0,'goal':None,'thinkingLevel':'low','model':{'id':'fixture-model','provider':'fixture'}}
def send(v):print(json.dumps(v),flush=True)
def response(r,data):send({'type':'response','command':r['type'],'id':r['id'],'success':True,'data':data})
def assistant(stop='stop',n=4):send({'type':'message_end','message':{'role':'assistant','stopReason':stop,'usage':{'input':n,'output':2}}})
if mode=='silent':time.sleep(30)
send({'type':'ready','protocolVersion':2 if mode=='wrong-version' else 1,'maxFrameBytes':1048576})
for line in sys.stdin:
    r=json.loads(line)
    if record:
        with open(record,'a') as out:out.write(json.dumps(r)+'\n')
    kind=r['type']
    if mode=='oversize':print('x'*(1024*1024+1),flush=True);continue
    if mode=='wrong-response':send({'type':'response','command':kind,'id':'unrelated','success':True,'data':{}});continue
    if kind=='get_state':
        current={**state,'model':dict(state['model'])}
        if mode=='busy':current['isSettled']=False
        if mode=='queued':current['queuedMessageCount']=1
        if mode=='active-goal':current['goal']={'active':True}
        if mode=='existing-fresh':current['messageCount']=2
        if mode=='ignored-model' or (prompted and mode=='changed-model'):current['model']['id']='different'
        if mode=='ignored-effort':current['thinkingLevel']='high'
        if mode=='changed-session' and prompted:current['sessionId']='46a81234-1234-4567-8123-123456789abd'
        if mode=='wrong-resume':current['sessionId']='46a81234-1234-4567-8123-123456789abd'
        if mode=='pending-after' and prompted:current['hasPendingAsyncWork']=True
        if mode=='wrong-workspace':path.write_text(json.dumps({**header,'cwd':'/'})+'\n')
        if prompted and mode=='missing-durability':path.unlink(missing_ok=True)
        response(r,current)
        if mode=='blocked-write' and not prompted:time.sleep(30)
    elif kind=='prompt':
        if mode=='early-completion':send({'type':'prompt_result','id':r['id'],'agentInvoked':True,'status':'completed','sessionSettled':True})
        response(r,{'agentInvoked':False} if mode=='local-command' else {})
        prompted=True;state['messageCount']=2
        if mode=='incomplete-frame':sys.stdout.write('{"type":');sys.stdout.flush();sys.exit(0)
        if mode=='missing-completion':sys.exit(0)
        if mode=='host-tool':send({'type':'host_tool_call','tool':'bash','command':'touch '+os.environ['OMP_TEST_FORBIDDEN']});continue
        if os.environ.get('OMP_TEST_RUNTIME_CLI'):
            assert os.environ['PRIVATEER_ROLE']=='agent' and os.environ['PRIVATEER_ACTOR']=='department'
            def call(command,args):
                result=subprocess.run([os.environ['OMP_TEST_RUNTIME_CLI'],'--home',os.environ['OMP_TEST_RUNTIME_HOME'],command,'--json',json.dumps(args)],capture_output=True,text=True)
                assert result.returncode==0,command
                return json.loads(result.stdout)
            inbox=call('mail.inbox',{'address':'department'})
            for mail in inbox['messages']:
                if mail['kind']=='request' and mail['state']=='open':
                    call('mail.send',{'id':'reply-'+mail['id'],'from':'department','to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'handled through OMP RPC fixture'})
            call('mail.ack',{'address':'department','through_seq':max(m['seq'] for m in inbox['messages'])})
        assistant('error' if mode=='assistant-error' else 'stop')
        send({'type':'agent_end','isTerminal':False,'yielded':True,'awaitingAsyncWork':True,'messages':[{'usage':{'input':100}}]})
        if mode=='early-settle':send({'type':'session_settled'})
        send({'type':'prompt_result','id':'unrelated' if mode=='wrong-prompt' else r['id'],'agentInvoked':True,'status':'error' if mode=='provider-error' else 'completed','sessionSettled':mode not in ('background','background-error','background-no-assistant','unsettled')})
        if mode in ('background','background-error','background-no-assistant'):
            send({'type':'agent_start'})
            if mode!='background-no-assistant':assistant('error' if mode=='background-error' else 'stop',3)
            send({'type':'session_settled'})
        if mode=='bad-exit':sys.exit(7)
    else:raise AssertionError(kind)
'''


class Bridge(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-omp-rpc-contract-")
        self.root = Path(self.temp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        native = self.bin / "omp"
        native.write_text(SERVER)
        native.chmod(0o700)
        self.record = self.root / "requests.jsonl"
        self.forbidden = self.root / "must-not-exist"

    def tearDown(self):
        self.temp.cleanup()

    def invoke(self, mode="happy", **options):
        self.record.unlink(missing_ok=True)
        request = {"argv": ["omp", "--mode", "rpc", "--no-ui"], "cwd": str(self.root),
                   "prompt": "Literal $(touch forbidden) --help", "timeout": 1, **options}
        run = subprocess.run([sys.executable,str(ROOT / "plugins/omp_rpc.py"),"--request",json.dumps(request)],
                             capture_output=True,text=True,timeout=5,
                             env={**os.environ,"PATH":str(self.bin)+os.pathsep+os.environ['PATH'],
                                  "OMP_TEST_MODE":mode,"OMP_TEST_RECORD":str(self.record),"OMP_TEST_FORBIDDEN":str(self.forbidden)})
        self.assertFalse(self.forbidden.exists())
        events = [json.loads(line) for line in run.stdout.splitlines()]
        records = [json.loads(line) for line in self.record.read_text().splitlines()] if self.record.exists() else []
        return run, events, records

    def test_correlated_turn_keeps_literal_input_and_counts_completed_messages_once(self):
        run, events, records = self.invoke(model='fixture/fixture-model',effort='low')
        self.assertEqual(run.returncode,0,run.stdout+run.stderr)
        self.assertEqual([r['type'] for r in records],['get_state','prompt','get_state'])
        self.assertEqual(records[1]['message'],'Literal $(touch forbidden) --help')
        self.assertEqual(events[0]['type'],'session')
        self.assertEqual(events[-1]['type'],'turn.completed')
        self.assertEqual(events[-1]['usage'],{'input':4,'output':2})
        self.assertEqual(sum('usage' in e for e in events),1)

    def test_background_delivery_must_settle_and_its_final_assistant_must_complete(self):
        run, events, _ = self.invoke('background')
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(events[-1]['usage'],{'input':7,'output':4})
        for mode in ['background-error','background-no-assistant','unsettled','pending-after']:
            run, events, _ = self.invoke(mode)
            self.assertNotEqual(run.returncode,0)
            self.assertFalse(any(e['type']=='turn.completed' for e in events))

    def test_identity_model_effort_and_busy_state_are_checked_before_prompt(self):
        for mode in ['busy','queued','active-goal','existing-fresh','ignored-model','ignored-effort']:
            with self.subTest(mode=mode):
                run, _, records=self.invoke(mode,model='fixture-model',effort='low')
                self.assertNotEqual(run.returncode,0)
                self.assertFalse(any(r['type']=='prompt' for r in records))

    def test_exact_resume_checks_file_identity_and_recorded_repository_before_admission(self):
        saved=self.root/'saved.jsonl'
        saved.write_text(json.dumps({'type':'session','id':SESSION,'cwd':str(self.root)})+'\n')
        run, events, _=self.invoke(resume=SESSION)
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(events[0]['id'],SESSION)
        for mode in ['wrong-resume','wrong-workspace']:
            run, _, records=self.invoke(mode,resume=SESSION)
            self.assertNotEqual(run.returncode,0)
            self.assertFalse(any(r['type']=='prompt' for r in records))
        native=self.root/'native-session.jsonl'
        native.write_text(json.dumps({'type':'title','title':'Fixture'})+'\n'+json.dumps({'type':'session','id':SESSION,'cwd':str(self.root)})+'\n')
        run,_,_=self.invoke(resume=str(native))
        self.assertEqual(run.returncode,0,run.stdout)
        run, _, records=self.invoke(resume=str(saved))
        self.assertNotEqual(run.returncode,0,'native file must be the exact requested path')
        self.assertFalse(any(r['type']=='prompt' for r in records))
        saved.write_text(json.dumps({'type':'session','id':SESSION,'cwd':'/'})+'\n')
        run, _, records=self.invoke(resume=str(saved))
        self.assertNotEqual(run.returncode,0)
        self.assertEqual(records,[],'wrong file scope must fail before native launch')

    def test_session_header_reader_is_bounded_and_does_not_scan_transcript_records(self):
        saved=self.root/'saved.jsonl'
        header=json.dumps({'type':'session','id':SESSION,'cwd':str(self.root)})+'\n'
        for prefix in ['x'*65537, json.dumps({'type':'message','record':{'type':'session'}})+'\n',
                       json.dumps({'type':'title'})+'\n'+json.dumps({'type':'title'})+'\n']:
            saved.write_text(prefix+header)
            run,_,records=self.invoke(resume=str(saved))
            self.assertNotEqual(run.returncode,0)
            self.assertEqual(records,[])

    def test_failures_wrong_correlation_and_truncation_never_become_completion(self):
        for mode in ['wrong-version','oversize','wrong-response','wrong-prompt','provider-error','assistant-error',
                     'early-completion','early-settle','incomplete-frame','missing-completion','host-tool','local-command',
                     'bad-exit','changed-session','changed-model','missing-durability']:
            with self.subTest(mode=mode):
                run, events, _=self.invoke(mode,model='fixture-model')
                self.assertNotEqual(run.returncode,0,run.stdout)
                self.assertFalse(any(e['type']=='turn.completed' for e in events))

    def test_ephemeral_turn_does_not_advertise_a_durable_native_session(self):
        run,events,_=self.invoke(ephemeral=True)
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertFalse(any(e['type']=='session' for e in events))

    def test_blocked_host_and_invalid_deadline_are_bounded(self):
        for mode in ['silent','blocked-write']:
            before=time.monotonic()
            run,events,_=self.invoke(mode,timeout=0.1,prompt='x'*32768)
            self.assertNotEqual(run.returncode,0)
            self.assertLess(time.monotonic()-before,3)
            self.assertFalse(any(e['type']=='turn.completed' for e in events))
        run,_,records=self.invoke(timeout=float('nan'))
        self.assertNotEqual(run.returncode,0)
        self.assertEqual(records,[])


if __name__ == '__main__':
    unittest.main()
