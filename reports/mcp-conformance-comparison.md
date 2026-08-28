# MCP Conformance Comparison

- Official oracle: `@modelcontextprotocol/conformance@0.2.0-alpha.11`
- Client specification: `2026-07-28`
- Upstream server era: `dual`
- Suite: `all`
- Fixture source: `https://github.com/modelcontextprotocol/conformance` at `c321dd32035556e6769d3724a8ee97d87c3faaac`

## Target outcomes

| Target | Compliant scenarios | Failed scenarios | Failed checks | Fixture failures | Not applicable | Ambiguous | Missing |
|---|---:|---:|---:|---:|---:|---:|---:|
| Fixture direct | 31 | 9 | 17 | 0 | 0 | 0 | 0 |
| Built-in data-plane route | 0 | 40 | 110 | 0 | 0 | 0 | 0 |
| External data-plane route | 7 | 33 | 50 | 0 | 0 | 0 | 0 |

## Comparison summary

| Classification | Scenarios |
|---|---:|
| all compliant | 0 |
| fixture-only failure | 0 |
| built-in data-plane only failure | 7 |
| external data-plane only failure | 0 |
| fixture + built-in data-plane failure | 0 |
| fixture + external data-plane failure | 0 |
| both gateways only failure | 24 |
| shared failure | 9 |
| fixture failure | 0 |
| not applicable | 0 |
| ambiguous | 0 |

## Scenarios

| Scenario | Fixture direct | Built-in data-plane route | External data-plane route | Classification | Specification references |
|---|---|---|---|---|---|
| caching | compliant | failure | failure | both gateways only failure | [MCP-Caching](https://modelcontextprotocol.io/specification/draft/server/utilities/caching)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2549](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2549) |
| completion-complete | compliant | failure | failure | both gateways only failure | [MCP-Completion](https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/completion)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| dns-rebinding-protection | compliant | failure | compliant | built-in data-plane only failure | [MCP-DNS-Rebinding-Protection](https://modelcontextprotocol.io/specification/2025-11-25/basic/security_best_practices#local-mcp-server-compromise)<br>[MCP-Transport-Security](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#security-warning) |
| http-custom-header-server-validation | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2243-Custom-Headers](https://modelcontextprotocol.io/specification/draft/basic/transports#server-behavior-for-custom-headers) |
| http-header-validation | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[RFC-9110-5.5-Field-Values](https://www.rfc-editor.org/rfc/rfc9110#section-5.5)<br>[SEP-2243-Case-Sensitivity](https://modelcontextprotocol.io/specification/draft/basic/transports#case-sensitivity)<br>[SEP-2243-Server-Validation](https://modelcontextprotocol.io/specification/draft/basic/transports#server-validation) |
| input-required-result-basic-elicitation | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-basic-list-roots | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-basic-sampling | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-capability-check | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-ignore-extra-params | compliant | failure | compliant | built-in data-plane only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-missing-input-response | compliant | failure | compliant | built-in data-plane only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-multi-round | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-multiple-input-requests | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-non-tool-request | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-request-state | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-result-type | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-tampered-state | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-unsupported-methods | compliant | failure | compliant | built-in data-plane only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| input-required-result-validate-input | compliant | failure | compliant | built-in data-plane only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2322](https://modelcontextprotocol.io/specification/draft/basic/utilities/mrtr) |
| json-schema-2020-12 | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-1613](https://github.com/modelcontextprotocol/specification/pull/655)<br>[SEP-2106](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2106) |
| prompts-get-embedded-resource | compliant | failure | failure | both gateways only failure | [MCP-Prompts-Embedded-Resources](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts#embedded-resources)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| prompts-get-simple | compliant | failure | failure | both gateways only failure | [MCP-Prompts-Get](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts#getting-prompts)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| prompts-get-with-args | compliant | failure | failure | both gateways only failure | [MCP-Prompts-Get](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts#getting-prompts)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| prompts-get-with-image | compliant | failure | failure | both gateways only failure | [MCP-Prompts-Image](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts#image-content)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| prompts-list | compliant | failure | failure | both gateways only failure | [MCP-Prompts-List](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts#listing-prompts)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| resources-list | compliant | failure | failure | both gateways only failure | [MCP-Resources-List](https://modelcontextprotocol.io/specification/2025-06-18/server/resources#listing-resources)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| resources-read-binary | compliant | failure | failure | both gateways only failure | [MCP-Resources-Read](https://modelcontextprotocol.io/specification/2025-06-18/server/resources#reading-resources)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| resources-read-text | compliant | failure | failure | both gateways only failure | [MCP-Resources-Read](https://modelcontextprotocol.io/specification/2025-06-18/server/resources#reading-resources)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| resources-templates-read | compliant | failure | failure | both gateways only failure | [MCP-Resources-Templates](https://modelcontextprotocol.io/specification/2025-06-18/server/resources#resource-templates)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| sep-2164-resource-not-found | compliant | failure | compliant | built-in data-plane only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[SEP-2164](https://modelcontextprotocol.io/specification/draft/server/resources#error-handling) |
| server-sse-multiple-streams | compliant | failure | compliant | built-in data-plane only failure | [SEP-1699](https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1699) |
| server-stateless | compliant | failure | failure | both gateways only failure | [SEP-2575](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2575) |
| tools-call-audio | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-Call](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#calling-tools) |
| tools-call-embedded-resource | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-Call](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#calling-tools) |
| tools-call-error | failure | failure | failure | shared failure | [MCP-Error-Handling](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| tools-call-image | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-Call](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#calling-tools) |
| tools-call-mixed-content | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-Call](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#calling-tools) |
| tools-call-simple-text | failure | failure | failure | shared failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-Call](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#calling-tools) |
| tools-call-with-progress | failure | failure | failure | shared failure | [MCP-Progress](https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/progress)<br>[MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json) |
| tools-list | compliant | failure | failure | both gateways only failure | [MCP-Schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/draft/schema.json)<br>[MCP-Tools-List](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#listing-tools)<br>[MCP-Tools-List](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#tool-names)<br>[SEP-986](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#tool-names) |
