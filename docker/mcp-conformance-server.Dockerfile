FROM node:22-bookworm-slim

ARG MCP_CONFORMANCE_REVISION=c321dd32035556e6769d3724a8ee97d87c3faaac

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt
RUN git clone https://github.com/modelcontextprotocol/conformance.git mcp-conformance \
    && git -C mcp-conformance checkout --detach "${MCP_CONFORMANCE_REVISION}"

WORKDIR /opt/mcp-conformance/examples/servers/typescript
RUN npm ci

COPY docker/mcp-conformance.patch /tmp/mcp-conformance.patch
RUN git apply --check /tmp/mcp-conformance.patch && git apply /tmp/mcp-conformance.patch

WORKDIR /opt/mcp-conformance
RUN git diff --exit-code -- . ':(exclude)examples/servers/typescript/everything-server.ts' \
    && git diff --check -- examples/servers/typescript/everything-server.ts \
    && test "$(grep -Fxc "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });" examples/servers/typescript/everything-server.ts)" = 1

WORKDIR /opt/mcp-conformance/examples/servers/typescript
ENV PORT=3000
EXPOSE 3000
CMD ["npm", "start"]
