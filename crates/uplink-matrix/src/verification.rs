//! Verifying uplink's device with another of the account's, by comparing
//! emoji (SAS), as clients do (SPEC §9.4). Only of the account's own
//! devices: requests of other users go unanswered.
//!
//! A verification's state is the SDK's; the account's [`Clients`] entry
//! keeps its requests, to list them. Each is watched while it lasts:
//! comparing starts once a request is accepted, a comparison the other
//! device starts is accepted, and every change is pushed as a
//! `SecurityChanged` notice (SPEC §5.5).

use crate::client::Clients;
use matrix_sdk::{
    encryption::verification::{
        SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
    },
    ruma::events::key::verification::VerificationMethod,
    stream::StreamExt,
};
use uplink_protocol::{
    matrix::{MatrixVerification, SasEmoji, SecurityChanged, VerificationPhase},
    Notice,
};
use uplink_tx_log::Dispatcher;

/// The methods uplink offers: emoji only, as it shows no camera or code.
pub fn methods() -> Vec<VerificationMethod> {
    vec![VerificationMethod::SasV1]
}

/// Keeps the request, to list it, and watches it while it lasts.
pub fn track(
    clients: &Clients,
    dispatcher: &Dispatcher,
    account: &str,
    request: VerificationRequest,
) {
    clients.track(account, request.clone());
    notify(dispatcher, account);
    tokio::spawn(watch(dispatcher.clone(), account.to_owned(), request));
}

async fn watch(dispatcher: Dispatcher, account: String, request: VerificationRequest) {
    let mut changes = request.changes();
    while let Some(state) = changes.next().await {
        notify(&dispatcher, &account);
        match state {
            VerificationRequestState::Ready { .. } => {
                if let Err(e) = request.start_sas().await {
                    tracing::warn!(
                        account,
                        flow = request.flow_id(),
                        "starting to compare: {e:#}"
                    );
                }
            }
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
                ..
            } => {
                watch_sas(&dispatcher, &account, sas).await;
                return;
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => return,
            _ => {}
        }
    }
}

async fn watch_sas(dispatcher: &Dispatcher, account: &str, sas: SasVerification) {
    let mut changes = sas.changes();
    // Of a comparison the other device started.
    if matches!(sas.state(), SasState::Started { .. }) {
        if let Err(e) = sas.accept().await {
            tracing::warn!(account, "accepting to compare: {e:#}");
        }
    }
    notify(dispatcher, account);
    while let Some(state) = changes.next().await {
        notify(dispatcher, account);
        if matches!(state, SasState::Done { .. } | SasState::Cancelled(_)) {
            return;
        }
    }
}

fn notify(dispatcher: &Dispatcher, account: &str) {
    dispatcher
        .hub()
        .notify(Notice::SecurityChanged(SecurityChanged {
            account: account.to_owned(),
        }));
}

/// The request as the protocol shows it.
pub fn shown(request: &VerificationRequest) -> MatrixVerification {
    let (other_device, phase) = match request.state() {
        VerificationRequestState::Created { .. } => (None, VerificationPhase::Requested),
        VerificationRequestState::Requested {
            other_device_data, ..
        } => (
            Some(other_device_data.device_id().to_string()),
            VerificationPhase::Requested,
        ),
        VerificationRequestState::Ready {
            other_device_data, ..
        } => (
            Some(other_device_data.device_id().to_string()),
            VerificationPhase::Ready,
        ),
        VerificationRequestState::Transitioned { verification } => match verification {
            Verification::SasV1(sas) => (
                Some(sas.other_device().device_id().to_string()),
                sas_phase(&sas),
            ),
            // Not offered (see `methods`).
            other => unreachable!("a verification by {other:?}"),
        },
        VerificationRequestState::Done => (None, VerificationPhase::Done),
        VerificationRequestState::Cancelled(info) => (
            None,
            VerificationPhase::Cancelled {
                reason: info.reason().to_owned(),
                by_us: info.cancelled_by_us(),
            },
        ),
    };
    MatrixVerification {
        flow_id: request.flow_id().to_owned(),
        other_device,
        we_started: request.we_started(),
        phase,
    }
}

fn sas_phase(sas: &SasVerification) -> VerificationPhase {
    match sas.state() {
        SasState::Created { .. } | SasState::Started { .. } | SasState::Accepted { .. } => {
            VerificationPhase::Started
        }
        SasState::KeysExchanged { decimals, .. } => VerificationPhase::Compare {
            emoji: sas
                .emoji()
                .into_iter()
                .flatten()
                .map(|e| SasEmoji {
                    symbol: e.symbol.to_owned(),
                    description: e.description.to_owned(),
                })
                .collect(),
            decimals: vec![decimals.0, decimals.1, decimals.2],
        },
        SasState::Confirmed => VerificationPhase::Confirmed,
        SasState::Done { .. } => VerificationPhase::Done,
        SasState::Cancelled(info) => VerificationPhase::Cancelled {
            reason: info.reason().to_owned(),
            by_us: info.cancelled_by_us(),
        },
    }
}

/// The comparison the request went on to, if it has.
pub fn sas(request: &VerificationRequest) -> Option<SasVerification> {
    match request.state() {
        VerificationRequestState::Transitioned {
            verification: Verification::SasV1(sas),
            ..
        } => Some(sas),
        _ => None,
    }
}
