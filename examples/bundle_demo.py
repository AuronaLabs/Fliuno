"""Real Node/CPython MCP in private isolation with pinned runtime companions.

Only the model is a loopback fixture. Uses a locally installed native runtime;
no package install or download. Controls prove host file/TCP access
before testing isolation. --tamper changes a temporary copy, never repo files.
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading

from rpc_client import LocalKernel, build_server


def model_fixture(host_path, endpoint):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            previous = next((v for v in body['input'] if v.get('type') == 'function_call_output'), None)
            if previous:
                output = [{'type': 'message', 'content': [{'type': 'output_text', 'text': 'Bundle result observed.'}]}]
            else:
                name = next(t['name'] for t in body['tools'] if t['name'].startswith('mcp_bundle_'))
                output = [{'type': 'function_call', 'call_id': 'bundle-once', 'name': name,
                           'arguments': json.dumps({'hostPath': str(host_path), 'endpoint': endpoint})}]
            encoded = json.dumps({'status': 'completed', 'output': output}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', choices=['node', 'python'], default='node')
    parser.add_argument('--node', default=shutil.which('node'))
    parser.add_argument('--python', default=sys.executable)
    parser.add_argument('--deny', action='store_true')
    parser.add_argument('--tamper', action='store_true')
    args = parser.parse_args()
    if args.runtime == 'node' and not args.node:
        raise SystemExit('Install Node or pass --node with its native executable path.')
    executable = build_server()
    runtime = str(Path(args.node if args.runtime == 'node' else args.python).resolve())
    script = 'server.js' if args.runtime == 'node' else 'server.py'
    runtime_args = (['--preserve-symlinks', '--preserve-symlinks-main', '--max-old-space-size=64']
                    if args.runtime == 'node' else ['-u', '-B', '-S'])
    with tempfile.TemporaryDirectory(prefix='fliuno-bundle-') as temporary, socket.socket() as listener:
        root = Path(temporary)
        source = root / 'sources'
        shutil.copytree(Path(__file__).with_name('bundle' if args.runtime == 'node' else 'bundle_python'), source)
        host_path = root / 'private.txt'
        host_path.write_text('private host bytes', encoding='utf-8')
        listener.bind(('127.0.0.1', 0))
        listener.listen(8)
        endpoint = f'127.0.0.1:{listener.getsockname()[1]}'
        environment = dict(os.environ, FLIUNO_BUNDLE_SECRET='fixture-hidden')
        control_directory = root / 'control'
        control_directory.mkdir()
        control = subprocess.run([runtime, *(['-B'] if args.runtime == 'python' else []),
                                  str(source / script), '--probe', str(host_path), endpoint],
                                 cwd=control_directory, env=environment, capture_output=True, text=True,
                                 encoding='utf-8', timeout=15, check=True)
        report = json.loads(control.stdout)
        control_version = report['runtimeVersion']
        assert all(report[field] for field in ['hostRead', 'hostWrite', 'network', 'scratchWrite', 'secretSeen']), report
        host_path.write_text('private host bytes', encoding='utf-8')
        if args.runtime == 'python':
            from python_runtime import stage_runtime
            stage_runtime(runtime, source)
        assets = [{'source': str(path), 'path': path.relative_to(source).as_posix(),
                   'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                  for path in sorted(source.rglob('*')) if path.is_file()]
        config = root / 'mcp.json'
        config.write_text(json.dumps({'servers': [{
            # Without both flags Node realpaths the drive root on Windows,
            # outside AppContainer storage. These are host runtime arguments;
            # no host filesystem ACL is expanded to make the runtime work.
            'id': 'bundle', 'command': runtime, 'args': [*runtime_args, script],
            'isolation': 'windows_app_container' if os.name == 'nt' else 'linux_landlock',
            'assets': assets, 'timeoutMs': 30000,
        }]}), encoding='utf-8')
        if args.tamper:
            (source / 'data.json').write_text('{"changed":true}', encoding='utf-8')
            failed = subprocess.run([str(executable), '--db', str(root / 'kernel.db'), '--mcp-config', str(config)],
                                    input='', capture_output=True, text=True, encoding='utf-8',
                                    timeout=45, env=dict(environment, FLIUNO_PROVIDER='demo'))
            assert (failed.returncode != 0 and not failed.stdout
                    and 'MCP sandbox launch failed' in failed.stderr), failed
            print('Changed resource rejected before MCP initialization; no model or tool call.')
            return
        server, thread = model_fixture(host_path, endpoint)
        try:
            environment.update(FLIUNO_PROVIDER='openai', FLIUNO_MODEL='offline-fixture',
                               OPENAI_BASE_URL=f'http://127.0.0.1:{server.server_port}/v1/', OPENAI_API_KEY='fixture-only')
            with LocalKernel(executable, ['--db', root / 'kernel.db', '--mcp-config', config], environment) as kernel:
                kernel.call('initialize', {'protocolVersion': '0.1', 'clientName': 'bundle-demo'}, timeout=45)
                workspace = kernel.call('workspace/open', {'root': str(root)})
                session = kernel.call('session/create', {'workspaceId': workspace['id']})
                task = kernel.call('task/start', {'sessionId': session['id'], 'input': 'Inspect the staged runtime.'})
                waiting = kernel.wait(task['id'], 'waiting_approval')
                kernel.call('approval/respond', {'taskId': task['id'], 'approvalId': waiting['pendingApproval']['id'],
                                                 'decision': 'deny' if args.deny else 'allow_once'})
                done = kernel.wait(task['id'], 'failed' if args.deny else 'completed')
                events = kernel.call('events/list', {'taskId': task['id']})['events']
                completed = [event['data'] for event in events if event['data']['type'] == 'tool_completed']
                if args.deny:
                    assert not completed
                else:
                    report = json.loads(completed[0]['output']['content'][0]['text'])
                    assert report['runtime'] == args.runtime and report['runtimeVersion'] == control_version
                    assert report['data'] == json.loads((source / 'data.json').read_text(encoding='utf-8'))
                    assert report['scratchWrite'] and report['calls'] == 1, report
                    assert all(not report[field] for field in ['hostRead', 'hostWrite', 'network', 'secretSeen']), report
                    print(f'Isolated {args.runtime} result:', json.dumps(report, ensure_ascii=False))
                assert host_path.read_text(encoding='utf-8') == 'private host bytes'
                print('Pinned companions:', len(assets), '/ task:', done['status'])
                print('Approval:', 'deny' if args.deny else 'allow_once')
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == '__main__':
    sys.stdout.reconfigure(encoding='utf-8')
    main()
