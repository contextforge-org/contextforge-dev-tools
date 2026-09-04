#!/usr/bin/env node
/** Generate the ephemeral RSA key used by the standalone dataplane stack. */

import { generateKeyPairSync } from 'node:crypto';
import { chmodSync, existsSync, writeFileSync } from 'node:fs';

const [outputPath] = process.argv.slice(2);
if (!outputPath) {
  process.stderr.write('output-path is required\n');
  process.exit(1);
}

if (existsSync(outputPath)) {
  chmodSync(outputPath, 0o600);
  process.exit(0);
}

const { privateKey } = generateKeyPairSync('rsa', {
  modulusLength: 2048,
  privateKeyEncoding: { type: 'pkcs8', format: 'pem' },
  publicKeyEncoding: { type: 'spki', format: 'pem' },
});

writeFileSync(outputPath, privateKey, { encoding: 'utf8', mode: 0o600 });
chmodSync(outputPath, 0o600);
