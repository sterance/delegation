//! `GET /api/jobs` and `GET /api/jobs/{id}`: the queue and its history, for
//! the dashboard's Jobs page. Metadata only: which job, where it ran, how
//! each attempt ended. Never a message's text.

use super::store::{self, JobFilter, JobView};
use crate::auth::CurrentUser;
use crate::http::{ApiError, ApiResult};
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Deserialize)]
pub struct JobsQuery {
    /// `active` (queued, assigned or running; oldest in the queue first) or
    /// `recent` (finished; newest first). Left out: everything, newest first.
    status: Option<String>,
    limit: Option<usize>,
    /// The admin only: everyone's jobs, not just their own.
    all: Option<String>,
}

#[derive(Serialize)]
pub struct JobsResponse {
    jobs: Vec<JobView>,
}

fn wants_all(all: Option<&str>) -> bool {
    matches!(all, Some("1" | "true"))
}

/// `GET /api/jobs?status=active|recent&limit=&all=1`: the user's jobs, each
/// with its attempts.
pub async fn list_jobs(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<JobsQuery>,
) -> ApiResult<Json<JobsResponse>> {
    let filter = match query.status.as_deref() {
        None | Some("") => JobFilter::All,
        Some("active") => JobFilter::Active,
        Some("recent") => JobFilter::Recent,
        Some(_) => return Err(ApiError::bad_request("status is \"active\" or \"recent\".")),
    };
    let everyone = wants_all(query.all.as_deref());
    if everyone && !user.is_admin() {
        return Err(ApiError::forbidden("Only the admin can list everyone's jobs."));
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let owner = (!everyone).then_some(user.id);
    let jobs = state
        .db
        .call(move |c| store::list_jobs(c, owner.as_deref(), filter, limit))
        .await?;
    Ok(Json(JobsResponse { jobs }))
}

/// `GET /api/jobs/{id}`: one job, same shape. 404 unless it's the user's
/// (the admin can read anyone's).
pub async fn get_job(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(job_id): Path<String>,
) -> ApiResult<Json<JobView>> {
    let owner = (!user.is_admin()).then_some(user.id);
    state
        .db
        .call(move |c| store::get_job(c, owner.as_deref(), &job_id))
        .await?
        .map(Json)
        .ok_or_else(ApiError::not_found)
}
