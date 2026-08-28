FROM node:22-bookworm-slim

ARG MCP_CONFORMANCE_REVISION=c321dd32035556e6769d3724a8ee97d87c3faaac

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt
RUN git clone https://github.com/modelcontextprotocol/conformance.git mcp-conformance \
    && git -C mcp-conformance checkout --detach "${MCP_CONFORMANCE_REVISION}"

WORKDIR /opt/mcp-conformance/examples/servers/typescript
RUN npm ci

COPY docker/patch-mcp-conformance-hosts.mjs /usr/local/bin/patch-mcp-conformance-hosts.mjs
RUN node /usr/local/bin/patch-mcp-conformance-hosts.mjs everything-server.ts

WORKDIR /opt/mcp-conformance
RUN git diff --exit-code -- . ':(exclude)examples/servers/typescript/everything-server.ts' \
    && git diff --check -- examples/servers/typescript/everything-server.ts \
    && test "$(grep -Fxc "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });" examples/servers/typescript/everything-server.ts)" = 1

WORKDIR /opt/mcp-conformance/examples/servers/typescript
ENV PORT=3000
EXPOSE 3000
CMD ["npm", "start"]
