//! MCP token authority, the tool executor, and tool credential management.

use std::sync::Arc;

use mcp::{TokenAuthority, ToolDescriptor, ToolExecutor, ToolOutcome, WorkspaceScope};

use super::AppState;
use super::error::{RegistryError, Result};
use super::util::lock_unpoisoned;
use crate::integrations::{BindingState, CandidateTool, Credential, Delivery, ResolvedIntegration};
use crate::types::{ToolConfig, ToolList};

impl AppState {
    /// Resolves the enabled tool integrations for `id`: tools requested in
    /// `gavmo.yaml` that have a correctly bound secret configured.
    pub(super) async fn resolve_integrations_for(&self, id: &str) -> Vec<ResolvedIntegration> {
        let candidates = self.candidates_for(id).await;
        // Values, not just presence: a credential bound to a different
        // destination than the current `base_url` isn't advertised at all
        // (defense in depth over the forwarder's own check).
        let values = match self
            .secrets_fetch_many(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await
        {
            Ok(values) => values,
            Err(e) => {
                // The VM still boots, just without tools; the panel shows why.
                tracing::warn!(workspace = %id, error = %e, "no tools enabled: secret store unavailable");
                return Vec::new();
            }
        };
        candidates
            .into_iter()
            .filter(|candidate| {
                matches!(
                    crate::integrations::classify_binding(
                        &candidate.def,
                        values.get(&candidate.secret_key).map(String::as_str),
                    ),
                    BindingState::Configured
                )
            })
            .map(CandidateTool::into_resolved)
            .collect()
    }

    /// The integrations of `id`'s `Running` slot (empty if none/stopped).
    pub(super) fn integrations_for(&self, id: &str) -> Arc<[ResolvedIntegration]> {
        lock_unpoisoned(&self.inner.workspace_integrations)
            .get(id)
            .cloned()
            .unwrap_or_else(|| Arc::new([]))
    }

    /// Every tool this workspace's `gavmo.yaml` requests, read live (no secret
    /// access) so the control panel reflects config changes immediately.
    pub(super) async fn candidates_for(&self, id: &str) -> Vec<CandidateTool> {
        self.candidates_and_config_error(id).await.0
    }

    /// [`Self::candidates_for`] plus why `gavmo.yaml` was ignored, if it was.
    async fn candidates_and_config_error(&self, id: &str) -> (Vec<CandidateTool>, Option<String>) {
        // Reads `<id>/gavmo.yaml`.
        if !self.is_known(id).await {
            return (Vec::new(), None);
        }
        let (config, error) = crate::integrations::read_config(id).await;
        (crate::integrations::resolve_candidates(id, &config), error)
    }

    /// Finds a single requested tool by id (structural; no secret-store access).
    pub(super) async fn candidate(&self, id: &str, tool_id: &str) -> Result<CandidateTool> {
        self.require_known(id).await?;
        self.candidates_for(id)
            .await
            .into_iter()
            .find(|candidate| candidate.id == tool_id)
            .ok_or_else(|| RegistryError::InvalidInput(format!("unknown tool: {tool_id}")))
    }

    /// The control panel's view of a workspace's requested tools and whether
    /// each has its credential configured. Fails with
    /// [`RegistryError::SecretStore`] if the store can't be read, rather than
    /// reporting every tool as unconfigured.
    pub async fn list_tool_configs(&self, id: &str) -> Result<ToolList> {
        self.require_known(id).await?;
        let (candidates, config_error) = self.candidates_and_config_error(id).await;
        // Values are needed to detect a changed destination, not just presence.
        let values = self
            .secrets_fetch_many(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await?;

        let tools = candidates
            .into_iter()
            .map(|tool| {
                let state = crate::integrations::classify_binding(
                    &tool.def,
                    values.get(&tool.secret_key).map(String::as_str),
                );
                let delivery = Delivery::of(&tool.def)
                    .map(|d| d.describe())
                    .unwrap_or_default();
                let (configured, url_changed, bound_url, approved_delivery) = match state {
                    BindingState::NotConfigured => (false, false, None, None),
                    BindingState::Configured => (true, false, None, None),
                    BindingState::DestinationChanged { bound_url } => {
                        (false, true, bound_url, None)
                    }
                    BindingState::DeliveryChanged { approved } => (
                        false,
                        false,
                        None,
                        Some(approved.map_or_else(
                            || "an unrecorded method (saved by an older GAVMO)".to_owned(),
                            |d| d.describe(),
                        )),
                    ),
                };
                ToolConfig {
                    configured,
                    url_changed,
                    bound_url,
                    delivery,
                    approved_delivery,
                    id: tool.id,
                    description: tool
                        .def
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("HTTP access to {}", tool.def.base_url)),
                    base_url: tool.def.base_url,
                    auth: tool.def.auth.as_str().to_owned(),
                    secret_name: tool.def.secret,
                    instructions: tool.def.instructions,
                }
            })
            .collect();
        Ok(ToolList {
            tools,
            config_error,
        })
    }

    /// Stores (or replaces) a tool's credential in the secret store. For Basic
    /// auth both `username` and `token` are required; for Bearer/Header only
    /// `token`. Takes effect on the VM's next open, since a running VM's
    /// tools are fixed at boot.
    pub async fn set_tool_secret(
        &self,
        id: &str,
        tool_id: &str,
        username: Option<String>,
        token: String,
    ) -> Result<()> {
        let tool = self.candidate(id, tool_id).await?;

        let delivery = Delivery::of(&tool.def).map_err(RegistryError::InvalidInput)?;
        let credential = match delivery {
            Delivery::Basic => {
                let username = username.filter(|u| !u.is_empty()).ok_or_else(|| {
                    RegistryError::InvalidInput("basic auth requires a username/email".to_owned())
                })?;
                Credential::Basic {
                    username,
                    password: token,
                }
            }
            Delivery::Bearer | Delivery::Header { .. } => Credential::Token { token },
        };

        // Bind the credential to the destination and delivery the user
        // approved, so a later `base_url` or `auth`/`header_name` change in the
        // untrusted `gavmo.yaml` fails closed instead of silently redirecting
        // or re-routing the token.
        let canonical = toolspec::canonical_base_url(&tool.def.base_url).map_err(|e| {
            RegistryError::InvalidInput(format!("tool base_url could not be canonicalized: {e}"))
        })?;
        let bound = crate::integrations::wrap_stored_secret(&canonical, delivery, credential)
            .map_err(RegistryError::InvalidInput)?;

        self.secret_set(tool.secret_key, bound).await
    }

    /// Removes a tool's stored credential (disabling it on the next open).
    pub async fn clear_tool_secret(&self, id: &str, tool_id: &str) -> Result<()> {
        let tool = self.candidate(id, tool_id).await?;
        self.secret_delete(tool.secret_key).await
    }

    /// Live credential test: forwards a `GET` to the tool's `test_path` (or
    /// `/`) with the stored credential; `Ok` on 2xx. Without an auth-reflecting
    /// `test_path` this is only a reachability probe.
    pub async fn test_tool(&self, id: &str, tool_id: &str) -> Result<String> {
        let tool = self.candidate(id, tool_id).await?;
        let secret_value = self.secret_get(tool.secret_key.clone()).await?;

        let test_path = tool
            .def
            .test_path
            .clone()
            .unwrap_or_else(|| "/".to_string());
        let mut args = serde_json::Map::new();
        args.insert("method".to_string(), serde_json::json!("GET"));
        args.insert("path".to_string(), serde_json::json!(test_path));

        match crate::integrations::forward(
            &tool.into_resolved(),
            secret_value,
            &args,
            &self.inner.http_client,
        )
        .await
        {
            ToolOutcome::Success(text) => Ok(text),
            ToolOutcome::Error(text) => Err(RegistryError::Failed(text)),
        }
    }
}

/// Lets the `mcp` gateway resolve a bearer token to a workspace without
/// depending on `gavmo`.
impl TokenAuthority for AppState {
    fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
        let tokens = lock_unpoisoned(&self.inner.mcp_tokens);
        let workspace_id = tokens.get(token)?.clone();
        Some(WorkspaceScope { workspace_id })
    }
}

