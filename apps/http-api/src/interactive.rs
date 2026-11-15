//! Caller-scoped controls for the demo providers. Only the mock holds the Restate callback token.
use super::{
    auth::{Access, Authenticated, Caller},
    error, unavailable, App, Shared,
};
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ekyc_domain::{
    product::{CaseState, CheckStatus},
    CheckKind,
};
use futures_util::future::join_all;
use reqwest::{redirect::Policy, Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, env, error::Error, time::Duration};

pub struct MockProviders {
    http: Client,
    bases: BTreeMap<CheckKind, Url>,
}

impl MockProviders {
    pub fn from_env() -> Result<Option<Self>, Box<dyn Error>> {
        match env::var("EKYC_DEMO_CALLBACKS").as_deref() {
            Ok("1") => (),
            Ok("0") | Err(env::VarError::NotPresent) => return Ok(None),
            _ => return Err("EKYC_DEMO_CALLBACKS must be 0 or 1".into()),
        }
        let mut bases = BTreeMap::new();
        for kind in CheckKind::ALL {
            let key = format!("{}_URL", kind.as_str().to_uppercase());
            if let Ok(value) = env::var(&key) {
                let url = Url::parse(&value)?;
                if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                    return Err(format!("invalid {key}").into());
                }
                bases.insert(kind, url);
            }
        }
        if bases.is_empty() {
            return Ok(None);
        }
        if bases.len() != CheckKind::ALL.len() {
            return Err("interactive demo needs all four configured provider URLs".into());
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(Policy::none())
            .build()?;
        Ok(Some(Self { http, bases }))
    }

    fn endpoint(&self, kind: CheckKind, id: &str, operation: &str) -> Url {
        let mut url = self.bases[&kind].clone();
        url.path_segments_mut()
            .expect("HTTP base URL")
            .pop_if_empty()
            .extend(["__mock__", "interactive", id]);
        url.query_pairs_mut().append_pair("operation_id", operation);
        url
    }

    async fn state(&self, id: &str, kind: CheckKind, operation: &str) -> ProviderState {
        match self
            .http
            .get(self.endpoint(kind, id, operation))
            .send()
            .await
        {
            Ok(res) if res.status() == StatusCode::NOT_FOUND => {
                ProviderState::new(Status::Registering)
            }
            Ok(res) if res.status().is_success() => res
                .json()
                .await
                .unwrap_or_else(|_| ProviderState::new(Status::Unavailable)),
            _ => ProviderState::new(Status::Unavailable),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Registering,
    AwaitingResponse,
    Sending,
    Sent,
    Closed,
    Unavailable,
}

#[derive(Serialize, Deserialize)]
struct ProviderState {
    status: Status,
    response: Option<String>,
    invocation_id: Option<String>,
}

impl ProviderState {
    fn new(status: Status) -> Self {
        Self {
            status,
            response: None,
            invocation_id: None,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Choice {
    response: String,
}

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/v2/cases/{id}/interactive", get(list))
        .route("/v2/cases/{id}/interactive/{kind}", post(respond))
}

#[derive(Deserialize)]
struct Case {
    state: CaseState,
    checks: Vec<Check>,
}
impl Case {
    fn terminal(&self) -> bool {
        matches!(self.state, CaseState::Approved | CaseState::Rejected)
    }
}

#[derive(Deserialize)]
struct Check {
    kind: CheckKind,
    status: CheckStatus,
    #[serde(default)]
    interactive: bool,
    operation_id: Option<String>,
}

async fn owned_case(app: &App, caller: &Caller, id: &str) -> Result<Case, Box<Response>> {
    if !matches!(caller.access, Access::Product { .. }) {
        return Err(error(StatusCode::FORBIDDEN, "role_forbidden").into());
    }
    if !caller.owns(id) {
        return Err(error(StatusCode::NOT_FOUND, "case_not_found").into());
    }
    let view = app
        .store
        .scoped_case(id, &caller.scope())
        .await
        .map_err(|_| unavailable(app))?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "case_not_found"))?;
    let case: Case = serde_json::from_value(view).map_err(|_| unavailable(app))?;
    if !case
        .checks
        .iter()
        .any(|c| c.interactive && c.operation_id.is_some())
    {
        return Err(error(StatusCode::CONFLICT, "not_an_interactive_case").into());
    }
    Ok(case)
}

async fn list(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
) -> Response {
    let case = match owned_case(&app, &caller, &id).await {
        Ok(c) => c,
        Err(e) => return *e,
    };
    let Some(mocks) = &app.interactive else {
        return unavailable(&app);
    };
    let ended = case.terminal();
    let checks=join_all(case.checks.iter().filter(|c|c.interactive).filter_map(|c| {
        let operation=c.operation_id.as_deref()?;
        let id=&id;
        Some(async move {
            let mut state=mocks.state(id,c.kind,operation).await;
            if (ended || c.status != CheckStatus::Pending) && !matches!(state.status,Status::Sent | Status::Sending) {
                state.status=Status::Closed;
            }
            json!({"kind":c.kind,"operation_id":operation,"status":state.status,"response":state.response,"invocation_id":state.invocation_id})
        })
    })).await;
    Json(json!({"checks":checks})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    operation_id: String,
}

async fn respond(
    State(app): State<Shared>,
    Path((id, kind)): Path<(String, CheckKind)>,
    Authenticated(caller): Authenticated,
    Query(target): Query<Target>,
    body: Result<Json<Choice>, JsonRejection>,
) -> Response {
    let case = match owned_case(&app, &caller, &id).await {
        Ok(case) => case,
        Err(err) => return *err,
    };
    let Some(check) = case
        .checks
        .iter()
        .find(|check| check.kind == kind && check.interactive)
    else {
        return error(StatusCode::NOT_FOUND, "check_not_interactive");
    };
    if check.operation_id.as_deref() != Some(&target.operation_id) {
        return error(StatusCode::CONFLICT, "stale_operation");
    }
    let Json(choice) = match body {
        Ok(body) => body,
        Err(err) => return error(err.status(), "invalid_request_body"),
    };
    let Some(mocks) = &app.interactive else {
        return unavailable(&app);
    };
    if case.terminal() || check.status != CheckStatus::Pending {
        let previous = mocks.state(&id, kind, &target.operation_id).await;
        if previous.response.as_ref() == Some(&choice.response) {
            return Json(previous).into_response();
        }
        if previous.status == Status::Unavailable {
            return unavailable(&app);
        }
        return error(StatusCode::CONFLICT, "check_already_finished");
    }
    let res = match mocks
        .http
        .post(mocks.endpoint(kind, &id, &target.operation_id))
        .json(&choice)
        .send()
        .await
    {
        Ok(res) => res,
        Err(_) => return unavailable(&app),
    };
    let status = res.status();
    match status {
        StatusCode::OK | StatusCode::ACCEPTED => match res.json::<ProviderState>().await {
            Ok(state) => (status, Json(state)).into_response(),
            Err(_) => unavailable(&app),
        },
        StatusCode::NOT_FOUND => error(StatusCode::CONFLICT, "check_not_yet_waiting"),
        StatusCode::CONFLICT => error(status, "response_already_chosen"),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            error(status, "invalid_provider_response")
        }
        _ => unavailable(&app),
    }
}
