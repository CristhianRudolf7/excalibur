//! Live broker authorization. Synchronous callbacks run on dedicated broker
//! workers; durable security remains on the existing Tokio storage boundary.
use crate::{Action, Decision, Error, Result, Security};
use excalibur_types::runtime::{
    Effect, NetworkAdmission, NetworkAuthorizer, PreparedTool, ToolCall, ToolResult,
};
use serde_json::json;
use std::sync::Arc;
struct EgressGate {
    security: Security,
    run: String,
    runtime: tokio::runtime::Handle,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}
struct Admission {
    security: Security,
    run: String,
    runtime: tokio::runtime::Handle,
    prepared: PreparedTool,
    ticket: String,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}
impl Security {
    async fn ensure_network_run_active(&self, run: &str) -> Result<()> {
        let run = run.to_owned();
        let running = self
            .database()
            .read(excalibur_store::ReadLane::Interactive, move |connection| {
                Ok(crate::finalization::authoritative_run_state(connection, &run)? == "running")
            })
            .await?;
        if running {
            Ok(())
        } else {
            Err(Error::Denied(
                "network connections require an actively running execution".into(),
            ))
        }
    }

    /// Call inside the host runtime; invoke callbacks on dedicated broker workers,
    /// never on a Tokio executor thread. Every attempt rechecks rules/taint/expiry.
    pub fn network_authorizer(&self, run: impl Into<String>) -> Result<Arc<dyn NetworkAuthorizer>> {
        self.network_authorizer_with_clock(run, Arc::new(excalibur_store::unix_seconds))
    }
    /// Explicit host clock (Unix seconds), useful for deterministic recovery and
    /// expiry verification. It is never selected by a model/tool argument.
    pub fn network_authorizer_with_clock(
        &self,
        run: impl Into<String>,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<Arc<dyn NetworkAuthorizer>> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Denied("network guard requires a live host runtime".into()))?;
        Ok(Arc::new(EgressGate {
            security: self.clone(),
            run: run.into(),
            runtime,
            clock,
        }))
    }
}
impl EgressGate {
    fn admit(&self, domain: &str, port: u16) -> std::result::Result<Admission, String> {
        self.runtime.block_on(async {
            let now=(self.clock)();
            self.security.ensure_network_run_active(&self.run).await?;
            if self.security.redactor().contains_secret(domain.as_bytes()) {return Err(Error::Denied("secret literal destination requires a bound host secret executor".into()));}
            let action=Action::Network{domain:domain.to_owned()};
            match self.security.authorize(&self.run,&action,now).await? {Decision::Allow=>{},Decision::Ask{approval_id,..}=>return Err(Error::ApprovalPending(approval_id)),Decision::Deny{reason}=>return Err(Error::Denied(reason))}
            let prepared=PreparedTool{call:ToolCall{id:format!("network:{}",excalibur_types::RunId::new().ulid()),name:"__host_network_connect".into(),arguments:json!({"domain":domain,"port":port}).to_string()},effect:Effect::NonIdempotent,resources:Vec::new(),exclusive:false,data:json!({"domain":domain,"port":port})};
            let ticket=self.security.admit(&self.run,&prepared,std::slice::from_ref(&action),now).await?;
            self.security.validate_ticket(&self.run,&prepared,&ticket,now).await?;
            if !self.security.begin_dispatch(&ticket).await? {return Err(Error::Denied("network attempt already admitted".into()));}
            self.security.audit(json!({"kind":"network_connection_admitted","run":self.run,"domain":domain,"port":port,"at":now})).await?;
            self.security.finish_dispatch(&ticket,&ToolResult::ok(json!({"network_authorized":true}))).await?;
            Ok(Admission{security:self.security.clone(),run:self.run.clone(),runtime:self.runtime.clone(),prepared,ticket,clock:self.clock.clone()})
        }).map_err(|_|"live network authorization denied; inspect durable approval/audit state".into())
    }
}
impl NetworkAdmission for Admission {
    fn revalidate(&self) -> std::result::Result<(), String> {
        self.runtime
            .block_on(async {
                self.security.ensure_network_run_active(&self.run).await?;
                self.security
                    .validate_ticket(&self.run, &self.prepared, &self.ticket, (self.clock)())
                    .await
                    .map(|_| ())
            })
            .map_err(|_| {
                "network authorization or execution state changed during DNS resolution".into()
            })
    }
}
impl NetworkAuthorizer for EgressGate {
    fn rejected(&self, domain: &str, port: u16, reason: &str) -> std::result::Result<(), String> {
        self.runtime.block_on(self.security.audit(json!({"kind":"network_connection_denied","run":self.run,"domain":domain,"port":port,"reason":reason,"at":(self.clock)()}))).map_err(|_|"network denial audit unavailable".into())
    }
    fn authorize(&self, domain: &str, port: u16) -> std::result::Result<(), String> {
        self.admit(domain, port).map(|_| ())
    }
    fn authorize_connection(
        &self,
        domain: &str,
        port: u16,
    ) -> std::result::Result<Box<dyn NetworkAdmission>, String> {
        Ok(Box::new(self.admit(domain, port)?))
    }
}
