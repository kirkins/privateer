"""A stand-in upstream MCP server: newline-delimited JSON-RPC on stdio,
scripted by argv (the broker gives it a minimal environment). Its window
tool, target argument and fields come from a routing profile, the same file
the broker pins, so one fake serves every profile.

  --record FILE        append every message received, one JSON line each,
                       plus {"started": PID, "env": ENVIRONMENT} at start
  --init-delay SECS    answer initialize this late
  --crash-after N      exit after receiving the Nth tools/call, once: a marker
                       FILE.crashed beside --record keeps a restart from
                       crashing again
  --profile FILE       the routing profile (default: fake_profile.json beside
                       this file): routing.list.tool lists the windows,
                       routing.target_argument names one, and the list's
                       items, target_field, path_field, text_target and
                       text_path shape the answer
  --windows FILE       the open windows: a JSON list of {target, path}, read
                       at each list
  --windows-text       answer the list in the text form only (otherwise at
                       list.items, with structuredContent's JSON as text)

Tools (tools/call name):
  echo    {"text"}            answer with the text
  slow    {"secs"}            sleep, sending progress if asked, then answer;
                              a cancel for it means no answer
  notify  {}                  send notifications/message, then answer
  ask     {}                  send a server request, answer with the error
                              code the client side gave it
  crash   {}                  exit at once, mid-call
  LIST (the profile's list tool) {}   the windows in --windows
  Read {TARGET, filePath}     answer with the window and path it got
  Build {TARGET}              answer with the window it got
  Search {query}              window-free: answer with the query
  Grep {pattern}              window-free until `retool`, then it takes
                              TARGET too
  retool  {}                  give Grep a TARGET and send
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
parser.add_argument('--profile', default=os.path.join(os.path.dirname(__file__), 'fake_profile.json'))
options = parser.parse_args()
with open(options.profile) as stream:
    ROUTING = json.load(stream)['routing']
LIST = ROUTING['list']
TARGET = ROUTING['target_argument']

out_lock = threading.Lock()
cancelled = set()
asked = {}
calls = 0
retooled = False
WINDOW = {TARGET: {'type': 'string'}}


def tools():
    """Each tool's name and inputSchema properties."""
    listed = {name: {} for name in ('echo', 'slow', 'notify', 'ask', 'crash', 'retool')}
    listed.update({
        LIST['tool']: {},
        'Read': dict(WINDOW, filePath={'type': 'string'}),
        'Build': dict(WINDOW),
        'Search': {'query': {'type': 'string'}},
        'Grep': dict(WINDOW, pattern={'type': 'string'}) if retooled else {'pattern': {'type': 'string'}},
    })
    return [{'name': name, 'inputSchema': {'type': 'object', 'properties': properties}}
            for name, properties in listed.items()]


def at_pointer(pointer, value):
    """A result holding `value` at JSON pointer `pointer`."""
    parts = [p.replace('~1', '/').replace('~0', '~') for p in pointer.split('/')[1:]]
    for part in reversed(parts):
        value = {part: value}
    return value


def list_windows(request_id):
    windows = []
    if options.windows and os.path.exists(options.windows):
        with open(options.windows) as stream:
            windows = json.load(stream)
    if options.windows_text:
        lines = ['Open windows:'] + [
            f"* {LIST['text_target']}{w['target']}, {LIST['text_path']}{w['path']}" for w in windows]
        result = {'content': [{'type': 'text', 'text': '\n'.join(lines)}], 'isError': False}
    else:
        items = [{LIST['target_field']: w['target'], LIST['path_field']: w['path']} for w in windows]
        result = at_pointer(LIST['items'], items)
        mirror = result.get('structuredContent', result)
        result.update({'content': [{'type': 'text', 'text': json.dumps(mirror)}], 'isError': False})
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
    elif name == LIST['tool']:
        list_windows(request_id)
    elif name == 'Read':
        result(request_id, f"read {arguments.get('filePath')} in window {arguments.get(TARGET)}")
    elif name == 'Build':
        result(request_id, f"built in window {arguments.get(TARGET)}")
    elif name == 'Search':
        result(request_id, f"found {arguments.get('query')}")
    elif name == 'Grep':
        result(request_id, f"grep in window {arguments.get(TARGET)}")
    elif name == 'retool':
        retooled = True
        send({'jsonrpc': '2.0', 'method': 'notifications/tools/list_changed'})
        result(request_id, 'retooled')
    else:
        send({'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32602, 'message': 'unknown tool'}})


def main():
    record({'started': os.getpid(), 'env': dict(os.environ)})
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
                'serverInfo': {'name': 'fake-upstream', 'version': '1.0'}}})
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
