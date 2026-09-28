//! Link flows for new ChatGPT accounts. They live in memory for at most 15 minutes; the code
//! verifiers never reach the database.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{accounts, models, oauth};
use crate::crypto::random_token;
use crate::state::AppState;

const FLOW_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_FLOWS: usize = 100;

#[derive(Default)]
pub struct LinkFlows {
    flows: Mutex<HashMap<String, Flow>>,
}

struct Flow {
    started: Instant,
    /// The admin session that started the flow. Tokens are saved only while it exists.
    session: i64,
    state: FlowState,
}

#[derive(Clone)]
pub enum FlowState {
    Device,
    Pkce {
        verifier: String,
        oauth_state: String,
    },
    /// The code exchange of a PKCE flow is running.
    Completing,
    /// The tokens are being saved. Cancelling does not stop this step.
    Saving,
    Done {
        account: i64,
    },
    Failed {
        message: String,
    },
}

impl LinkFlows {
    fn insert(&self, session: i64, state: FlowState) -> Result<String, String> {
        let mut flows = self.flows.lock().expect("link flows");
        flows.retain(|_, flow| flow.started.elapsed() < FLOW_TIMEOUT);
        if flows.len() >= MAX_FLOWS {
            return Err("Too many open sign-ins. Wait a few minutes.".into());
        }
        let id = random_token(18);
        flows.insert(
            id.clone(),
            Flow {
                started: Instant::now(),
                session,
                state,
            },
        );
        Ok(id)
    }

    /// The state of a flow of this admin session.
    pub fn get(&self, id: &str, session: i64) -> Option<FlowState> {
        let flows = self.flows.lock().expect("link flows");
        flows
            .get(id)
            .filter(|flow| flow.started.elapsed() < FLOW_TIMEOUT && flow.session == session)
            .map(|flow| flow.state.clone())
    }

    fn set(&self, id: &str, state: FlowState) {
        if let Some(flow) = self.flows.lock().expect("link flows").get_mut(id) {
            flow.state = state;
        }
    }

    /// Removes the flow. A device flow that is still polling stops at its next step.
    fn session_of(&self, id: &str) -> Option<i64> {
        let flows = self.flows.lock().expect("link flows");
        flows
            .get(id)
            .filter(|flow| flow.started.elapsed() < FLOW_TIMEOUT)
            .map(|flow| flow.session)
    }

    /// Cancels a flow of this session. A flow that is already saving its tokens finishes.
    pub fn cancel(&self, id: &str, session: i64) {
        let mut flows = self.flows.lock().expect("link flows");
        if flows
            .get(id)
            .is_some_and(|flow| flow.session == session && !matches!(flow.state, FlowState::Saving))
        {
            flows.remove(id);
        }
    }

    /// Marks the flow as saving and returns its session, or `None` when it was cancelled or
    /// expired. After this, a cancel cannot remove the flow any more.
    fn claim_for_save(&self, id: &str) -> Option<i64> {
        let mut flows = self.flows.lock().expect("link flows");
        let flow = flows.get_mut(id).filter(|flow| flow.started.elapsed() < FLOW_TIMEOUT)?;
        flow.state = FlowState::Saving;
        Some(flow.session)
    }

    /// Takes a PKCE flow for completion, so the code is exchanged only once.
    fn take_pkce(&self, id: &str, session: i64) -> Option<(String, String)> {
        let mut flows = self.flows.lock().expect("link flows");
        let flow = flows
            .get_mut(id)
            .filter(|flow| flow.started.elapsed() < FLOW_TIMEOUT && flow.session == session)?;
        let FlowState::Pkce { verifier, oauth_state } = flow.state.clone() else {
            return None;
        };
        flow.state = FlowState::Completing;
        Some((verifier, oauth_state))
    }
}

pub struct DeviceStart {
    pub flow: String,
    pub user_code: String,
    pub verification_url: &'static str,
}

