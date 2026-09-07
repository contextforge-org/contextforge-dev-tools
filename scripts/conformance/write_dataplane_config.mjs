#!/usr/bin/env node
/** Publish a test routing snapshot directly to the harness Redis. */
import { sign } from 'node:crypto';
import { readFileSync, realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

/** Discover the pinned fixture instead of maintaining a second, incomplete catalog. */
export async function fixtureCatalog(backendUrl, protocolVersion) {
  let requestId = 0;
  let sessionId;
  const modern = protocolVersion >= '2026-07-28';
  const clientInfo = { name: 'cf-integration-config', version: '1.0' };
  async function rpc(method, params = {}, notification = false) {
    if (modern) params = { ...params, _meta: {
      'io.modelcontextprotocol/protocolVersion': protocolVersion,
      'io.modelcontextprotocol/clientInfo': clientInfo,
      'io.modelcontextprotocol/clientCapabilities': {},
    } };
    const id = notification ? undefined : ++requestId;
    const response = await fetch(backendUrl, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        accept: 'application/json, text/event-stream',
        ...(method === 'initialize' ? {} : { 'mcp-protocol-version': protocolVersion }),
        ...(modern ? { 'mcp-method': method } : {}),
        ...(sessionId ? { 'mcp-session-id': sessionId } : {}),
      },
      body: JSON.stringify({ jsonrpc: '2.0', id, method, params }),
      signal: AbortSignal.timeout(10000),
    });
    if (!response.ok) throw new Error(`fixture ${method} failed: HTTP ${response.status}`);
    sessionId = response.headers.get('mcp-session-id') ?? sessionId;
    if (notification) { await response.body?.cancel(); return; }
    const body = await response.text();
    const messages = response.headers.get('content-type')?.startsWith('text/event-stream')
      ? body.split(/\r?\n\r?\n/).map((event) =>
          event.split(/\r?\n/).filter((line) => line.startsWith('data:'))
            .map((line) => line.slice(5).trimStart()).join('\n'))
          .filter((data) => data.trim()).map((data) => JSON.parse(data))
      : [JSON.parse(body)];
    const message = messages.find((message) => message.id === id);
    if (!message || message.error || !message.result) {
      throw new Error(`fixture ${method} did not return a successful result`);
    }
    return message.result;
  }
  async function list(method, key) {
    const items = [];
    const cursors = new Set();
    let cursor;
    do {
      const page = await rpc(method, cursor === undefined ? {} : { cursor });
      if (!Array.isArray(page[key])) throw new Error(`fixture ${method} has no ${key} array`);
      items.push(...page[key]);
      cursor = page.nextCursor ?? undefined;
      if (cursor !== undefined && (typeof cursor !== 'string' || cursors.has(cursor))) {
        throw new Error(`fixture ${method} returned an invalid or repeated cursor`);
      }
      cursors.add(cursor);
    } while (cursor !== undefined);
    return items;
  }
  try {
    if (modern) await rpc('server/discover');
    else {
      await rpc('initialize', { protocolVersion, capabilities: {}, clientInfo });
      await rpc('notifications/initialized', {}, true);
    }
    const tools = await list('tools/list', 'tools');
    if (!tools.length || tools.some((tool) => !tool.name || !tool.inputSchema
        || typeof tool.inputSchema !== 'object' || Array.isArray(tool.inputSchema))) {
      throw new Error('fixture tools must include names and input schemas');
    }
    return {
      tools: tools.map((tool) => tool.name),
      toolSchemas: Object.fromEntries(tools.map((tool) => [tool.name, tool.inputSchema])),
      resources: (await list('resources/list', 'resources')).map((resource) => resource.uri),
      resourceTemplates: (await list('resources/templates/list', 'resourceTemplates')).map((resource) => resource.uriTemplate),
      prompts: (await list('prompts/list', 'prompts')).map((prompt) => prompt.name),
    };
  } finally {
    if (sessionId) await fetch(backendUrl, {
      method: 'DELETE',
      headers: { 'mcp-session-id': sessionId, 'mcp-protocol-version': protocolVersion },
      signal: AbortSignal.timeout(10000),
    }).then((response) => response.body?.cancel());
  }
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function tokenSubject(token) {
  const parts = token.split('.');
  if (parts.length !== 3) fail('MCP_CONFORMANCE_TOKEN is not a JWT');
  let claims;
  try {
    claims = JSON.parse(Buffer.from(parts[1], 'base64url').toString('utf8'));
  } catch {
    fail('MCP_CONFORMANCE_TOKEN has invalid claims');
  }
  if (typeof claims.sub !== 'string' || claims.sub.length === 0) {
    fail('MCP_CONFORMANCE_TOKEN has no string subject');
  }
  return claims.sub;
}

