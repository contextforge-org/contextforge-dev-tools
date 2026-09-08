//! Discover fixture catalogs and publish the dataplane's MessagePack contract.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::mcp::gateway::{MCP_PROTOCOL_VERSION, MCP_SESSION_ID};
use crate::mcp::protocol::{self, is_stateless_protocol, jsonrpc_with_id, with_request_metadata};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Tool {
    name: String,
    #[serde(rename = "inputSchema")]
    input_schema: Map<String, Value>,
}

pub(super) struct Catalog {
    tools: Vec<Tool>,
    resources: Vec<String>,
    resource_templates: Vec<String>,
    prompts: Vec<String>,
}

impl Catalog {
    pub(super) fn for_client(tools: Vec<String>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|name| Tool {
                    name,
                    input_schema: Map::new(),
                })
                .collect(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
        }
    }

    pub(super) fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    pub(super) fn config(
        &self,
        server_id: &str,
        backend_url: &str,
        protocol_version: &str,
    ) -> Value {
        let backend_name = "conformance-backend";
        let routes = |names: Vec<&str>| {
            names
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        json!({"backend_name": backend_name, "upstream_name": name}),
                    )
                })
                .collect::<Map<_, _>>()
        };
        let schemas: BTreeMap<_, _> = self
            .tools
            .iter()
            .map(|tool| (&tool.name, &tool.input_schema))
            .collect();
        json!({"virtual_hosts": {server_id: {
            "backends": {backend_name: {
                "name": backend_name, "url": backend_url, "mcp_protocol_version": protocol_version,
                "passthrough_headers": [], "add_headers": {}, "remove_headers": [],
                "completion": {}, "tool_schemas": schemas,
            }},
            "tools": routes(self.tool_names()),
            "resources": routes(self.resources.iter().map(String::as_str).collect()),
            "resource_templates": routes(self.resource_templates.iter().map(String::as_str).collect()),
            "prompts": routes(self.prompts.iter().map(String::as_str).collect()),
        }}})
    }
}

pub(super) async fn publish(redis_url: &str, subject: &str, body: &Value) -> Result<()> {
    let client = redis::Client::open(redis_url).context("invalid config Redis URL")?;
    let options = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(Some(TIMEOUT))
        .set_response_timeout(Some(TIMEOUT));
    let mut connection = client
        .get_multiplexed_async_connection_with_config(&options)
        .await
        .context("failed to connect to config Redis")?;
    // User::new(subject) uses this compact key; named maps preserve empty objects.
    redis::cmd("SET")
        .arg(rmp_serde::to_vec(&("UserConfig", subject))?)
        .arg(rmp_serde::to_vec_named(body)?)
        .query_async::<()>(&mut connection)
        .await
        .context("failed to publish dataplane config")
}

pub(super) async fn fixture_catalog(url: Url, version: &str) -> Result<Catalog> {
    let mut client = FixtureClient {
        http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
        url,
        version: version.to_owned(),
        session: None,
        request_id: 0,
    };
    let result = client.catalog().await;
    if let Some(session) = client.session.take() {
        let cleanup = client
            .http
            .delete(client.url.clone())
            .header(MCP_SESSION_ID, session)
            .header(MCP_PROTOCOL_VERSION, version)
            .send()
            .await;
        // Preserve the discovery error while still attempting session cleanup.
        if result.is_ok() {
            cleanup.context("failed to close fixture session")?;
        }
    }
    result
}

struct FixtureClient {
    http: reqwest::Client,
    url: Url,
    version: String,
    session: Option<String>,
    request_id: u64,
}

impl FixtureClient {
    async fn rpc(&mut self, method: &str, params: Value, notification: bool) -> Result<Value> {
        self.request_id += 1;
        let modern = is_stateless_protocol(&self.version);
        let params = if modern {
            with_request_metadata(Some(params), &self.version)
        } else {
            params
        };
        let mut body = jsonrpc_with_id(method, Some(params), json!(self.request_id));
        if notification {
            body.as_object_mut()
                .context("JSON-RPC request must be an object")?
                .remove("id");
        }
        let mut request = self
            .http
            .post(self.url.clone())
            .header(ACCEPT, protocol::ACCEPT)
            .json(&body);
        if method != "initialize" {
            request = request.header(MCP_PROTOCOL_VERSION, &self.version);
        }
        if modern {
            request = request.header("mcp-method", method);
        }
        if let Some(session) = &self.session {
            request = request.header(MCP_SESSION_ID, session);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("fixture {method} failed"))?;
        ensure!(
            response.status().is_success(),
            "fixture {method} failed: HTTP {}",
            response.status()
        );
        if let Some(session) = response.headers().get(MCP_SESSION_ID) {
            self.session = Some(session.to_str()?.to_owned());
        }
        if notification {
            return Ok(Value::Null);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let message = protocol::parse_mcp_body(&response.text().await?, &content_type)?
            .with_context(|| format!("fixture {method} returned no message"))?;
        ensure!(
            message["id"] == self.request_id
                && message["error"].is_null()
                && message["result"].is_object(),
            "fixture {method} did not return a successful result"
        );
        Ok(message["result"].clone())
    }

    async fn list(&mut self, method: &str, field: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut cursors = BTreeSet::new();
        let mut params = json!({});
        loop {
            let mut page = self.rpc(method, params, false).await?;
            items.append(
                page[field]
                    .as_array_mut()
                    .with_context(|| format!("fixture {method} has no {field} array"))?,
            );
            let cursor = page["nextCursor"].take();
            if cursor.is_null() {
                return Ok(items);
            }
            ensure!(
                cursor.is_string()
                    && cursors.insert(cursor.as_str().unwrap_or_default().to_owned()),
                "fixture {method} returned an invalid or repeated cursor"
            );
            params = json!({"cursor": cursor});
        }
    }

    async fn names(&mut self, method: &str, field: &str, name: &str) -> Result<Vec<String>> {
        self.list(method, field)
            .await?
            .into_iter()
            .map(|item| {
                item[name]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .with_context(|| format!("fixture {method} has an invalid {name}"))
            })
            .collect()
    }

    async fn catalog(&mut self) -> Result<Catalog> {
        if is_stateless_protocol(&self.version) {
            self.rpc("server/discover", json!({}), false).await?;
        } else {
            let initialize = protocol::initialize_with_id_and_version(json!(0), &self.version);
            self.rpc("initialize", initialize["params"].clone(), false)
                .await?;
            self.rpc("notifications/initialized", json!({}), true)
                .await?;
        }
        let tools: Vec<Tool> = self
            .list("tools/list", "tools")
            .await?
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .context("fixture tools must include names and input schemas")?;
        ensure!(
            !tools.is_empty() && tools.iter().all(|tool| !tool.name.is_empty()),
            "fixture tools must include names and input schemas"
        );
        Ok(Catalog {
            tools,
            resources: self.names("resources/list", "resources", "uri").await?,
            resource_templates: self
                .names(
                    "resources/templates/list",
                    "resourceTemplates",
                    "uriTemplate",
                )
                .await?,
            prompts: self.names("prompts/list", "prompts", "name").await?,
        })
    }
}
