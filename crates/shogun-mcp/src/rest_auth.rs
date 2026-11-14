use shogun_agents::entitlement::Entitlements;

use crate::memory_api::{plan_allows, scope_allows, AuthResult, TokenRegistry, TokenScope, Tool};

use super::rest_routing::{resolve, RouteMiss};
use super::{RestRequest, Routed};

/// The plan gate for a resolved endpoint: the Memory API itself for every tool endpoint, plus the
/// per-tool `agent_execution` requirement of the Gmail write-backs (FR-TG-31).
fn plan_allows_routed(routed: &Routed, ent: &Entitlements) -> bool {
    match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. } => plan_allows(*tool, ent),
        _ => ent.memory_api,
    }
}

/// The tool a resolved endpoint runs, for the scope gate. `actions.execute` and `actions.status`
/// have their own route shapes; `/v1/status` and `/v1/metrics` run no tool.
fn routed_tool(routed: &Routed) -> Option<Tool> {
    match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. } => Some(*tool),
        Routed::Action => Some(Tool::ActionsExecute),
        Routed::ApprovalStatus { .. } => Some(Tool::ActionsStatus),
        _ => None,
    }
}

/// The scope gate for a resolved endpoint (issue #400): the one [`scope_allows`] predicate the
/// MCP face applies too.
fn scope_allows_routed(routed: &Routed, scope: TokenScope) -> bool {
    // `map_or(true, ..)`: the workspace MSRV (1.80) predates `is_none_or`.
    routed_tool(routed).is_none_or(|tool| scope_allows(scope, tool))
}

/// The token of an `Authorization: Bearer <token>` header. The scheme name is case-insensitive
/// (RFC 7235 §2.1), so `bearer <token>` authenticates too (issue #779).
pub fn bearer(authorization: Option<&str>) -> Option<String> {
    let (scheme, token) = authorization?.trim_start().split_once(|c: char| c.is_ascii_whitespace())?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

/// Route a request: resolve the endpoint, then apply auth, then the plan gate. `/v1/status` and
/// `/v1/metrics` are the two unauthenticated endpoints (localhost-bound health/discovery, no
/// capture content); every tool endpoint requires a valid token (FR-API-03) AND a plan that
/// includes the Memory API (issue #97: Pro/Trial only) AND a token scope that covers the endpoint
/// (issue #400: a `read` token reaches read endpoints only). Both gates live here in the shared
/// routing layer so the REST server and CLI face cannot drift, and every special-cased write
/// branch in the server runs after this refusal, never before it.
pub fn route(req: &RestRequest, tokens: &TokenRegistry, ent: &Entitlements) -> Routed {
    match resolve(req.method, &req.path) {
        Err(RouteMiss::NotFound) => Routed::NotFound,
        Err(RouteMiss::MethodNotAllowed) => Routed::MethodNotAllowed,
        Ok(Routed::Status) => Routed::Status, // unauthenticated discovery
        Ok(Routed::Metrics) => Routed::Metrics, // unauthenticated health (NFR-SLO-00)
        Ok(resolved) => authorize(resolved, tokens.authenticate(req.token.as_deref()), ent),
    }
}

/// The gates a resolved tool endpoint passes, in order: who are you (401), what may this device
/// do (plan, 403 `plan_required`), what may this token do (scope, 403 `scope_insufficient`).
pub(super) fn authorize(resolved: Routed, auth: AuthResult, ent: &Entitlements) -> Routed {
    match auth {
        AuthResult::Granted(_) if !plan_allows_routed(&resolved, ent) => Routed::PlanLocked,
        AuthResult::Granted(scope) if !scope_allows_routed(&resolved, scope) => {
            Routed::ScopeInsufficient
        }
        AuthResult::Granted(_) => resolved,
        AuthResult::DeniedNoToken | AuthResult::DeniedInvalidToken => Routed::Unauthorized,
    }
}
