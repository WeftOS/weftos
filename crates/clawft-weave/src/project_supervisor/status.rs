//! Read-only views of the supervisor: per-project status and leftovers.

use super::*;

impl Supervisor {
    /// Status of one project.
    pub async fn status(&self, id: &str) -> Status {
        let slot = self.slot(id);
        let (state_, restarts, exit, failed, unregistered_secs) = {
            let st = slot.st();
            (
                st.state,
                st.restarts,
                st.last_exit,
                st.failed.clone(),
                st.unregistered_since
                    .map(|t| Instant::now().saturating_duration_since(t).as_secs()),
            )
        };
        let file = state::read(&self.run_dir(id)).unwrap_or_default();
        let probe = self.launcher.probe(id).await;
        let (pid, state_, failed) = match probe {
            ChildProbe::Running { identity } => (Some(identity.host_pid()), state_, failed),
            ChildProbe::Unverifiable { reason } => (None, ChildState::Failed, Some(reason)),
            _ => (None, state_, failed),
        };
        let stale_build = pid.is_some()
            && file
                .kernel_sha
                .as_deref()
                .is_some_and(|sha| sha != self.cfg.build_sha);
        Status {
            project_id: id.to_owned(),
            state: state_,
            pid,
            container: file.container,
            socket: self.socket(id),
            restarts,
            last_exit_code: exit.and_then(|e| e.code),
            failed_reason: failed,
            kernel_sha: file.kernel_sha,
            kernel_version: file.kernel_version,
            stale_build,
            unregistered_secs,
        }
    }

    /// Status of every project the supervisor knows.
    pub async fn status_all(&self) -> Vec<Status> {
        let ids: Vec<String> = self
            .slots
            .lock()
            .expect("slots lock poisoned")
            .keys()
            .cloned()
            .collect();
        let mut v = Vec::new();
        for id in ids {
            v.push(self.status(&id).await);
        }
        v.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        v
    }

    /// Leftover run dirs the last adoption scan could not verify.
    pub fn unverifiable(&self) -> Vec<Found> {
        self.leftovers
            .lock()
            .expect("leftovers lock poisoned")
            .iter()
            .filter(|f| {
                matches!(f, Found::Unverifiable { reason, .. } if *reason != adopt::Skip::Dead)
                    || matches!(f, Found::UnverifiableContainer { .. })
            })
            .cloned()
            .collect()
    }

    /// A live kernel for `id` that the supervisor does not manage (an
    /// adopted-but-refused leftover, or one that failed verification): its
    /// pid and why. `project.stop` names it instead of saying "was not
    /// running". It is never signalled.
    pub fn unmanaged(&self, id: &str) -> Option<(u32, String)> {
        self.leftovers
            .lock()
            .expect("leftovers lock poisoned")
            .iter()
            .find_map(|f| match f {
                Found::Unverifiable {
                    id: i,
                    pid: Some(pid),
                    reason,
                } if i == id && *reason != adopt::Skip::Dead && child::pid_alive(*pid) => {
                    Some((*pid, reason.to_string()))
                }
                _ => None,
            })
    }

    /// `via = child-kernel` for the project (the owner's opt-in).
    pub fn is_child_kernel(&self, id: &str) -> bool {
        matches!(
            clawft_types::project::find_by_id(&self.cfg.manifests_dir, id),
            Ok(Some(m)) if m.serve.as_ref().is_some_and(|s| s.via == ServeVia::ChildKernel)
        )
    }
}
