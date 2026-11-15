//! Placement: which client gets a queued job. A pure function, so the
//! rules can be tested without clients, sockets or a database.

use protocol::ModelState;
use std::time::Instant;

/// A client the scheduler is considering for one job.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub client_id: String,
    /// Whether the job may run here at all: the pinned client for a pinned
    /// job, or a client in the job owner's pool for a pooled one.
    pub permitted: bool,
    /// The job's model in this client's inventory. None when the client
    /// isn't connected, doesn't have the model, or can't use it right now.
    pub model_state: Option<ModelState>,
    /// Attempts it's working on now.
    pub active: usize,
    /// One of those attempts uses a different model. llama.cpp loads one
    /// model at a time, so a request for another would be refused.
    pub busy_with_another_model: bool,
    /// When its last attempt ended. None: not since the server started.
    pub last_finished: Option<Instant>,
}

/// Warm models first: a loaded one answers in seconds, a cold one can take
/// a minute to load.
fn warmth(state: ModelState) -> Option<u8> {
    match state {
        ModelState::Loaded => Some(0),
        ModelState::Loading => Some(1),
        ModelState::Available => Some(2),
        ModelState::Downloading | ModelState::Failed => None,
    }
}

/// The client to place a job on, or None if no candidate is eligible.
///
/// Eligible: permitted, offering the model in a usable state, with a free
/// slot (`active < max_concurrent`), and not mid-reply with another model.
/// Among those, in order:
///
/// 1. not `avoid` (the client that just lost this job), unless it's the
///    only eligible one: it may have reconnected, but whatever lost the job
///    there may happen again;
/// 2. the model already loaded, then loading, then merely available;
/// 3. fewest active attempts;
/// 4. idle longest (spreads the load), then `client_id`, so the choice is
///    deterministic.
pub fn pick<'a>(avoid: Option<&str>, max_concurrent: usize, candidates: &'a [Candidate]) -> Option<&'a str> {
    candidates
        .iter()
        .filter(|c| c.permitted && c.active < max_concurrent && !c.busy_with_another_model)
        .filter_map(|c| Some((c, warmth(c.model_state?)?)))
        .min_by(|(a, a_warmth), (b, b_warmth)| {
            let avoided = |c: &Candidate| avoid == Some(c.client_id.as_str());
            avoided(a)
                .cmp(&avoided(b))
                .then(a_warmth.cmp(b_warmth))
                .then(a.active.cmp(&b.active))
                // None (never used) sorts before any time: idle longest.
                .then(a.last_finished.cmp(&b.last_finished))
                .then(a.client_id.cmp(&b.client_id))
        })
        .map(|(c, _)| c.client_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(id: &str, state: ModelState) -> Candidate {
        Candidate {
            client_id: id.into(),
            permitted: true,
            model_state: Some(state),
            active: 0,
            busy_with_another_model: false,
            last_finished: None,
        }
    }

    #[test]
    fn warm_models_win_then_the_least_busy_client() {
        let cold = candidate("a", ModelState::Available);
        let loading = candidate("b", ModelState::Loading);
        let warm = candidate("c", ModelState::Loaded);
        assert_eq!(pick(None, 2, &[cold.clone(), loading.clone(), warm.clone()]), Some("c"));
        assert_eq!(pick(None, 2, &[cold.clone(), loading.clone()]), Some("b"));
        assert_eq!(pick(None, 2, std::slice::from_ref(&cold)), Some("a"));

        // A loaded model on a client with one reply running still beats a
        // cold one elsewhere, but between equals the idle client wins.
        let warm_busy = Candidate {
            active: 1,
            ..warm.clone()
        };
        assert_eq!(pick(None, 2, &[cold.clone(), warm_busy.clone()]), Some("c"));
        let warm_idle = candidate("d", ModelState::Loaded);
        assert_eq!(pick(None, 2, &[warm_busy, warm_idle]), Some("d"));
    }

    #[test]
    fn full_clients_and_clients_running_another_model_are_never_picked() {
        let full = Candidate {
            active: 1,
            ..candidate("a", ModelState::Loaded)
        };
        assert_eq!(pick(None, 1, std::slice::from_ref(&full)), None);
        assert_eq!(pick(None, 2, std::slice::from_ref(&full)), Some("a"), "a second slot");
        let other_model = Candidate {
            active: 1,
            busy_with_another_model: true,
            ..candidate("b", ModelState::Available)
        };
        assert_eq!(pick(None, 4, std::slice::from_ref(&other_model)), None);
        // A cold idle client is better than nothing.
        let cold = candidate("c", ModelState::Available);
        assert_eq!(pick(None, 1, &[full, other_model, cold]), Some("c"));
    }

    #[test]
    fn clients_that_cannot_run_the_model_are_never_picked() {
        let offline = Candidate {
            model_state: None,
            ..candidate("a", ModelState::Loaded)
        };
        let downloading = candidate("b", ModelState::Downloading);
        let broken = candidate("c", ModelState::Failed);
        assert_eq!(pick(None, 1, &[offline, downloading, broken]), None);
        assert_eq!(pick(None, 1, &[]), None);
    }

    #[test]
    fn the_client_that_just_lost_the_job_is_avoided_unless_it_is_the_only_one() {
        let lost = candidate("a", ModelState::Loaded);
        let other = candidate("b", ModelState::Available);
        // Even though the model is warm on the one that lost it.
        assert_eq!(pick(Some("a"), 1, &[lost.clone(), other.clone()]), Some("b"));
        assert_eq!(
            pick(Some("a"), 1, std::slice::from_ref(&lost)),
            Some("a"),
            "it reconnected"
        );
        let other_full = Candidate { active: 1, ..other };
        assert_eq!(pick(Some("a"), 1, &[lost, other_full]), Some("a"));
    }

    #[test]
    fn clients_outside_the_pool_are_never_picked() {
        let someone_elses = Candidate {
            permitted: false,
            ..candidate("a", ModelState::Loaded)
        };
        assert_eq!(pick(None, 1, std::slice::from_ref(&someone_elses)), None);
        let mine = Candidate {
            active: 1,
            ..candidate("b", ModelState::Available)
        };
        // Busy and cold, but it's the only one the job may use.
        assert_eq!(pick(None, 2, &[someone_elses, mine]), Some("b"));
    }

    #[test]
    fn ties_go_to_the_client_idle_longest_then_by_id() {
        let now = Instant::now();
        let recent = Candidate {
            last_finished: Some(now),
            ..candidate("a", ModelState::Loaded)
        };
        let earlier = Candidate {
            last_finished: Some(now - Duration::from_secs(60)),
            ..candidate("b", ModelState::Loaded)
        };
        let never = candidate("c", ModelState::Loaded);
        assert_eq!(pick(None, 1, &[recent.clone(), earlier.clone(), never]), Some("c"));
        assert_eq!(pick(None, 1, &[recent, earlier]), Some("b"));
        let (x, y) = (candidate("y", ModelState::Loaded), candidate("x", ModelState::Loaded));
        assert_eq!(pick(None, 1, &[x, y]), Some("x"), "deterministic");
    }
}
