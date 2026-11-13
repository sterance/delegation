//! Models on clients, from the dashboard's side: the catalog of what can be
//! installed, each client's inventory, and the commands that change it.
//!
//! Installing, deleting, configuring models and choosing the runtime
//! variant need **Manage** on the client; seeing what it
//! has needs **View** (so future borrowers will see models they can Use but
//! not manage them). The browser names catalog entries by id only.
//!
//! The model suggestion engine plugs in here without changing the client:
//! it fills the catalog (a table, later) and sends the same commands.

pub mod catalog;
pub mod commands;

use crate::access::{self, ClientAction};
use crate::auth::CurrentUser;
use crate::clients::require;
use crate::http::{ApiError, ApiJson, ApiResult};
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use catalog::CatalogEntry;
use commands::{CancelError, CommandKind, NewCommand, TrackedCommand, TrackedState};
use protocol::{
    valid_model_id, Backend, LlamaCppVersion, LocalModel, ModelCommandKind, ModelSettings, ModelSpec, RuntimeInfo,
    Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// `GET /api/models/catalog`: what can be installed.
pub async fn list_catalog(State(state): State<AppState>, CurrentUser(_user): CurrentUser) -> Json<Vec<CatalogEntry>> {
    Json(state.catalog.list())
}

#[derive(Serialize)]
pub struct ClientModels {
    client_id: String,
    connected: bool,
    can_manage: bool,
    /// Empty until the client has reported, and while it's disconnected.
    models: Vec<LocalModel>,
    storage: Option<Storage>,
    runtimes: Vec<RuntimeInfo>,
    /// Recent and running commands, newest first.
    commands: Vec<TrackedCommand>,
}

/// The models a client offers, minus `echo` when the server has it off.
pub fn shown_models(state: &AppState, models: &[LocalModel]) -> Vec<LocalModel> {
    let echo_on = state.settings.client.inference.echo_model_enabled;
    models
        .iter()
        .filter(|m| echo_on || m.backend != Backend::Echo)
        .cloned()
        .collect()
}

/// `GET /api/clients/:id/models` (View).
pub async fn client_models(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(client_id): Path<String>,
) -> ApiResult<Json<ClientModels>> {
    let row = require(&state, &user, &client_id, ClientAction::View).await?;
    let can_manage = access::client_access(&user, &row).is_some_and(|a| access::allows(a, ClientAction::Manage));
    let connected = state.live.sender(&client_id).is_some();
    let inventory = state.live.inventory(&client_id);
    Ok(Json(ClientModels {
        connected,
        can_manage,
        models: inventory
            .as_ref()
            .map(|i| shown_models(&state, &i.models))
            .unwrap_or_default(),
        storage: inventory.as_ref().map(|i| i.storage),
        runtimes: inventory.as_ref().map(|i| i.runtimes.clone()).unwrap_or_default(),
        commands: state.model_commands.for_client(&client_id),
        client_id,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallBody {
    catalog_id: String,
}

/// 202 with the command, or 409 with it if it failed at once (offline).
fn sent(command: TrackedCommand) -> Response {
    if command.state == TrackedState::Failed {
        let error = command.error.clone().unwrap_or_else(|| "The command failed.".into());
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": error, "command": command })),
        )
            .into_response();
    }
    (StatusCode::ACCEPTED, Json(command)).into_response()
}

/// `POST /api/clients/:id/models` (Manage): `{ "catalog_id": "…" }` sends a pull.
pub async fn install(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(client_id): Path<String>,
    ApiJson(body): ApiJson<InstallBody>,
) -> ApiResult<Response> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    let spec = state
        .catalog
        .get(&body.catalog_id)
        .ok_or_else(|| ApiError::bad_request(format!("There's no catalog entry {:?}.", body.catalog_id)))?
        .clone();
    let runtimes = state
        .live
        .inventory(&client_id)
        .map(|i| i.runtimes.clone())
        .unwrap_or_default();
    if let Some(why) = runtime_too_old(&spec, &runtimes) {
        return Err(ApiError::conflict(why));
    }
    let command = state.model_commands.send(
        &state.live,
        NewCommand {
            client_id: &client_id,
            kind: CommandKind::Pull,
            model_id: &spec.id,
            model_name: &spec.display_name,
            requested_by: &user.username,
            command: ModelCommandKind::Pull { model: spec.clone() },
        },
    );
    Ok(sent(command))
}

/// `DELETE /api/clients/:id/models/:model_id` (Manage).
pub async fn delete_model(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((client_id, model_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    if !valid_model_id(&model_id) {
        return Err(ApiError::bad_request("That isn't a valid model id."));
    }
    let known = state
        .live
        .inventory(&client_id)
        .and_then(|i| i.models.iter().find(|m| m.id == model_id).cloned());
    if known.as_ref().is_some_and(|m| m.backend == Backend::Echo) {
        return Err(ApiError::bad_request(
            "The echo model is built in and can't be deleted.",
        ));
    }
    let name = known.map_or_else(|| model_id.clone(), |m| m.display_name);
    let command = state.model_commands.send(
        &state.live,
        NewCommand {
            client_id: &client_id,
            kind: CommandKind::Delete,
            model_id: &model_id,
            model_name: &name,
            requested_by: &user.username,
            command: ModelCommandKind::Delete {
                model_id: model_id.clone(),
            },
        },
    );
    Ok(sent(command))
}

/// `POST /api/clients/:id/model-commands/:command_id/cancel` (Manage): stops a pull.
pub async fn cancel_command(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((client_id, command_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    match state.model_commands.cancel(&state.live, &client_id, &command_id) {
        Ok(command) => {
            crate::log(format!(
                "[models] {} cancelled {command_id} on {client_id}",
                user.username
            ));
            Ok((StatusCode::ACCEPTED, Json(command)).into_response())
        }
        Err(CancelError::NotFound) => Err(ApiError::not_found()),
        Err(CancelError::NotCancellable) => Err(ApiError::conflict("Only downloads in progress can be cancelled.")),
        Err(CancelError::Offline) => Err(ApiError::conflict("The client is offline.")),
    }
}

/// Why `spec` can't run on a client with these runtimes, if it can't: its
/// `min_runtime_version` is newer than the client's llama.cpp. A client
/// that hasn't reported yet gets the benefit of the doubt (it refuses the
/// pull itself if it's too old).
pub fn runtime_too_old(spec: &ModelSpec, runtimes: &[RuntimeInfo]) -> Option<String> {
    let need = LlamaCppVersion::parse(spec.min_runtime_version.as_deref()?)?;
    let have = runtimes
        .iter()
        .filter(|r| r.backend == Backend::LlamaCpp)
        .filter_map(|r| LlamaCppVersion::parse(&r.version))
        .max()?;
    (have < need).then(|| {
        format!(
            "{} needs llama.cpp {need} or newer; this client has {have}. Update the client first.",
            spec.display_name
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBody {
    variant: String,
}

/// A variant name as the protocol uses them ("auto", "cpu", "cuda-12", …).
fn plausible_variant(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `POST /api/clients/:id/runtime` (Manage): `{ "variant": "auto" | "cpu" | … }`
/// sends `select_runtime`. The client checks it's a variant it has pinned.
pub async fn select_runtime(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(client_id): Path<String>,
    ApiJson(body): ApiJson<RuntimeBody>,
) -> ApiResult<Response> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    if !plausible_variant(&body.variant) {
        return Err(ApiError::bad_request("That isn't a runtime variant."));
    }
    let name = if body.variant == "auto" {
        "llama.cpp: automatic".to_string()
    } else {
        format!("llama.cpp: {}", body.variant)
    };
    let command = state.model_commands.send(
        &state.live,
        NewCommand {
            client_id: &client_id,
            kind: CommandKind::SelectRuntime,
            model_id: "",
            model_name: &name,
            requested_by: &user.username,
            command: ModelCommandKind::SelectRuntime { variant: body.variant },
        },
    );
    Ok(sent(command))
}

/// The settings body: like `ModelSettings`, but nothing else is accepted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    #[serde(default)]
    gpu_layers: protocol::GpuLayers,
    #[serde(default)]
    context_length: Option<u32>,
}

/// `PUT /api/clients/:id/models/:model_id/settings` (Manage): the model's
/// settings, `{ "gpu_layers": "auto" | "off" | "all" | n, "context_length": n | null }`,
/// sent as `configure`. The client stores them and applies them on the next load.
pub async fn configure_model(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((client_id, model_id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SettingsBody>,
) -> ApiResult<Response> {
    let settings = ModelSettings {
        gpu_layers: body.gpu_layers,
        context_length: body.context_length,
    };
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    if !valid_model_id(&model_id) {
        return Err(ApiError::bad_request("That isn't a valid model id."));
    }
    if let Some(why) = settings.problem() {
        return Err(ApiError::bad_request(why));
    }
    let known = state
        .live
        .inventory(&client_id)
        .and_then(|i| i.models.iter().find(|m| m.id == model_id).cloned());
    if known.as_ref().is_some_and(|m| m.backend == Backend::Echo) {
        return Err(ApiError::bad_request("The echo model has no settings."));
    }
    let name = known.map_or_else(|| model_id.clone(), |m| m.display_name);
    let command = state.model_commands.send(
        &state.live,
        NewCommand {
            client_id: &client_id,
            kind: CommandKind::Configure,
            model_id: &model_id,
            model_name: &name,
            requested_by: &user.username,
            command: ModelCommandKind::Configure {
                model_id: model_id.clone(),
                settings,
            },
        },
    );
    Ok(sent(command))
}
