FROM node:26.13.1-trixie-slim@sha256:abe7e5a82b61a8ef67b315b85497c9e65173387ccf2f4a029248184b8e020be5

ARG MCP_CONFORMANCE_REVISION=f44482ba17df816d3176962a11cdf36aec9bda00

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt
RUN git clone https://github.com/modelcontextprotocol/conformance.git mcp-conformance \
    && git -C mcp-conformance fetch origin "${MCP_CONFORMANCE_REVISION}" \
    && git -C mcp-conformance checkout --detach "${MCP_CONFORMANCE_REVISION}"

WORKDIR /opt/mcp-conformance
COPY docker/mcp-conformance-fixture-deps.patch /tmp/mcp-conformance-fixture-deps.patch
RUN git apply --directory=examples/servers/typescript /tmp/mcp-conformance-fixture-deps.patch

COPY docker/mcp-conformance.patch /tmp/mcp-conformance.patch
RUN git apply --check /tmp/mcp-conformance.patch && git apply /tmp/mcp-conformance.patch

RUN git diff --exit-code -- . \
        ':(exclude)examples/servers/typescript/everything-server.ts' \
        ':(exclude)examples/servers/typescript/package.json' \
        ':(exclude)examples/servers/typescript/package-lock.json' \
    && git diff --check \
    && test "$(grep -Fxc "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });" examples/servers/typescript/everything-server.ts)" = 1

WORKDIR /opt/mcp-conformance/examples/servers/typescript
RUN npm ci

ENV PORT=3000
EXPOSE 3000
CMD ["npm", "start"]
