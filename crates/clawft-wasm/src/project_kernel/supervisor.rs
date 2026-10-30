//! Guest identity, certification, tighten-only governance and signed chain.
use crate::identity::{ack_check, cert_check, engine, governance_overlay_effect, parent, policy};
use crate::{
    Result, abi, bridge,
    chain::ChainManager,
    forward::Forward,
    governance::{GovernanceDecision, GovernanceEngine, GovernanceRequest},
    governance_overlay::Effective,
    mesh_local::*,
    storage,
};
use chrono::Utc;
use clawft_types::project::{
    ProjectAnchorStmt, ProjectCert,
    canon::{hex_decode, hex_encode},
    cert::{PopOp, key_id, pop_signed_bytes},
};
use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

struct Kernel {
    boot: abi::Boot,
    user: [u8; 32],
    key: SigningKey,
    cert: Option<ProjectCert>,
    session: Option<String>,
    heartbeat: Duration,
    chain: ChainManager,
    effective: Effective,
    engine: GovernanceEngine,
    forward: Forward,
    parent_up: bool,
    last_activity: u64,
    stopping: bool,
}


impl Kernel {
    fn register(&mut self) -> Result<()> {
        let challenge: ChallengeReply = serde_json::from_value(parent(
            &self.boot.project_id,
            METHOD_CHALLENGE,
            json!({"project_id":self.boot.project_id}),
        )?)?;
        let mut client = [0; 16];
        rand::rngs::OsRng.fill_bytes(&mut client);
        let client = hex_encode(&client);
        let pop = pop_signed_bytes(
            PopOp::Register,
            &key_id(&self.user),
            &challenge.nonce,
            &self.boot.project_id,
        )?;
        let bind = bind_signed_bytes(
            &self.boot.project_id,
            &challenge.nonce,
            &client,
            &self.boot.socket,
            self.boot.pid,
        );
        let req = RegisterRequest {
            protocol: PROTOCOL_TAG.to_owned(),
            role: MeshRole::Project,
            project_id: self.boot.project_id.clone(),
            project_pubkey: hex_encode(&self.key.verifying_key().to_bytes()),
            cert: self.cert.clone(),
            addresses: vec![self.boot.project_id.clone()],
            topic_prefixes: vec![], // The slice does not advertise a streaming subscription implementation.
            version: env!("CARGO_PKG_VERSION").into(),
            build_sha: option_env!("WEFTOS_BUILD_SHA").unwrap_or("").into(),
            pid: self.boot.pid,
            socket: self.boot.socket.clone(),
            features: vec!["anchor".into()],
            container: None,
            client_nonce: client.clone(),
            bind_sig: hex_encode(&self.key.sign(&bind).to_bytes()),
            root_sha256: self.boot.root_sha256.clone(),
            spawn_nonce: self.boot.spawn_nonce.clone(),
            nonce_reply: NonceReply {
                nonce: challenge.nonce.clone(),
                sig: hex_encode(&self.key.sign(&pop).to_bytes()),
            },
        };
        let ack: RegisterAck = serde_json::from_value(parent(
            &self.boot.project_id,
            METHOD_REGISTER,
            serde_json::to_value(req)?,
        )?)?;
        let cert = ack_check(
            &ack,
            &self.boot,
            &self.key,
            &self.user,
            &challenge.nonce,
            &client,
        )?;
        storage::atomic(
            Path::new("/project/project.cert.json"),
            &serde_json::to_vec(&cert)?,
        )?;
        if self.chain.is_empty() {
            let head = ack
                .parent_head
                .as_ref()
                .ok_or("new project needs parent genesis head")?;
            self.chain.append(
                "project",
                "project.genesis",
                Some(json!({"project_id":self.boot.project_id,"cert":cert,"parent_head":head})),
            );
            self.record_policy()?;
        }
        self.cert = Some(cert);
        self.session = Some(ack.session);
        self.heartbeat = Duration::from_secs(ack.heartbeat_secs.clamp(1, 30));
        self.boot.spawn_nonce = None;
        self.parent_up = true;
        Ok(())
    }

