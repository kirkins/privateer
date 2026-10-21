"""A stand-in for `xcrun mcpbridge`: a newline-delimited JSON-RPC MCP server
on stdio, scripted by argv (the broker gives it a minimal environment).

  --record FILE        append every message received, one JSON line each,
                       plus {"started": PID} at start
  --init-delay SECS    answer initialize this late
  --crash-after N      exit after receiving the Nth tools/call, once: a marker
                       FILE.crashed beside --record keeps a restart from
                       crashing again
  --windows FILE       Xcode's windows: a JSON list of {tabIdentifier,
                       workspacePath}, read at each XcodeListWindows
  --windows-text       answer XcodeListWindows in the text form only
                       (otherwise structuredContent, with the same JSON as text)

Tools (tools/call name):
  echo    {"text"}            answer with the text
  slow    {"secs"}            sleep, sending progress if asked, then answer;
                              a cancel for it means no answer
  notify  {}                  send notifications/message, then answer
  ask     {}                  send a server request, answer with the error
                              code the client side gave it
  crash   {}                  exit at once, mid-call
  XcodeListWindows {}         the windows in --windows
  XcodeRead {tabIdentifier, filePath}   answer with the tab and path it got
  BuildProject {tabIdentifier}          answer with the tab it got
  DocumentationSearch {query}           window-free: answer with the query
  XcodeGrep {pattern}         window-free until `retool`, then it takes
                              tabIdentifier too
  retool  {}                  give XcodeGrep a tabIdentifier and send
                              notifications/tools/list_changed
"""
import argparse
import json
import os
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--record')
parser.add_argument('--init-delay', type=float, default=0)
parser.add_argument('--crash-after', type=int, default=0)
parser.add_argument('--windows')
parser.add_argument('--windows-text', action='store_true')
options = parser.parse_args()

out_lock = threading.Lock()
cancelled = set()
asked = {}
calls = 0
retooled = False
TAB = {'tabIdentifier': {'type': 'string'}}


def tools():
    """Each tool's name and inputSchema properties."""
    listed = {name: {} for name in ('echo', 'slow', 'notify', 'ask', 'crash', 'retool')}
    listed.update({
        'XcodeListWindows': {},
        'XcodeRead': dict(TAB, filePath={'type': 'string'}),
        'BuildProject': dict(TAB),
        'DocumentationSearch': {'query': {'type': 'string'}},
        'XcodeGrep': dict(TAB, pattern={'type': 'string'}) if retooled else {'pattern': {'type': 'string'}},
    })
    return [{'name': name, 'inputSchema': {'type': 'object', 'properties': properties}}
            for name, properties in listed.items()]


def list_windows(request_id):
    windows = []
    if options.windows and os.path.exists(options.windows):
        with open(options.windows) as stream:
            windows = json.load(stream)
    if options.windows_text:
        lines = ['Open Xcode windows:'] + [
            f"* tabIdentifier: {w['tabIdentifier']}, workspacePath: {w['workspacePath']}" for w in windows]
        result = {'content': [{'type': 'text', 'text': '\n'.join(lines)}], 'isError': False}
    else:
        body = {'windows': windows}
        result = {'content': [{'type': 'text', 'text': json.dumps(body)}],
                  'structuredContent': body, 'isError': False}
    send({'jsonrpc': '2.0', 'id': request_id, 'result': result})


def record(message):
    if options.record:
        with open(options.record, 'a') as stream:
            stream.write(json.dumps(message) + '\n')


def send(message):
    with out_lock:
        sys.stdout.write(json.dumps(message) + '\n')
        sys.stdout.flush()


def result(request_id, text):
    send({'jsonrpc': '2.0', 'id': request_id,
          'result': {'content': [{'type': 'text', 'text': text}], 'isError': False}})


def slow(request_id, arguments, token):
    secs = float(arguments.get('secs', 1))
    steps = 4
    for step in range(steps):
        time.sleep(secs / steps)
        if request_id in cancelled:
            return
        if token is not None:
            send({'jsonrpc': '2.0', 'method': 'notifications/progress',
                  'params': {'progressToken': token, 'progress': step + 1, 'total': steps}})
    if request_id not in cancelled:
        result(request_id, 'slept')


def ask(request_id):
    asked_id = f'srv-{request_id}'
    event = threading.Event()
    asked[asked_id] = (event, None)
    send({'jsonrpc': '2.0', 'id': asked_id, 'method': 'sampling/createMessage',
          'params': {'messages': [], 'maxTokens': 1}})
    event.wait(10)
    answer = asked.pop(asked_id, (None, None))[1] or {}
    result(request_id, json.dumps(answer.get('error', {}).get('code')))


def call(request_id, params):
    global calls, retooled
    calls += 1
    name = params.get('name')
    arguments = params.get('arguments') or {}
    token = (params.get('_meta') or {}).get('progressToken')
    marker = (options.record or '/nonexistent') + '.crashed'
    if name == 'crash' or (options.crash_after and calls >= options.crash_after
                           and not os.path.exists(marker)):
        if options.record:
            open(marker, 'w').close()
        os._exit(3)
    if name == 'echo':
        result(request_id, str(arguments.get('text', '')))
    elif name == 'slow':
        threading.Thread(target=slow, args=(request_id, arguments, token), daemon=True).start()
    elif name == 'notify':
        send({'jsonrpc': '2.0', 'method': 'notifications/message',
              'params': {'level': 'info', 'data': 'hello everyone'}})
        result(request_id, 'notified')
    elif name == 'ask':
        threading.Thread(target=ask, args=(request_id,), daemon=True).start()
    elif name == 'XcodeListWindows':
        list_windows(request_id)
    elif name == 'XcodeRead':
        result(request_id, f"read {arguments.get('filePath')} in tab {arguments.get('tabIdentifier')}")
    elif name == 'BuildProject':
        result(request_id, f"built in tab {arguments.get('tabIdentifier')}")
    elif name == 'DocumentationSearch':
        result(request_id, f"found {arguments.get('query')}")
    elif name == 'XcodeGrep':
        result(request_id, f"grep in tab {arguments.get('tabIdentifier')}")
    elif name == 'retool':
        retooled = True
        send({'jsonrpc': '2.0', 'method': 'notifications/tools/list_changed'})
        result(request_id, 'retooled')
    else:
        send({'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32602, 'message': 'unknown tool'}})


def main():
    record({'started': os.getpid()})
    for raw in sys.stdin:
        message = json.loads(raw)
        record(message)
        method = message.get('method')
        request_id = message.get('id')
        if method is None and request_id in asked:
            asked[request_id] = (asked[request_id][0], message)
            asked[request_id][0].set()
        elif method == 'initialize':
            time.sleep(options.init_delay)
            send({'jsonrpc': '2.0', 'id': request_id, 'result': {
                'protocolVersion': message['params']['protocolVersion'],
                'capabilities': {'tools': {}},
                'serverInfo': {'name': 'fake-mcpbridge', 'version': '1.0'}}})
        elif method == 'tools/list':
            send({'jsonrpc': '2.0', 'id': request_id, 'result': {'tools': tools()}})
        elif method == 'tools/call':
            call(request_id, message.get('params') or {})
        elif method == 'notifications/cancelled':
            cancelled.add(message['params']['requestId'])
        elif request_id is not None:
            send({'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32601, 'message': 'no'}})


if __name__ == '__main__':
    main()
