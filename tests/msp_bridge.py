#!/usr/bin/env python3
"""Muse MSP pipe, identity, approval and lifecycle regressions; no model calls."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
SESSION = "46a81234-1234-4567-8123-123456789abc"
SERVER = r'''#!/usr/bin/env python3
import json,os,sys,time,uuid
mode=os.environ['MSP_TEST_MODE']
sid='46a81234-1234-4567-8123-123456789abc'
state={'sessionId':sid,'workspaceRoot':os.getcwd(),'status':'idle','activeTurnId':None,'modelId':None,'providerId':'echo','approvalMode':{'mode':'onRequest'}}
turn=None
def send(v): print(json.dumps(v),flush=True)
def event(method,**params):send({'jsonrpc':'2.0','method':method,'params':params})
def reply(r,result):send({'jsonrpc':'2.0','id':r['id'],'result':result})
def done():
    terminal={'failed':'failed','cancelled':'cancelled','unknown-terminal':'future'}.get(mode,'completed')
    event('turn/completed',sessionId='other' if mode=='wrong-session' else sid,
          turnId=str(uuid.uuid4()) if mode=='wrong-turn' else turn,terminal=terminal,
          usage={'inputTokens':12,'outputTokens':3,'cachedTokens':2,'reasoningTokens':1})
    if mode=='bad-exit':sys.exit(7)
if mode=='silent':time.sleep(30)
for line in sys.stdin:
    r=json.loads(line)
    with open(os.environ['MSP_TEST_RECORD'],'a') as out:out.write(json.dumps(r)+'\n')
    method=r.get('method');p=r.get('params',{})
    if method=='initialize':
        if mode=='wrong-id':send({'jsonrpc':'2.0','id':'other','result':{}})
        elif mode=='oversize':print('x'*(1024*1024+1),flush=True)
        else:reply(r,{'schema':{'version':2 if mode=='wrong-version' else 1},'sessionDurability':'ephemeral' if mode=='ephemeral' else 'durable'})
    elif method=='initialized':pass
    elif method in ('session/start','session/resume'):
        assert uuid.UUID(p['commandId']).version==7
        if method=='session/resume':state['sessionId']=p['sessionId']
        if mode=='changed-resume':state['sessionId']=str(uuid.uuid4())
        if mode=='wrong-workspace':state['workspaceRoot']='/'
        if mode=='busy':state['status']='running';state['activeTurnId']=str(uuid.uuid4())
        if method=='session/start':
            state['modelId']=None if mode=='ignored-model' else p.get('modelId')
            state['providerId']='wrong' if mode=='ignored-provider' else p.get('providerId','echo')
            state['approvalMode']={'mode':'onRequest' if mode=='ignored-mode' else p.get('approvalMode','onRequest')}
            for _ in range(9 if mode=='startup-flood' else 1):event('session/started',session=state)
        reply(r,{'session':state,'pendingRequests':[{'kind':'approval'}] if mode=='pending' else []})
        if mode=='blocked-write':time.sleep(30)
    elif method=='session/setModel':
        if mode!='ignored-model':state['modelId']=p['model']['modelId']
        if 'providerId' in p['model']:state['providerId']=p['model']['providerId']
        reply(r,{'commandId':p['commandId'],'status':'accepted'})
    elif method=='session/read':reply(r,{'session':state})
    elif method=='session/setApprovalMode':
        state['approvalMode']={'mode':'onRequest' if mode=='ignored-mode' else p['mode']}
        reply(r,{'commandId':p['commandId'],'status':'accepted','effectiveMode':state['approvalMode']})
    elif method=='turn/start':
        turn=str(uuid.uuid4()) # Never derive the returned native turn from commandId.
        assert uuid.UUID(p['commandId']).version==7
        if mode=='early-wrong-start':event('turn/started',sessionId=sid,turnId=str(uuid.uuid4()),commandId=p['commandId'])
        if mode=='early-completion':done()
        if mode=='incomplete-frame':
            sys.stdout.write('{"jsonrpc":');sys.stdout.flush();sys.exit(0)
        reply(r,{'commandId':p['commandId'],'status':'accepted','turnId':turn,'startedNewTurn':mode!='queued','disposition':'queued' if mode=='queued' else 'started'})
        if mode=='missing-completion':sys.exit(0)
        if mode in ('permission','only-persistent','stale-requirement'):
            choices=[{'choiceId':'forever','scope':'localPersistent','decision':'approvedPolicyAmendment'}]
            if mode!='only-persistent':choices += [{'choiceId':'once','scope':'once','decision':'approved'},{'choiceId':'deny','scope':'once','decision':'denied'}]
            send({'jsonrpc':'2.0','id':'presentation','method':'approval/request','params':{'sessionId':sid,'turnId':turn,'approvalId':'approval-1','currentRequirementId':{'approvalId':'approval-1','sourceIndex':42},'availableChoices':choices}})
        elif mode=='client-tool':
            send({'jsonrpc':'2.0','id':'tool','method':'terminal/create','params':{'sessionId':sid,'command':'touch '+os.environ['MSP_TEST_FORBIDDEN']}})
        elif mode!='early-completion':
            event('usage/changed',usage={'secret_counter':99})
            event('session/tokenUsage',sessionId=sid,usage={'inputTokens':99})
            done()
    elif method=='approval/decide':
        if mode=='stale-requirement':send({'jsonrpc':'2.0','id':r['id'],'error':{'code':-32001,'message':'stale'}})
        else:reply(r,{'commandId':p['commandId'],'status':'accepted'});done()
    elif 'result' in r or 'error' in r:pass # Presentation receipt, never a decision.
    else:raise AssertionError(method)
'''


class Bridge(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-msp-contract-")
        self.root = Path(self.temp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        executable = self.bin / "muse"
        executable.write_text(SERVER)
        executable.chmod(0o700)
        self.record = self.root / "requests.jsonl"
        self.forbidden = self.root / "must-not-exist"

    def tearDown(self):
        self.temp.cleanup()

    def invoke(self, mode="happy", **options):
        self.record.unlink(missing_ok=True)
        request = {"argv": ["muse", "serve", "--provider", "echo"], "cwd": str(self.root),
                   "roots": [str(self.root)], "prompt": "Literal $(touch never) --help", "permissions": "deny",
                   "timeout": 1, **options}
        run = subprocess.run([sys.executable, str(ROOT / "plugins/msp.py"), "--request", json.dumps(request)],
                             capture_output=True, text=True, timeout=5,
                             env={**os.environ, "PATH": str(self.bin)+os.pathsep+os.environ["PATH"],
                                  "MSP_TEST_MODE": mode, "MSP_TEST_RECORD": str(self.record),
                                  "MSP_TEST_FORBIDDEN": str(self.forbidden)})
        self.assertFalse(self.forbidden.exists(), "an unadvertised client tool executed")
        events = [json.loads(line) for line in run.stdout.splitlines()]
        records = [json.loads(line) for line in self.record.read_text().splitlines()] if self.record.exists() else []
        return run, events, records

    def test_owned_host_handshake_literal_input_and_native_turn_identity(self):
        run, events, records = self.invoke(model="fixture-model", provider="echo", effort="low", approval_mode="onRequest")
        self.assertEqual(run.returncode, 0, run.stdout+run.stderr)
        self.assertEqual([r.get('method') for r in records[:3]], ['initialize','initialized','session/start'])
        turn = next(r for r in records if r.get('method')=='turn/start')
        self.assertEqual(turn['params']['input'][0]['text'], 'Literal $(touch never) --help')
        self.assertEqual(turn['params']['reasoningEffort'], 'low')
        self.assertEqual(events[-1]['type'], 'turn.completed')
        self.assertNotEqual(events[-1]['turn_id'], turn['params']['commandId'])
        self.assertEqual(events[-1]['usage']['inputTokens'],12)
        self.assertEqual(sum('usage' in e for e in events),1, 'nested cumulative counters must not be counted twice')

    def test_early_completion_is_buffered_until_the_exact_acceptance_reply(self):
        run, events, _ = self.invoke('early-completion')
        self.assertEqual(run.returncode,0,run.stdout)
        self.assertEqual(events[-1]['type'],'turn.completed')

    def test_resume_loads_only_the_requested_idle_workspace_and_reselects_options(self):
        run, events, records = self.invoke(resume=SESSION,model='fixture-model',provider='echo',approval_mode='denyUnmatched')
        self.assertEqual(run.returncode,0,run.stdout)
        methods=[r.get('method') for r in records]
        self.assertIn('session/resume',methods)
        self.assertNotIn('session/start',methods)
        self.assertIn('session/setModel',methods)
        self.assertIn('session/setApprovalMode',methods)
        session=next(e for e in events if e['type']=='session.started')
        self.assertEqual(session['session_id'],SESSION)
        self.assertEqual(session['source'],'msp.resume')
        for mode in ['changed-resume','wrong-workspace','busy','pending','ignored-model','ignored-mode']:
            with self.subTest(mode=mode):
                run, _, records = self.invoke(mode,resume=SESSION,model='fixture-model',approval_mode='denyUnmatched')
                self.assertNotEqual(run.returncode,0)
                self.assertFalse(any(r.get('method') in ('session/start','turn/start') for r in records))

    def test_explicit_model_provider_and_mode_must_be_confirmed_before_prompt(self):
        for mode in ['ignored-model','ignored-provider','ignored-mode']:
            with self.subTest(mode=mode):
                run, _, records=self.invoke(mode,model='fixture-model',provider='echo',approval_mode='denyUnmatched')
                self.assertNotEqual(run.returncode,0)
                self.assertFalse(any(r.get('method')=='turn/start' for r in records))

    def test_approval_receipt_does_not_grant_and_decision_keeps_stage_and_once_scope(self):
        run, _, records=self.invoke('permission',permissions='allow_once')
        self.assertEqual(run.returncode,0,run.stdout)
        receipt=next(r for r in records if r.get('id')=='presentation')
        self.assertEqual(receipt['result'],{})
        decision=next(r for r in records if r.get('method')=='approval/decide')
        self.assertEqual(decision['params']['choiceId'],'once')
        self.assertEqual(decision['params']['requirementId'],{'approvalId':'approval-1','sourceIndex':42})
        for mode,opts in [('permission',{}),('only-persistent',{'permissions':'allow_once'}),('stale-requirement',{'permissions':'allow_once'})]:
            with self.subTest(mode=mode):
                run, events, records=self.invoke(mode,**opts)
                self.assertNotEqual(run.returncode,0)
                self.assertFalse(any(e['type']=='turn.completed' for e in events))
                self.assertFalse(any(r.get('params',{}).get('choiceId')=='forever' for r in records))
                self.assertLessEqual(sum(r.get('method')=='approval/decide' for r in records),1)

    def test_mismatched_malformed_incomplete_and_failed_native_turns_never_report_done(self):
        for mode in ['wrong-id','wrong-version','ephemeral','oversize','startup-flood','wrong-session','wrong-turn',
                     'early-wrong-start','queued','failed','cancelled','unknown-terminal','bad-exit','missing-completion','incomplete-frame','client-tool']:
            with self.subTest(mode=mode):
                run, events, _=self.invoke(mode)
                self.assertNotEqual(run.returncode,0,run.stdout)
                self.assertFalse(any(e['type']=='turn.completed' for e in events))

    def test_silent_or_blocked_host_is_bounded_and_invalid_deadline_launches_nothing(self):
        for mode in ['silent','blocked-write']:
            before=time.monotonic()
            run, events, _=self.invoke(mode,timeout=0.1,prompt='x'*32768)
            self.assertNotEqual(run.returncode,0)
            self.assertLess(time.monotonic()-before,3)
            self.assertFalse(any(e['type']=='turn.completed' for e in events))
        run, _, records=self.invoke(timeout=float('nan'))
        self.assertNotEqual(run.returncode,0)
        self.assertEqual(records,[])


if __name__ == '__main__':
    unittest.main()
