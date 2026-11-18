// Deliberately small offline MCP fixture. Built-ins need no package download.
const { inspect } = require('./lib/report.js');
if (process.argv[2] === '--probe') {
  inspect(process.argv[3], process.argv[4]).then(value => console.log(JSON.stringify(value)));
} else {
  const lines = require('node:readline').createInterface({ input: process.stdin });
  let calls = 0;
  lines.on('line', async line => {
    const request = JSON.parse(line);
    if (!Object.hasOwn(request, 'id')) return;
    let result;
    switch (request.method) {
      case 'initialize':
        result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} },
          serverInfo: { name: 'fliuno-bundle-fixture', version: '1' } };
        break;
      case 'tools/list':
        result = { tools: [{ name: 'inspect', description: 'Inspect explicit staged companions and isolation',
          inputSchema: { type: 'object', properties: { hostPath: { type: 'string' }, endpoint: { type: 'string' } },
            required: ['hostPath', 'endpoint'], additionalProperties: false } }] };
        break;
      case 'tools/call': {
        const args = request.params.arguments;
        const report = await inspect(args.hostPath, args.endpoint);
        result = { content: [{ type: 'text', text: JSON.stringify({ ...report, calls: ++calls }) }] };
        break;
      }
      case 'ping': result = {}; break;
      default:
        process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id,
          error: { code: -32601, message: 'fixture method unavailable' } }) + '\n');
        return;
    }
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
  });
}
