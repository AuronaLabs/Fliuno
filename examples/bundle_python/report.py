import json
import os
import socket
import sys


def attempt(operation):
    try:
        operation()
        return True
    except OSError:
        return False


def inspect(host_path, endpoint):
    host, port = endpoint.split(':')
    with socket.socket() as connection:
        connection.settimeout(1)
        network = attempt(lambda: connection.connect((host, int(port))))
    with open(os.path.join(os.path.dirname(__file__), 'data.json'), encoding='utf-8') as source:
        data = json.load(source)

    def read_host():
        with open(host_path, 'rb') as source:
            source.read()

    def write(path, contents):
        with open(path, 'w', encoding='utf-8') as output:
            output.write(contents)

    return dict(runtime='python', runtimeVersion=sys.version.split()[0],
                data=data, hostRead=attempt(read_host),
                hostWrite=attempt(lambda: write(host_path, 'changed by native control')),
                scratchWrite=attempt(lambda: write('inside.txt', 'private output')),
                secretSeen='FLIUNO_BUNDLE_SECRET' in os.environ, network=network)
