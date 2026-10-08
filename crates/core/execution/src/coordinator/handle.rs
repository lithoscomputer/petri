//! The control path into a running coordinator: cancel, deliver a control,
//! pause and unpause.

use std::collections::BTreeSet;

use ir::{Control, FiringId};
use tokio::sync::{mpsc, oneshot};

use super::{Coordinator, CoordinatorError};
use crate::{CancelReason, CancelRequest, CoordinatorEvent, ExecutionId, InvocationId};

pub(super) struct ControlRequest {
    execution: ExecutionId,
    firing:    FiringId,
    control:   Control,
    reply:     oneshot::Sender<driver::DeliverDisposition>,
}

/// A request to record a run-level pause or unpause. The reply fires once
/// the record is durable, or at once when the state already says so.
pub(super) struct PauseRequest {
    paused: bool,
    reply:  oneshot::Sender<()>,
}

/// A cloneable control path for a coordinator that is currently running.
#[derive(Clone)]
pub struct CoordinatorHandle {
    cancel:  mpsc::UnboundedSender<CancelRequest>,
    control: mpsc::UnboundedSender<ControlRequest>,
    pause:   mpsc::UnboundedSender<PauseRequest>,
}

impl CoordinatorHandle {
    /// Politely cancel an invocation and every active descendant. A second
    /// request on an invocation that is already cancelled reaches its
    /// drivers again, which escalates them to the kill tier.
    pub fn cancel(&self, invocation: InvocationId) {
        let _ = self.cancel.send(CancelRequest {
            invocation,
            reason: None,
            escalate: true,
        });
    }

    /// Politely cancel the complete root run.
    pub fn cancel_root(&self) {
        self.cancel(InvocationId::ROOT);
    }

    /// Politely cancel the complete root run and record why: the reason
    /// rides the `InvocationCancelRequested` record and the public event.
    pub fn cancel_root_for(&self, reason: CancelReason) {
        let _ = self.cancel.send(CancelRequest {
            invocation: InvocationId::ROOT,
            reason:     Some(reason),
            escalate:   true,
        });
    }

    /// Deliver a control to one execution-local firing.
    pub async fn deliver(
        &self,
        execution: ExecutionId,
        firing: FiringId,
        control: Control,
    ) -> driver::DeliverDisposition {
        let (reply, result) = oneshot::channel();
        if self
            .control
            .send(ControlRequest {
                execution,
                firing,
                control,
                reply,
            })
            .is_err()
        {
            return driver::DeliverDisposition::NotLive;
        }
        result.await.unwrap_or(driver::DeliverDisposition::NotLive)
    }

    /// Record a run-level pause (`true`) or unpause (`false`) durably. The
    /// returned future completes once the record is on disk and every
    /// observer has seen it, so a caller that releases admission afterwards
    /// never releases before the unpause is durable. A redundant request
    /// (the state already says so) records nothing. Completes at once when
    /// the coordinator is gone: nothing is left to record against.
    /// Ask for a pause or unpause to be recorded, without waiting: the
    /// request joins the coordinator's queue now, so requests are recorded
    /// in the order they were made.
    pub fn request_paused(&self, paused: bool) {
        let (reply, _recorded) = oneshot::channel();
        let _ = self.pause.send(PauseRequest { paused, reply });
    }

    pub async fn set_paused(&self, paused: bool) {
        let (reply, recorded) = oneshot::channel();
        if self.pause.send(PauseRequest { paused, reply }).is_err() {
            return;
        }
        let _ = recorded.await;
    }
}

impl Coordinator {
    pub fn handle(&self) -> CoordinatorHandle {
        CoordinatorHandle {
            cancel:  self.cancel_tx.clone(),
            control: self.control_tx.clone(),
            pause:   self.pause_tx.clone(),
        }
    }

    /// Record a pause or unpause when it changes the recorded state, then
    /// tell the requester. The reply is sent after the append and after
    /// every observer saw the record, so an observer-derived event is queued
    /// before the requester acts on it.
    pub(super) async fn handle_pause(
        &mut self,
        request: PauseRequest,
    ) -> Result<(), CoordinatorError> {
        let PauseRequest { paused, reply } = request;
        if self.store.state().paused != paused {
            self.append(if paused {
                CoordinatorEvent::RunPaused
            } else {
                CoordinatorEvent::RunUnpaused
            })
            .await?;
        }
        let _ = reply.send(());
        Ok(())
    }

    pub(super) async fn handle_cancel(
        &mut self,
        request: CancelRequest,
    ) -> Result<(), CoordinatorError> {
        let CancelRequest {
            invocation: cancelled,
            reason,
            escalate,
        } = request;
        if !self.store.state().invocations.contains_key(&cancelled) {
            return Ok(());
        }

        let affected: Vec<_> = self
            .store
            .state()
            .invocations
            .iter()
            .filter_map(|(candidate, state)| {
                (state.result.is_none() && self.is_descendant_or_same(*candidate, cancelled))
                    .then_some((*candidate, state.cancelled))
            })
            .collect();
        self.cancel_invocations(affected, cancelled, reason, escalate)
            .await
    }

    /// Record the cancel of every affected invocation; the reason goes on
    /// the one the requester named, the descendants follow from it. The
    /// drivers of the newly cancelled invocations are told to cancel; a
    /// driver that was already cancelled is told again only when the request
    /// escalates, which is what reaches its kill tier.
    pub(super) async fn cancel_invocations(
        &mut self,
        affected: Vec<(InvocationId, bool)>,
        requested: InvocationId,
        reason: Option<CancelReason>,
        escalate: bool,
    ) -> Result<(), CoordinatorError> {
        for (invocation, already_cancelled) in &affected {
            if !already_cancelled {
                self.append(CoordinatorEvent::InvocationCancelRequested {
                    invocation: *invocation,
                    reason:     (*invocation == requested).then(|| reason.clone()).flatten(),
                })
                .await?;
            }
        }

        let affected: BTreeSet<_> = affected
            .into_iter()
            .filter(|(_, already_cancelled)| escalate || !already_cancelled)
            .map(|(invocation, _)| invocation)
            .collect();
        for handle in self.live.handles_of(&affected) {
            handle.cancel(ir::CancelScopeId::ROOT).await;
        }
        Ok(())
    }

    pub(super) fn handle_control(&self, request: ControlRequest) {
        let Some(handle) = self.live.handle_of(request.execution) else {
            let _ = request.reply.send(driver::DeliverDisposition::NotLive);
            return;
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let disposition = handle.deliver(request.firing, request.control).await;
            let _ = request.reply.send(disposition);
        });
    }
}
