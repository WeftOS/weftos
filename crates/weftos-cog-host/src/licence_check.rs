//! Socket half of the ADR-106 start check.
//!
//! Production calls `cog.check_run` and then applies the two host overrides
//! (a configured directory is not `not_seed_bound`; an unreadable revocation
//! list rejects a permit). [`HostLicence::evaluate_local`] is the raw kernel
//! verdict for the test listener. The listener must not call [`HostLicence::check`]:
//! that would open the socket again.

use std::path::PathBuf;

use clawft_kernel::licence::{RunPermit, RunRefusal, RunRequest, RunVerdict};
#[cfg(test)]
use clawft_kernel::licence::check_run;
#[cfg(unix)]
use weftos_cog_protocol::{CallError, CheckRunResult, DaemonCheck, RefusalCode};
use weftos_cog_protocol::CheckRunParams;

use super::{HostLicence, Stores, TRANSPORT_MARK};

impl HostLicence {
    pub(super) fn check_ready(&self, stores: &Stores, req: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        let params = CheckRunParams::new(req.cog_id, req.version, req.sha256, req.blake3)
            .map_err(|e| mark_transport("malformed_reply", e))?;
        let verdict = self.ask_daemon(&params)?;
        if let RunVerdict::Permit(permit) = &verdict
            && permit.blake3 != req.blake3
        {
            return Err(mark_transport("malformed_reply", "permit blake3 does not match the request"));
        }
        // A configured directory means the operator intends to bind this Seed.
        // Until a binding is imported, a Cognitum-origin start is refused.
        // No directory at all stays `NotSeedBound` and never reaches here.
        if matches!(verdict, RunVerdict::NotSeedBound) {
            return Err(RunRefusal::BindingInactive(format!(
                "the licence directory {} is configured but holds no binding yet: import the signed binding first",
                self.dir.display()
            )));
        }
        if let (RunVerdict::Permit(_), Some(e)) = (&verdict, stores.revocations.subjects_error()) {
            return Err(RunRefusal::BindingInactive(format!("revocation list unreadable: {e}")));
        }
        Ok(verdict)
    }

    /// Kernel `check_run` for one request. No host overrides and no socket.
    #[cfg(test)]
    pub(super) fn evaluate_local(&self, req: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        match &*self.state() {
            super::State::Ready(s) => check_run(&s.grants, Some(&s.approvals), req),
            super::State::Unconfigured => Ok(RunVerdict::NotSeedBound),
            super::State::Broken(e) => Err(RunRefusal::BindingInactive(format!("licence state unusable: {e}"))),
        }
    }

    #[cfg(unix)]
    fn ask_daemon(&self, params: &CheckRunParams) -> Result<RunVerdict, RunRefusal> {
        let socket = self.resolve_socket()?;
        match DaemonCheck::new(socket).with_timeout(self.daemon_timeout).check(params) {
            Ok(CheckRunResult::NotSeedBound) => Ok(RunVerdict::NotSeedBound),
            Ok(CheckRunResult::Permit { grant_id, approval_id, blake3 }) => {
                Ok(RunVerdict::Permit(RunPermit { grant_id, approval_id, blake3 }))
            }
            Err(e) => Err(refusal_from_call(e)),
        }
    }

    #[cfg(not(unix))]
    fn ask_daemon(&self, params: &CheckRunParams) -> Result<RunVerdict, RunRefusal> {
        let _ = (params, &self.daemon_socket, self.daemon_timeout);
        Err(mark_transport("daemon_unavailable", "cog.check_run needs a Unix socket"))
    }

    /// An explicit socket wins over `$WEFTOS_RUNTIME_DIR/kernel.sock`.
    #[cfg(unix)]
    fn resolve_socket(&self) -> Result<PathBuf, RunRefusal> {
        if let Some(path) = &self.daemon_socket {
            return Ok(path.clone());
        }
        match std::env::var("WEFTOS_RUNTIME_DIR") {
            Ok(dir) if !dir.trim().is_empty() => Ok(PathBuf::from(dir.trim()).join("kernel.sock")),
            _ => Err(mark_transport(
                "daemon_unavailable",
                "WEFTOS_RUNTIME_DIR is unset and no daemon socket was configured",
            )),
        }
    }
}

fn mark_transport(code: &str, detail: impl std::fmt::Display) -> RunRefusal {
    RunRefusal::BindingInactive(format!("{TRANSPORT_MARK}{code}:{detail}"))
}

#[cfg(unix)]
fn refusal_from_call(err: CallError) -> RunRefusal {
    match err {
        CallError::DaemonUnavailable(m) => mark_transport("daemon_unavailable", m),
        CallError::MalformedReply(m) => mark_transport("malformed_reply", m),
        CallError::Timeout => mark_transport("timeout", "the daemon did not answer before the deadline"),
        CallError::VersionMismatch(m) => mark_transport("version_mismatch", m),
        CallError::Denial { code, message } => match code {
            RefusalCode::BindingInactive => RunRefusal::BindingInactive(message),
            RefusalCode::NoGrant => RunRefusal::NoGrant,
            RefusalCode::GrantLapsed => RunRefusal::GrantLapsed,
            RefusalCode::NotInGrant => RunRefusal::NotInGrant,
            RefusalCode::HashRevoked => RunRefusal::HashRevoked,
            RefusalCode::NoApproval => RunRefusal::NoApproval,
            RefusalCode::NotHolder => RunRefusal::NotHolder(message),
            RefusalCode::DaemonUnavailable => mark_transport("daemon_unavailable", message),
        },
    }
}