    fn record_policy(&self) -> Result<()> {
        // Persist the rollback floor before committing the policy event. A
        // crash between the two can only tighten the next boot's floor.
        if fs::create_dir_all("/project/state").is_err()
            || storage::atomic(
                Path::new("/project/state/parent-policy.version"),
                self.effective.parent_version.to_string().as_bytes(),
            )
            .is_err()
        {
            std::process::exit(2);
        }
        self.chain.append("governance", "governance.overlay.applied", Some(json!({
            "parent_version":self.effective.parent_version,"overlay_hash":hex_encode(&self.effective.overlay_hash),
            "effective_hash":self.effective.effective_hash_hex(),"user_pin":true
        })));
        storage::durable(&self.chain)
    }

    fn beat(&mut self) -> Result<()> {
        if let Some(cert) = &self.cert {
            cert_check(cert, &self.boot, &self.key, &self.user)?;
        }
        let Some(session) = self.session.clone() else {
            return self.register();
        };
        let activity = Activity {
            last_activity_unix: self.last_activity,
            busy: Busy::default(),
        };
        let at = Utc::now().timestamp().max(0) as u64;
        let proof = session_signed_bytes(
            "heartbeat",
            &session,
            self.boot.pid,
            at,
            &activity_digest(&activity),
        );
        let req = HeartbeatRequest {
            session,
            pid: self.boot.pid,
            at_unix: at,
            sig: hex_encode(&self.key.sign(&proof).to_bytes()),
            activity,
        };
        match parent(
            &self.boot.project_id,
            METHOD_HEARTBEAT,
            serde_json::to_value(req)?,
        ) {
            Ok(_) => {
                self.parent_up = true;
                Ok(())
            }
            Err(e)
                if matches!(
                    e.to_string().as_str(),
                    "unknown_session" | "session_expired"
                ) =>
            {
                self.session = None;
                self.register()
            }
            Err(e) => Err(e),
        }
    }

