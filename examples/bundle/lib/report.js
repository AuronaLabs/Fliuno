const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
function attempt(operation) { try { operation(); return true; } catch { return false; } }
exports.inspect = async (hostPath, endpoint) => {
  const [host, port] = endpoint.split(':');
  const network = await new Promise(resolve => {
    const socket = net.createConnection({ host, port: Number(port) });
    const finish = value => { socket.destroy(); resolve(value); };
    socket.setTimeout(1000, () => finish(false));
    socket.on('error', () => finish(false));
    socket.on('connect', () => finish(true));
  });
  return {
    runtime: 'node', runtimeVersion: process.versions.node,
    data: JSON.parse(fs.readFileSync(path.join(__dirname, '../data.json'), 'utf8')),
    hostRead: attempt(() => fs.readFileSync(hostPath)),
    hostWrite: attempt(() => fs.writeFileSync(hostPath, 'changed by native control')),
    scratchWrite: attempt(() => fs.writeFileSync('inside.txt', 'private output')),
    secretSeen: Object.hasOwn(process.env, 'FLIUNO_BUNDLE_SECRET'), network,
  };
};