/// Starts a device code login and polls in the background until the user confirms it.
pub async fn start_device(state: &AppState, session: i64) -> Result<DeviceStart, String> {
    let device = oauth::request_device_code(&state.http).await.map_err(|err| {
        format!("Cannot start the device login: {err}. Device login must be allowed in the ChatGPT security settings.")
    })?;
    let flow = state.link_flows.insert(session, FlowState::Device)?;
    let start = DeviceStart {
        flow: flow.clone(),
        user_code: device.user_code.clone(),
        verification_url: oauth::DEVICE_VERIFICATION_URL,
    };

    let state = state.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        loop {
            tokio::time::sleep(device.interval).await;
            if state.link_flows.session_of(&flow).is_none() {
                return;
            }
            if started.elapsed() >= FLOW_TIMEOUT {
                state.link_flows.set(&flow, failed("The code expired. Start again."));
                return;
            }
            match oauth::poll_device_code(&state.http, &device).await {
                Ok(None) => continue,
                Ok(Some(tokens)) => {
                    state.link_flows.set(&flow, finish(&state, &flow, &tokens).await);
                    return;
                }
                Err(err) => {
                    state
                        .link_flows
                        .set(&flow, failed(&format!("The sign-in failed: {err}")));
                    return;
                }
            }
        }
    });
    Ok(start)
}

pub struct PkceStart {
    pub flow: String,
    pub authorize_url: String,
}

pub fn start_pkce(state: &AppState, session: i64) -> Result<PkceStart, String> {
    let pkce = oauth::start_pkce();
    let flow = state.link_flows.insert(
        session,
        FlowState::Pkce {
            verifier: pkce.verifier,
            oauth_state: pkce.state,
        },
    )?;
    Ok(PkceStart {
        flow,
        authorize_url: pkce.authorize_url,
    })
}

/// Completes a PKCE login with the redirect URL that the admin pasted.
pub async fn complete_pkce(state: &AppState, flow: &str, session: i64, pasted_url: &str) -> FlowState {
    let (code, returned_state) = match oauth::parse_callback(pasted_url) {
        Ok(parts) => parts,
        Err(message) => return failed(&message),
    };
    let Some((verifier, expected_state)) = state.link_flows.take_pkce(flow, session) else {
        return failed("This sign-in expired or was already used. Start again.");
    };
    if returned_state != expected_state {
        let result = failed("The address belongs to another sign-in. Start again.");
        state.link_flows.set(flow, result.clone());
        return result;
    }
    // Own task: a dropped browser request cannot stop the one-time code exchange half-way.
    let task_state = state.clone();
    let task_flow = flow.to_owned();
    tokio::spawn(async move {
        let result = match oauth::exchange_code(&task_state.http, &code, &verifier, oauth::PKCE_REDIRECT).await {
            Ok(tokens) => finish(&task_state, &task_flow, &tokens).await,
            Err(err) => failed(&format!("The sign-in failed: {err}")),
        };
        task_state.link_flows.set(&task_flow, result.clone());
        result
    })
    .await
    .unwrap_or_else(|_| failed("The sign-in stopped. Start again."))
}

/// SQL condition (literal, for `concat!`): the admin session `?` is a full, active session of an
/// enabled admin.
macro_rules! session_active {
    () => {
        "EXISTS (SELECT 1 FROM sessions s JOIN admins a ON a.id = s.admin_id
             WHERE s.id = ? AND s.pending_second_factor = 0 AND s.expires_at > unixepoch() AND a.disabled = 0)"
    };
}
pub(crate) use session_active;

async fn finish(state: &AppState, flow: &str, tokens: &oauth::TokenSet) -> FlowState {
    let Some(session) = state.link_flows.claim_for_save(flow) else {
        return failed("The sign-in was cancelled.");
    };
    match accounts::store_link(state, tokens, session).await {
        Ok(account) => {
            if let Err(err) = models::sync_account(state, account).await {
                tracing::warn!(account, error = %err, "cannot load the model list");
            }
            state.schedule_changed.notify_one();
            FlowState::Done { account }
        }
        Err(accounts::LinkError::Cancelled) => failed("The sign-in was cancelled."),
        Err(err) => failed(&format!("Cannot save the account: {err}")),
    }
}

fn failed(message: &str) -> FlowState {
    FlowState::Failed {
        message: message.to_owned(),
    }
}
