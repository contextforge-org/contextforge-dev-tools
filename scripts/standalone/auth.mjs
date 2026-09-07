#!/usr/bin/env node
/** Own the ephemeral signing key and loopback JWKS for standalone tests. */
import { createPublicKey, generateKeyPairSync } from 'node:crypto';
import { chmodSync, existsSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { pathToFileURL } from 'node:url';

export function startAuth(keyPath = '/keys/jwt.key', port = 4446) {
  if (!existsSync(keyPath)) {
    const { privateKey } = generateKeyPairSync('rsa', {
      modulusLength: 2048,
      privateKeyEncoding: { type: 'pkcs8', format: 'pem' },
      publicKeyEncoding: { type: 'spki', format: 'pem' },
    });
    writeFileSync(keyPath, privateKey, { mode: 0o600 });
  }
  chmodSync(keyPath, 0o600);
  const jwk = createPublicKey(readFileSync(keyPath)).export({ format: 'jwk' });
  const jwks = JSON.stringify({ keys: [{
    ...jwk, kid: 'cf-integration-standalone', alg: 'RS256', use: 'sig',
  }] });
  const server = createServer((request, response) => {
    if (request.url !== '/.well-known/jwks.json') {
      response.writeHead(404).end();
    } else if (!['GET', 'HEAD'].includes(request.method)) {
      response.writeHead(405, { allow: 'GET, HEAD' }).end();
    } else {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(request.method === 'HEAD' ? undefined : jwks);
    }
  });
  return server.listen(port, '127.0.0.1');
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  const server = startAuth();
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => server.close());
}