function stringArray(value, label) {
  let parsed;
  try {
    parsed = JSON.parse(value);
  } catch {
    fail(`${label} must be valid JSON`);
  }
  if (!Array.isArray(parsed) || parsed.some((item) => typeof item !== 'string' || !item)) {
    fail(`${label} must be a JSON string array`);
  }
  return parsed;
}

function routes(names, backendName) {
  return Object.fromEntries(
    names.map((name) => [name, { backend_name: backendName, upstream_name: name }]),
  );
}

export function config(serverId, backendUrl, protocolVersion, catalogs) {
  const backendName = 'conformance-backend';
  return {
    virtual_hosts: {
      [serverId]: {
        backends: {
          [backendName]: {
            name: backendName,
            url: backendUrl,
            mcp_protocol_version: protocolVersion,
            passthrough_headers: [],
            add_headers: {},
            remove_headers: [],
            completion: {},
            tool_schemas: catalogs.toolSchemas,
          },
        },
        tools: routes(catalogs.tools, backendName),
        resources: routes(catalogs.resources, backendName),
        resource_templates: routes(catalogs.resourceTemplates, backendName),
        prompts: routes(catalogs.prompts, backendName),
      },
    },
  };
}

async function publish(subject, body) {
  const { encode } = await import('@msgpack/msgpack');
  const { createClient } = await import('@redis/client');
  const client = createClient({
    url: process.env.CF_CONFIG_REDIS_URL ?? 'redis://redis:6379',
    socket: { connectTimeout: 10000, reconnectStrategy: false },
  });
  client.on('error', (error) => process.stderr.write(`Redis: ${error.message}\n`));
  try {
    await client.connect();
    // User::new(subject) uses the compact [KeyType::UserConfig, subject] key.
    // Named maps preserve empty maps and avoid depending on Rust field order.
    await client.set(Buffer.from(encode(['UserConfig', subject])), Buffer.from(encode(body)));
  } finally {
    if (client.isOpen) client.destroy();
  }
}

export function issueToken(tenantId, userId, privateKey = readFileSync('/keys/jwt.key')) {
  const now = Math.floor(Date.now() / 1000);
  const header = { alg: 'RS256', typ: 'JWT', kid: 'cf-integration-standalone' };
  const claims = { sub: userId, tenant_id: tenantId, iat: now, nbf: now, exp: now + 86400 };
  const payload = [header, claims].map((value) =>
    Buffer.from(JSON.stringify(value)).toString('base64url')).join('.');
  return `${payload}.${sign('RSA-SHA256', Buffer.from(payload), privateKey).toString('base64url')}`;
}

async function main() {
  const [mode, ...args] = process.argv.slice(2);
  if (mode === 'token') {
    const [tenantId, userId] = args;
    if (!tenantId) fail('tenant-id must not be empty');
    if (!userId) fail('user-id must not be empty');
    process.stdout.write(`${await issueToken(tenantId, userId)}\n`);
    return;
  }
  if (!['fixture', 'client'].includes(mode)) {
    fail('mode must be token, fixture, or client');
  }
  const [serverId, backendUrl, protocolVersion, toolNamesJson] = args;
  if (!serverId) fail('virtual-host-id must not be empty');
  try {
    const parsed = new URL(backendUrl);
    if (!['http:', 'https:'].includes(parsed.protocol)) fail('backend-url must use HTTP(S)');
  } catch {
    fail('backend-url must be an absolute HTTP(S) URL');
  }
  if (!protocolVersion) fail('protocol-version must not be empty');
  const token = process.env.MCP_CONFORMANCE_TOKEN;
  if (!token) fail('MCP_CONFORMANCE_TOKEN is required');

  const tools = mode === 'client' ? stringArray(toolNamesJson, 'tool-names-json') : undefined;
  const catalogs = mode === 'fixture'
    ? await fixtureCatalog(backendUrl, protocolVersion)
    : {
        tools,
        toolSchemas: Object.fromEntries(tools.map((name) => [name, {}])),
        resources: [],
        resourceTemplates: [],
        prompts: [],
      };
  await publish(
    tokenSubject(token),
    config(serverId, backendUrl, protocolVersion, catalogs),
  );
  process.stdout.write(`${JSON.stringify(catalogs.tools)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) await main();
