"""Offline MCP fixture using the actual CPython interpreter and staged modules."""
import json
import sys
from report import inspect


if len(sys.argv) > 1 and sys.argv[1] == '--probe':
    print(json.dumps(inspect(sys.argv[2], sys.argv[3])))
else:
    calls = 0
    for line in sys.stdin:
        request = json.loads(line)
        if 'id' not in request:
            continue
        method = request['method']
        if method == 'initialize':
            result = dict(protocolVersion=request['params']['protocolVersion'],
                          capabilities=dict(tools={}),
                          serverInfo=dict(name='fliuno-python-bundle-fixture', version='1'))
        elif method == 'tools/list':
            result = dict(tools=[dict(name='inspect', description='Inspect pinned Python companions',
                inputSchema=dict(type='object', properties=dict(hostPath=dict(type='string'),
                endpoint=dict(type='string')), required=['hostPath', 'endpoint'], additionalProperties=False))])
        elif method == 'tools/call':
            arguments = request['params']['arguments']
            calls += 1
            report = dict(inspect(arguments['hostPath'], arguments['endpoint']), calls=calls)
            result = dict(content=[dict(type='text', text=json.dumps(report))])
        elif method == 'ping':
            result = {}
        else:
            print(json.dumps(dict(jsonrpc='2.0', id=request['id'],
                  error=dict(code=-32601, message='fixture method unavailable'))), flush=True)
            continue
        print(json.dumps(dict(jsonrpc='2.0', id=request['id'], result=result)), flush=True)
