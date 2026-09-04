#!/usr/bin/env node
/** Publish one conformance route through the dataplane's current serializer. */

const DATAPLANE_CONFIG_URL =
  'http://dataplane:4445/contextforge-rs/admin/userconfigs';
const FIXTURE_TOOLS = [
  'test_simple_text',
  'test_image_content',
  'test_audio_content',
  'test_embedded_resource',
  'test_multiple_content_types',
  'test_tool_with_logging',
  'test_tool_with_progress',
  'test_error_handling',
  'test_reconnection',
  'test_sampling',
  'test_elicitation',
  'test_elicitation_sep1034_defaults',
  'test_elicitation_sep1330_enums',
  'json_schema_2020_12_tool',
];
const FIXTURE_RESOURCES = [
  'test://static-text',
  'test://static-binary',
  'test://watched-resource',
];
const FIXTURE_RESOURCE_TEMPLATES = ['test://template/{id}/data'];
const FIXTURE_PROMPTS = [
  'test_simple_prompt',
  'test_prompt_with_arguments',
  'test_prompt_with_embedded_resource',
  'test_prompt_with_image',
];

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

function config(serverId, backendUrl, protocolVersion, catalogs) {
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
            tool_schemas: Object.fromEntries(catalogs.tools.map((name) => [name, {}])),
            // Older published dataplanes used aliases instead of service routes.
            tool_name_aliases: catalogs.tools.map((name) => ({
              downstream_prefixed_name: name,
              upstream_name: name,
            })),
            resource_uri_aliases: catalogs.resources.map((name) => ({
              downstream_prefixed_uri: name,
              upstream_uri: name,
            })),
            prompt_name_aliases: catalogs.prompts.map((name) => ({
              downstream_prefixed_name: name,
              upstream_name: name,
            })),
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
  const endpoint = `${DATAPLANE_CONFIG_URL}/${encodeURIComponent(subject)}`;
  let lastError = 'dataplane did not respond';
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(endpoint, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(2000),
      });
      if (response.status === 202) return;
      lastError = `HTTP ${response.status}: ${(await response.text()).slice(0, 512)}`;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  fail(`dataplane config serializer was unavailable: ${lastError}`);
}

async function main() {
  const [mode, serverId, backendUrl, protocolVersion, toolNamesJson] = process.argv.slice(2);
  if (!['fixture', 'client'].includes(mode)) {
    fail('mode must be fixture or client');
  }
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

  const catalogs = mode === 'fixture'
    ? {
        tools: FIXTURE_TOOLS,
        resources: FIXTURE_RESOURCES,
        resourceTemplates: FIXTURE_RESOURCE_TEMPLATES,
        prompts: FIXTURE_PROMPTS,
      }
    : {
        tools: stringArray(toolNamesJson, 'tool-names-json'),
        resources: [],
        resourceTemplates: [],
        prompts: [],
      };
  await publish(
    tokenSubject(token),
    config(serverId, backendUrl, protocolVersion, catalogs),
  );
}

await main();
