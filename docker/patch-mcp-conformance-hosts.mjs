import { readFile, writeFile } from 'node:fs/promises';

const target = process.argv[2];
if (!target) {
  throw new Error('usage: patch-mcp-conformance-hosts.mjs <path>');
}

function replaceExactlyOnce(source, oldText, replacement, label) {
  const replacementCount = source.split(oldText).length - 1;
  if (replacementCount !== 1) {
    throw new Error(`expected exactly one ${label} patch target, found ${replacementCount}`);
  }
  return source.replace(oldText, replacement);
}

const hostOld = 'const app = createMcpExpressApp();';
const hostReplacement =
  "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });";

const versionListOld = `const LEGACY_SESSION_PROTOCOL_VERSIONS = [
  '2024-11-05',
  '2025-03-26',
  '2025-06-18',
  '2025-11-25'
];`;
const versionListReplacement = `${versionListOld}

// Harness-only switch for exercising explicit cross-era gateway paths.
const CONFORMANCE_SERVER_ERA = process.env.MCP_CONFORMANCE_SERVER_ERA;
if (!['dual', 'legacy', 'modern'].includes(CONFORMANCE_SERVER_ERA)) {
  throw new Error(
    \`invalid MCP_CONFORMANCE_SERVER_ERA: \${CONFORMANCE_SERVER_ERA}\`
  );
}`;

const requestClassificationOld = `  const isLegacySessionEraRequest =
    meta === undefined &&
    reqVersion !== undefined &&
    LEGACY_SESSION_PROTOCOL_VERSIONS.includes(reqVersion);

  if (!sessionId && (reqVersion || meta) && !isLegacySessionEraRequest) {`;
const requestClassificationReplacement = `  const isLegacySessionEraRequest =
    meta === undefined &&
    reqVersion !== undefined &&
    LEGACY_SESSION_PROTOCOL_VERSIONS.includes(reqVersion);
  const isModernEraRequest =
    !sessionId && (reqVersion !== undefined || meta !== undefined) &&
    !isLegacySessionEraRequest;

  // A legacy-only server must return a non-modern 4xx so a dual-era client
  // recognizes the server as legacy and retries with initialize.
  if (CONFORMANCE_SERVER_ERA === 'legacy' && isModernEraRequest) {
    return res.status(400).json({
      jsonrpc: '2.0',
      id,
      error: { code: -32601, message: 'Method not found' }
    });
  }

  // A modern-only server rejects initialization and names the only version it
  // supports, giving legacy clients the most actionable failure available.
  if (
    CONFORMANCE_SERVER_ERA === 'modern' &&
    isInitializeRequest(body) &&
    !isModernEraRequest
  ) {
    return res.status(400).json({
      jsonrpc: '2.0',
      id,
      error: {
        code: -32022,
        message: 'UnsupportedProtocolVersionError',
        data: {
          supported: ['2026-07-28'],
          requested: String(reqVersion ?? params.protocolVersion ?? 'legacy')
        }
      }
    });
  }

  if (isModernEraRequest) {`;

let source = await readFile(target, 'utf8');
source = replaceExactlyOnce(source, hostOld, hostReplacement, 'host');
source = replaceExactlyOnce(
  source,
  versionListOld,
  versionListReplacement,
  'server-era configuration'
);
source = replaceExactlyOnce(
  source,
  requestClassificationOld,
  requestClassificationReplacement,
  'server-era routing'
);
await writeFile(target, source);