/// The daemon's MCP tools: the `gavmo_ping` probe plus one host-locked HTTP
/// forwarder per enabled integration of the authorized workspace.
impl ToolExecutor for AppState {
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor> {
        let mut tools = vec![ping_descriptor()];
        for integration in self.integrations_for(&scope.workspace_id).iter() {
            tools.push(crate::integrations::integration_tool_descriptor(
                integration,
            ));
        }
        tools
    }

    fn has_tool(&self, scope: &WorkspaceScope, name: &str) -> bool {
        name == "gavmo_ping"
            || self
                .integrations_for(&scope.workspace_id)
                .iter()
                .any(|integration| integration.id == name)
    }

    async fn call_tool(
        &self,
        scope: &WorkspaceScope,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> ToolOutcome {
        if name == "gavmo_ping" {
            return ToolOutcome::Success(format!("pong from workspace {}", scope.workspace_id));
        }

        // A miss is only possible if tools were cleared between list and call.
        match self
            .integrations_for(&scope.workspace_id)
            .iter()
            .find(|integration| integration.id == name)
            .cloned()
        {
            Some(integration) => {
                // Fetched fresh per call; credentials are never cached.
                let secret_value = match self.secret_get(integration.secret_key.clone()).await {
                    Ok(value) => value,
                    Err(e) => return ToolOutcome::Error(e.to_string()),
                };
                crate::integrations::forward(
                    &integration,
                    secret_value,
                    &arguments,
                    &self.inner.http_client,
                )
                .await
            }
            None => ToolOutcome::Error(format!("unknown tool: {name}")),
        }
    }
}

/// The `gavmo_ping` tool descriptor: no arguments, just proves a round trip
/// (discovery + call + auth) through the whole pipeline.
pub(super) fn ping_descriptor() -> ToolDescriptor {
    let mut input_schema = serde_json::Map::new();
    input_schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".into()),
    );
    input_schema.insert("properties".to_string(), serde_json::json!({}));
    input_schema.insert(
        "additionalProperties".to_string(),
        serde_json::Value::Bool(false),
    );
    ToolDescriptor {
        name: "gavmo_ping".to_string(),
        description: "Checks connectivity to the GAVMO host-side tool gateway and reports \
             which workspace this VM is authorized for. Takes no arguments."
            .to_string(),
        input_schema,
    }
}