    fn anchor(&self) -> Result<()> {
        let cert = self.cert.as_ref().ok_or("uncertified")?;
        let last = self
            .chain
            .tail(0)
            .into_iter()
            .rev()
            .find(|e| e.source == "project.anchor" && e.kind == "project.anchored")
            .and_then(|e| {
                serde_json::from_value::<ProjectAnchorStmt>(e.payload?["statement"].clone()).ok()
            });
        let path = Path::new("/project/chain/anchor-wasm-pending.json");
        let pending = match fs::read(path) {
            Ok(bytes) => Some(serde_json::from_slice::<ProjectAnchorStmt>(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let (Some(p), Some(l)) = (&pending, &last)
            && p.hash() == l.hash()
        {
            fs::remove_file(path)?;
            return Ok(());
        }
        let stmt = if let Some(p) = pending {
            p.verify(&self.key.verifying_key().to_bytes())?;
            if p.project_id != self.boot.project_id
                || p.seq != last.as_ref().map_or(1, |l| l.seq + 1)
                || p.prev_anchor != last.as_ref().map(|l| l.hash())
            {
                return Err("pending anchor continuity mismatch".into());
            }
            p
        } else {
            let stmt = ProjectAnchorStmt {
                project_id: self.boot.project_id.clone(),
                project_key_id: cert.project_key_id.clone(),
                cert_serial: cert.serial,
                seq: last.as_ref().map_or(1, |l| l.seq + 1),
                chain_id: self.chain.chain_id() as u64,
                head_hash: hex_encode(&self.chain.head_hash()),
                head_seq: self.chain.head_sequence(),
                rule_hash: self.effective.effective_hash_hex(),
                at: clawft_types::project::cert::ts(Utc::now()),
                prev_anchor: last.map(|l| l.hash()),
                sig: String::new(),
            }
            .sign(&self.key);
            storage::atomic(path, &serde_json::to_vec(&stmt)?)?;
            stmt
        };
        let ack = parent(
            &self.boot.project_id,
            "project.anchor.submit",
            serde_json::to_value(&stmt)?,
        )?;
        let seq = ack["user_seq"].as_u64().ok_or("bad anchor ack")?;
        let hash = ack["user_event_hash"].as_str().ok_or("bad anchor ack")?;
        if hex_decode::<32>(hash).is_none() {
            return Err("bad anchor ack hash".into());
        }
        self.chain.append(
            "project.anchor",
            "project.anchored",
            Some(json!({"statement":stmt,"user_seq":seq,"user_event_hash":hash})),
        );
        storage::durable(&self.chain)?;
        fs::remove_file(path)?;
        Ok(())
    }

    fn request(&mut self, req: &Value) -> Result<Value> {
        if let Some(cert) = &self.cert {
            cert_check(cert, &self.boot, &self.key, &self.user)?;
        }
        let method = req["method"].as_str().ok_or("missing method")?;
        if !req["project"].is_null() && req["project"].as_str() != Some(&self.boot.project_id) {
            return Err("project_mismatch".into());
        }
        if !req["proto"].is_null() && req["proto"] != 1 {
            return Err("proto_mismatch".into());
        }
        let public = matches!(method, "kernel.handshake" | "kernel.status");
        if !public || !req["forward"].is_null() {
            self.forward.verify(
                req,
                &self.boot.project_id,
                &key_id(&self.key.verifying_key().to_bytes()),
                &self.user,
                Utc::now().timestamp_millis().max(0) as u64,
            )?;
        }
        if !public && req["proto"] != 1 {
            return Err("proto_mismatch".into());
        }
        if public {
            let mut status = json!({"proto":{"current":1,"min":1},"node_id":key_id(&self.key.verifying_key().to_bytes()),
                "user_key_id":key_id(&self.user),"profile":"project","roles":["project"],"project_id":self.boot.project_id,
                "bound_via":"project","pid":self.boot.pid,"runtime_dir":self.boot.runtime_dir,"depth":self.boot.depth,"parent":self.boot.parent,
                "version":env!("CARGO_PKG_VERSION"),"sha":option_env!("WEFTOS_BUILD_SHA").unwrap_or(""),
                "sandbox":"wasmtime","adapter":"wasmtime-project-v1","artifact_sha256":self.boot.artifact_sha256,
                "parent_up":self.parent_up,"rule_hash":self.effective.effective_hash_hex(),"chain_seq":self.chain.head_sequence()});
            if method == "kernel.handshake" && !req["params"]["challenge"].is_null() {
                let challenge = req["params"]["challenge"].as_str().ok_or("bad challenge")?;
                if hex_decode::<32>(challenge).is_none() {
                    return Err("bad challenge".into());
                }
                status["root_sha256"] = json!(self.boot.root_sha256);
                status["socket"] = json!(self.boot.socket);
                let body = clawft_types::project::canon::canonical_json(&status);
                let msg = format!("weftos-wasm-project-handshake-v1\n{challenge}\n{body}");
                status["guest_sig"] = json!(hex_encode(&self.key.sign(msg.as_bytes()).to_bytes()));
            }
            return Ok(status);
        }
        self.last_activity = Utc::now().timestamp().max(0) as u64;
        let mut request = GovernanceRequest::new("parent-forward", method);
        if method != "chain.status" {
            request.effect = governance_overlay_effect();
        }
        let decision = self.engine.evaluate(&request);
        self.chain.append(
            "governance",
            "governance.decision",
            Some(serde_json::to_value(&decision)?),
        );
        storage::durable(&self.chain)?;
        if !matches!(decision.decision, GovernanceDecision::Permit) {
            return Err("governance_denied".into());
        }
        match method {
            "kernel.stop" | "kernel.shutdown" => {
                self.stopping = true;
                Ok(json!({"stopping":true}))
            }
            "chain.status" => Ok(serde_json::to_value(self.chain.status())?),
            "chain.append" => {
                let source = req["params"]["source"].as_str().ok_or("missing source")?;
                let kind = req["params"]["kind"].as_str().ok_or("missing kind")?;
                if source.is_empty()
                    || kind.is_empty()
                    || crate::chain::is_caller_reserved_source(source)
                    || crate::chain::is_reserved_kind(kind)
                {
                    return Err("reserved chain event".into());
                }
                let ev = self
                    .chain
                    .append(source, kind, Some(req["params"]["payload"].clone()));
                storage::durable(&self.chain)?;
                Ok(serde_json::to_value(ev)?)
            }
            "governance.parent.update" => {
                let e = policy(
                    &self.boot,
                    &self.user,
                    &self.chain,
                    req["params"]["policy"].clone(),
                )?;
                let hash = e.effective_hash;
                self.chain
                    .set_rule_hash_provider(Arc::new(move || Some(hash)));
                self.engine = engine(&e);
                self.effective = e;
                self.record_policy()?;
                Ok(json!({"rule_hash":self.effective.effective_hash_hex()}))
            }
            _ => Err("unsupported_guest_method".into()),
        }
    }
}

pub fn run() -> Result<()> {
    let boot: abi::Boot = serde_json::from_slice(&bridge(abi::BOOT, &[])?)?;
    if boot.abi != abi::ABI || boot.pid == 0 || boot.socket.contains('\n') {
        return Err("bad bootstrap".into());
    }
    clawft_types::project::validate_id(&boot.project_id)?;
    let user = hex_decode::<32>(&boot.user_pubkey).ok_or("missing pinned user key")?;
    let key = storage::key()?;
    let chain = storage::load_chain(&key, storage::pq_key()?)?;
    let effective = policy(&boot, &user, &chain, boot.parent_policy.clone())?;
    let hash = effective.effective_hash;
    chain.set_rule_hash_provider(Arc::new(move || Some(hash)));
    let cert = match fs::read("/project/project.cert.json") {
        Ok(bytes) => {
            let cert = serde_json::from_slice(&bytes)?;
            cert_check(&cert, &boot, &key, &user)?;
            Some(cert)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    // Boot refuses an unverified/offline registration. A running certified
    // guest may survive a parent outage and replay the pending signed anchor.
    let mut k = Kernel {
        boot,
        user,
        key,
        cert,
        session: None,
        heartbeat: Duration::from_secs(5),
        chain,
        engine: engine(&effective),
        effective,
        forward: Forward::new(),
        parent_up: false,
        last_activity: Utc::now().timestamp().max(0) as u64,
        stopping: false,
    };
    k.register()?;
    clawft_kernel::governance_project::set_instance_project(
        crate::governance::ProjectAttestation::from_verified(
            k.boot.project_id.clone(),
            crate::governance::AttestSource::BoundKernel,
        ),
        key_id(&k.key.verifying_key().to_bytes()),
    );
    k.record_policy()?;
    let mut beat = Instant::now();
    let mut anchor = Instant::now() - Duration::from_secs(30);
    loop {
        if beat.elapsed() >= k.heartbeat {
            if let Err(e) = k.beat() {
                k.parent_up = false;
                let msg = e.to_string();
                let survivable = matches!(
                    msg.as_str(),
                    "parent_unavailable" | "unknown_session" | "session_expired"
                );
                // A certified guest awaiting signed adoption keeps retrying.
                let awaiting_adoption = msg == "spawn_not_expected"
                    && k.cert.is_some()
                    && k.boot.spawn_nonce.is_none()
                    && k.session.is_none();
                if !survivable && !awaiting_adoption {
                    return Err(e);
                }
            }
            beat = Instant::now();
        }
        if anchor.elapsed() >= Duration::from_secs(30) {
            // Any other failure leaves the pending statement durable for retry.
            if let Err(e) = k.anchor()
                && matches!(
                    e.to_string().as_str(),
                    "key_revoked" | "project_revoked" | "project_not_found"
                )
            {
                return Err(e);
            }
            anchor = Instant::now();
        }
        let bytes = bridge(abi::NEXT, &[])?;
        if bytes.is_empty() {
            continue;
        }
        let reply = match serde_json::from_slice::<Value>(&bytes) {
            Ok(req) => match k.request(&req) {
                Ok(value) => json!({"ok":true,"result":value,"id":req["id"]}),
                Err(e) => {
                    json!({"ok":false,"error":e.to_string(),"error_kind":"guest_refused","id":req["id"]})
                }
            },
            Err(_) => json!({"ok":false,"error":"invalid JSON","error_kind":"bad_request"}),
        };
        bridge(abi::REPLY, &serde_json::to_vec(&reply)?)?;
        if k.stopping {
            // One bounded final submission before unregister. Failure leaves
            // the signed pending statement for replay; shutdown never claims
            // successful delivery when the parent is unavailable.
            let _ = k.anchor();
            if let Some(session) = &k.session {
                let at = Utc::now().timestamp().max(0) as u64;
                let proof = session_signed_bytes("unregister", session, k.boot.pid, at, "");
                let _ = parent(
                    &k.boot.project_id,
                    METHOD_UNREGISTER,
                    json!({
                        "session":session,"pid":k.boot.pid,"at_unix":at,
                        "sig":hex_encode(&k.key.sign(&proof).to_bytes())
                    }),
                );
            }
            storage::durable(&k.chain)?;
            return Ok(());
        }
    }
}
