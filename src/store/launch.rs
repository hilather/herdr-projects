//! Atomic launch acknowledgment. External commands and receipt verification run
//! in the trusted adapter before entering this transaction.
use super::*;
use crate::operations::{Claim, Delivery, DeliveryState, Outcome};

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn encode(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| invalid("launch receipt encoding failed"))
}
fn event(
    db: &Connection,
    kind: &str,
    entity: &str,
    revision: u64,
    value: &impl serde::Serialize,
) -> Result<()> {
    db.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",
        params![kind, entity, integer(revision)?, encode(value)?],
    )?;
    Ok(())
}

/// A generic outcome can only reference the exact receipt installed by the
/// sealed lifecycle transaction. No independently committed claimed window exists.
pub(super) fn has_started_receipt(
    db: &Connection,
    operation: &OperationId,
    payload: &str,
) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.launch_started' AND entity=?1 AND payload=?2)",
        params![operation.as_str(),payload], |row| row.get(0))?)
}

pub(super) fn record_creation(tx: &Connection, claim: &Claim, prepared: &PreparedLaunchCreation, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
    let intent = &prepared.intent;
    intent.route.validate().map_err(StoreError::Invalid)?;
    if !matches!(intent.version, 1 | 2)
        || (intent.version == 2
            && (!intent.route.workspace_id.is_empty() || intent.workspace_token.is_some()))
        || intent.workspace_token.as_ref().is_some_and(|token| {
            token.len() != 64
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || !intent.route.workspace_id.is_empty()
        })
        || intent.operation != claim.operation
        || intent.session.born_nanos >= 1_000_000_000
        || intent.command_digest.len() != 64
        || !intent
            .command_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid launch creation intent"));
    }
    let delivery = super::delivery::delivery_with_budget(tx, &claim.operation, budget)?;
    if delivery.revision != claim.revision
        || delivery.state != DeliveryState::Claimed
        || delivery.epoch != claim.epoch
        || delivery.owner.as_deref() != Some(&claim.owner)
        || delivery.lease_until_ms != Some(claim.lease_until_ms)
        || now >= claim.lease_until_ms
    {
        return Err(StoreError::Conflict);
    }
    super::approvals::validate_use_with_budget(tx, claim, now, budget)?;
    let record = super::reservations::read_input(tx, &intent.operation, budget)?;
    if record.attempt != intent.attempt { return Err(StoreError::Conflict); }
    let binding = super::runtime::read_binding(tx, &record.inputs.binding, budget)?.ok_or(StoreError::Conflict)?;
    let (route,_) = super::worktrees::execution_route_with_budget(tx, &record, &binding, budget)?;
    if route != intent.route
        || binding.revision != record.inputs.binding_revision
        || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
    {
        return Err(StoreError::Conflict);
    }
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind IN ('runtime.launch_creation','runtime.launch_target','runtime.launch_started'))",
        [intent.operation.as_str()], |r| r.get(0))?;
    if exists {
        return Err(invalid(
            "creation already prepared; reconcile without replay",
        ));
    }
    event(
        tx,
        "runtime.launch_creation",
        intent.operation.as_str(),
        claim.revision,
        intent,
    )?;
    head(tx)
}

pub(crate) struct StartSelection {
    pub delivery: Delivery,
    pub record: AttemptInputRecord,
    pub events: Vec<Event>,
}
impl SqliteStore {
    pub(super) fn start_selection(&mut self, operation: &OperationId, expected: u64, budget: &read_budget::ReadBudget) -> Result<StartSelection> {
        budget.check()?;
        let tx = self.connection.transaction()?;
        let delivery = super::delivery::delivery_with_budget(&tx, operation, Some(budget))?;
        if delivery.revision != expected { return Err(StoreError::Conflict); }
        let record = super::reservations::read_input(&tx, operation, Some(budget))?;
        let mut statement = tx.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE entity=?1 AND kind IN ('runtime.launch_target','runtime.launch_release','runtime.launch_started','runtime.worktrees_creation','runtime.worktrees_ready') LIMIT 6")?;
        let mut rows = statement.query([operation.as_str()])?;
        let mut events = Vec::new();
        let mut kinds = std::collections::BTreeSet::new();
        while let Some(row) = rows.next()? {
            budget.row(row, &[(5,1)])?;
            let kind: String = row.get(1)?;
            if !kinds.insert(kind.clone()) { return Err(invalid("duplicate start evidence")); }
            let payload: String = row.get(5)?;
            events.push(Event { sequence: row.get(0)?, kind, entity: row.get(2)?, revision: row.get(3)?, payload_version: row.get(4)?,
                payload: serde_json::from_str(&payload).map_err(|_| invalid("invalid start evidence"))? });
        }
        budget.check()?;
        Ok(StartSelection { delivery, record, events })
    }
}

pub(crate) struct AdvancementSelection {
    pub delivery: Delivery,
    pub record: AttemptInputRecord,
    pub kinds: std::collections::BTreeSet<String>,
}
impl SqliteStore {
    pub(super) fn advancement_selection(&mut self, operation: &OperationId, expected: u64, budget: &read_budget::ReadBudget) -> Result<AdvancementSelection> {
        budget.check()?;
        let tx = self.connection.transaction()?;
        let delivery = super::delivery::delivery_with_budget(&tx, operation, Some(budget))?;
        if delivery.revision != expected { return Err(StoreError::Conflict); }
        let record = super::reservations::read_input(&tx, operation, Some(budget))?;
        // Probe each one-use boundary separately. Retained history and duplicate
        // audit rows must not turn a presence check into a full event scan.
        let mut statement = tx.prepare("WITH boundaries(kind) AS (VALUES ('runtime.worktrees_creation'),('runtime.launch_creation'),('runtime.launch_workspace'),('runtime.launch_layout'),('runtime.launch_target'),('runtime.launch_started'),('runtime.launch_release'),('runtime.launch_name')) SELECT kind FROM boundaries WHERE EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind=boundaries.kind)")?;
        let mut rows = statement.query([operation.as_str()])?;
        let mut kinds = std::collections::BTreeSet::new();
        while let Some(row) = rows.next()? {
            budget.row(row, &[])?;
            kinds.insert(row.get(0)?);
        }
        budget.check()?;
        Ok(AdvancementSelection { delivery, record, kinds })
    }
}

pub(crate) struct ResourceRecoverySelection {
    pub head: u64,
    pub delivery: Delivery,
    pub record: AttemptInputRecord,
    pub binding: RuntimeBinding,
    pub route: RuntimeRoute,
    pub events: Vec<Event>,
}
impl SqliteStore {
    pub(super) fn resource_recovery_selection(&mut self, operation: &OperationId, expected: u64,
        budget: &read_budget::ReadBudget) -> Result<ResourceRecoverySelection> {
        self.resource_selection(operation, expected, budget, false)
    }
    pub(super) fn resource_creation_selection(&mut self, operation: &OperationId, expected: u64,
        budget: &read_budget::ReadBudget) -> Result<ResourceRecoverySelection> {
        self.resource_selection(operation, expected, budget, true)
    }
    fn resource_selection(&mut self, operation: &OperationId, expected: u64,
        budget: &read_budget::ReadBudget, creating: bool) -> Result<ResourceRecoverySelection> {
        budget.check()?;
        let tx = self.connection.transaction()?;
        let head = head(&tx)?;
        let delivery = super::delivery::delivery_with_budget(&tx, operation, Some(budget))?;
        if delivery.revision != expected || !(delivery.attempts == 1 && delivery.state != DeliveryState::Confirmed
            || creating && delivery.state == DeliveryState::Pending && delivery.attempts == 0 && delivery.epoch == 0) {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_input(&tx, operation, Some(budget))?;
        let attempt = read_attempt_with_budget(&tx, &record.attempt, Some(budget))?;
        if attempt.state != AttemptState::Reserved || !attempt.retains_capacity() { return Err(StoreError::Conflict); }
        let binding = super::runtime::read_binding(&tx, &record.inputs.binding, Some(budget))?.ok_or(StoreError::Conflict)?;
        if delivery.attempts == 1 { super::approvals::validate_historical_consumption(&tx, &record, Some(budget))?; }
        let route = super::worktrees::execution_route_with_budget(&tx, &record, &binding, Some(budget))?.0;
        let mut statement = tx.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE entity=?1 AND kind IN ('runtime.launch_creation','runtime.launch_target','runtime.launch_workspace','runtime.launch_layout','runtime.launch_release','runtime.launch_started','runtime.worktrees_creation','runtime.worktrees_ready') ORDER BY sequence")?;
        let mut rows = statement.query([operation.as_str()])?;
        let mut events = Vec::new();
        let mut kinds = std::collections::BTreeSet::new();
        while let Some(row) = rows.next()? {
            budget.row(row, &[(5,1)])?;
            let kind: String = row.get(1)?;
            if !kinds.insert(kind.clone()) { return Err(invalid("duplicate resource recovery evidence")); }
            let payload: String = row.get(5)?;
            events.push(Event { sequence: row.get(0)?, kind, entity: row.get(2)?, revision: row.get(3)?,
                payload_version: row.get(4)?, payload: serde_json::from_str(&payload).map_err(|_| invalid("invalid resource recovery evidence"))? });
        }
        budget.check()?;
        Ok(ResourceRecoverySelection { head, delivery, record, binding, route, events })
    }
}

impl SqliteStore {
    /// Atomically consume approval, claim the launch and retain its recovery
    /// identity. A failure leaves the operation unclaimed and approval unused.
    #[cfg(test)]
    pub(crate) fn claim_launch_creation(
        &mut self, expected: u64, prepared: &PreparedLaunchCreation, now: i64, lease_ms: i64,
    ) -> Result<Claim> {
        self.claim_with_creation(&prepared.intent.operation, expected, "canonical-resource-adapter", now, lease_ms, Some(super::delivery::LaunchPreparation::Native(prepared)))
    }

    /// Advance only the existing worktree claim; never consume approval twice.
    pub(crate) fn continue_worktree_launch_creation_with_budget(&mut self,claim:&Claim,prepared:&PreparedLaunchCreation,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<()> {
        self.validate_claim_with_budget(claim,now,budget)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if claim.owner!="canonical-worktree-adapter" {return Err(StoreError::Conflict);}
        let ready:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.worktrees_ready' AND entity=?1)",[claim.operation.as_str()],|r|r.get(0))?;
        if !ready {return Err(invalid("worktree launch has no complete receipt"));}
        record_creation(&tx,claim,prepared,now,budget)?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;Ok(())
    }

    pub fn record_launch_workspace(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchWorkspace,
        now: i64,
    ) -> Result<u64> {
        self.record_workspace_boundary(
            Some(claim),
            None,
            claim.revision,
            &prepared.target,
            false,
            now,
            None,
        )
    }

    pub fn record_launch_layout(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchLayout,
        now: i64,
    ) -> Result<u64> {
        self.record_workspace_boundary(
            Some(claim),
            None,
            claim.revision,
            &prepared.workspace,
            true,
            now,
            None,
        )
    }

    /// Retain an exact observed bootstrap resource without renewing launch authority.
    pub fn observe_launch_workspace(
        &mut self,
        prepared: &PreparedLaunchWorkspace,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<u64> {
        self.record_workspace_boundary(
            None,
            Some(expected_head),
            expected_revision,
            &prepared.target,
            false,
            now,
            None,
        )
    }

    pub(crate) fn observe_launch_workspace_with_budget(&mut self, prepared: &PreparedLaunchWorkspace,
        revision: u64, head: u64, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
        self.record_workspace_boundary(None, Some(head), revision, &prepared.target, false, now, budget)
    }

    pub(crate) fn record_launch_workspace_with_budget(&mut self, claim: &Claim, prepared: &PreparedLaunchWorkspace, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
        self.record_workspace_boundary(Some(claim), None, claim.revision, &prepared.target, false, now, budget)
    }
    pub(crate) fn record_launch_layout_with_budget(&mut self, claim: &Claim, prepared: &PreparedLaunchLayout, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
        self.record_workspace_boundary(Some(claim), None, claim.revision, &prepared.workspace, true, now, budget)
    }

    fn record_workspace_boundary(
        &mut self,
        claim: Option<&Claim>,
        expected_head: Option<u64>,
        expected_revision: u64,
        target: &LaunchTarget,
        layout: bool,
        now: i64,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        if let Some(budget) = budget { budget.check()?; }
        super::delivery::now_check(now)?;
        if let Some(claim) = claim {
            self.validate_claim_with_budget(claim, now, budget)?;
        }
        target.route.validate().map_err(StoreError::Invalid)?;
        if target.version != 1
            || target.supervisor.is_some()
            || claim.is_some_and(|claim| target.operation != claim.operation)
            || target.route.workspace_id.is_empty()
            || target.route.tab_id.is_empty()
            || target.route.pane_id.is_empty()
            || target.terminal.is_empty()
            || target.terminal.len() > 512
            || target.terminal.chars().any(char::is_control)
            || target.observed_unix_ms < 0
            || target.observed_unix_ms > now
        {
            return Err(invalid("invalid workspace creation receipt"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if expected_head.is_some_and(|h| head(&tx).ok() != Some(h)) {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_input(&tx, &target.operation, budget)?;
        if record.attempt != target.attempt { return Err(StoreError::Conflict); }
        let delivery = super::delivery::delivery_with_budget(&tx, &target.operation, budget)?;
        if delivery.revision != expected_revision || delivery.attempts != 1 {
            return Err(StoreError::Conflict);
        }
        if let Some(claim) = claim {
            if delivery.state != DeliveryState::Claimed
                || delivery.epoch != claim.epoch
                || delivery.owner.as_deref() != Some(&claim.owner)
                || delivery.lease_until_ms != Some(claim.lease_until_ms)
                || now >= claim.lease_until_ms
            {
                return Err(StoreError::Conflict);
            }
            super::approvals::validate_use_with_budget(&tx, claim, now, budget)?;
        } else {
            super::approvals::validate_historical_consumption(&tx, &record, budget)?;
            if layout
                || !matches!(
                    delivery.state,
                    DeliveryState::Claimed | DeliveryState::Ambiguous
                )

            {
                return Err(StoreError::Conflict);
            }
        }
        let payload: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_creation' AND entity=?1",
            [target.operation.as_str()], budget, &[(0,1)],
            |r| r.get(0),
        )?;
        let intent: LaunchCreationIntent =
            serde_json::from_str(&payload).map_err(|_| invalid("invalid creation intent"))?;
        let binding = super::runtime::read_binding(&tx, &record.inputs.binding, budget)?.ok_or(StoreError::Conflict)?;
        if intent.version != 1
            || intent.operation != target.operation
            || intent.attempt != target.attempt
            || !intent.route.workspace_id.is_empty()
            || !intent.route.tab_id.is_empty()
            || !intent.route.pane_id.is_empty()
            || !intent.route.machine.is_empty()
            || intent.session != target.session
            || intent.route.socket != target.route.socket
            || intent.route.cwd != target.route.cwd
            || !target.route.machine.is_empty()
            || intent.route != RuntimeRoute::from_identity(&binding.identity)
            || binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
        {
            return Err(StoreError::Conflict);
        }
        let attempt = read_attempt_with_budget(&tx, &target.attempt, budget)?;
        if attempt.state != AttemptState::Reserved || !attempt.retains_capacity() { return Err(StoreError::Conflict); }
        if claim.is_none() && intent.workspace_token.is_none() {
            return Err(invalid("workspace creation lacks recovery marker"));
        }
        let kind = if layout {
            "runtime.launch_layout"
        } else {
            "runtime.launch_workspace"
        };
        let used:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND (kind=?2 OR kind IN ('runtime.launch_layout','runtime.launch_target','runtime.launch_started')))",params![target.operation.as_str(),kind],|r|r.get(0))?;
        if used {
            return Err(invalid(
                "workspace boundary already attempted; reconcile without replay",
            ));
        }
        if layout {
            let payload: String = read_budget::one(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_workspace' AND entity=?1",
                [target.operation.as_str()], budget, &[(0,1)],
                |r| r.get(0),
            )?;
            if serde_json::from_str::<LaunchTarget>(&payload)
                .map_err(|_| invalid("invalid workspace receipt"))?
                != *target
            {
                return Err(StoreError::Conflict);
            }
        }
        event(
            &tx,
            kind,
            target.operation.as_str(),
            expected_revision,
            target,
        )?;
        let result = head(&tx)?;
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(result)
    }

    /// Persist the selected incarnation before the external start. One operation
    /// can never replace its target, including after uncertainty or restart.
    pub fn record_launch_target(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchTarget,
        now: i64,
    ) -> Result<u64> {
        self.apply_launch_target(Some(claim), None, claim.revision, prepared, now, None)
    }

    /// Observation cannot renew a lease or authorize creation/start. A durable
    /// creation intent and its original consumed claim must already exist.
    pub fn observe_launch_target(
        &mut self,
        prepared: &PreparedLaunchTarget,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<u64> {
        self.observe_launch_target_with_budget(prepared, expected_revision, expected_head, now, None)
    }

    pub(crate) fn observe_launch_target_with_budget(
        &mut self, prepared: &PreparedLaunchTarget, expected_revision: u64,
        expected_head: u64, now: i64, budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        self.apply_launch_target(None, Some(expected_head), expected_revision, prepared, now, budget)
    }

    fn apply_launch_target(
        &mut self,
        claim: Option<&Claim>,
        expected_head: Option<u64>,
        expected_revision: u64,
        prepared: &PreparedLaunchTarget,
        now: i64,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        if let Some(budget) = budget { budget.check()?; }
        super::delivery::now_check(now)?;
        let target = &prepared.target;
        target.route.validate().map_err(StoreError::Invalid)?;
        if !matches!(
            (target.version, target.supervisor.is_some()),
            (1, false) | (2, true)
        ) || claim.is_some_and(|c| target.operation != c.operation)
            || target.observed_unix_ms < 0
            || target.observed_unix_ms > now
            || now - target.observed_unix_ms > 30_000
            || target.session.born_nanos >= 1_000_000_000
            || !target.route.machine.is_empty()
            || target.route.pane_id.is_empty()
            || target.terminal.is_empty()
            || target.terminal.len() > 512
            || target.terminal.chars().any(char::is_control)
        {
            return Err(invalid("invalid launch target observation"));
        }
        if let Some(identity) = &target.supervisor {
            identity
                .validate()
                .map_err(|_| invalid("invalid staged supervisor identity"))?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if expected_head.is_some_and(|h| head(&tx).ok() != Some(h)) {
            return Err(StoreError::Conflict);
        }
        let old = super::delivery::delivery_with_budget(&tx, &target.operation, budget)?;
        if old.revision != expected_revision {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_input(&tx, &target.operation, budget)?;
        if record.attempt != target.attempt { return Err(StoreError::Conflict); }
        if let Some(claim) = claim {
            if old.state != DeliveryState::Claimed
                || old.epoch != claim.epoch
                || old.owner.as_deref() != Some(claim.owner.as_str())
                || old.lease_until_ms != Some(claim.lease_until_ms)
                || now >= claim.lease_until_ms
            {
                return Err(StoreError::Conflict);
            }
            super::approvals::validate_use(&tx, claim, now)?;
        } else {
            super::approvals::validate_historical_consumption(&tx, &record, budget)?;
            if old.attempts != 1 || old.state == DeliveryState::Confirmed || target.version != 2 {
                return Err(StoreError::Conflict);
            }
            let payload: String = read_budget::one(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_creation' AND entity=?1",
                [target.operation.as_str()], budget, &[(0,1)],
                |r| r.get(0),
            )?;
            let intent: LaunchCreationIntent =
                serde_json::from_str(&payload).map_err(|_| invalid("invalid creation intent"))?;
            if !matches!(intent.version, 1 | 2)
                || (intent.version == 2
                    && (!intent.route.workspace_id.is_empty() || intent.workspace_token.is_some()))
                || intent.operation != target.operation
                || intent.attempt != target.attempt
                || intent.session != target.session
                || intent.route.socket != target.route.socket
                || (!intent.route.workspace_id.is_empty()
                    && intent.route.workspace_id != target.route.workspace_id)
                || intent.route.cwd != target.route.cwd
            {
                return Err(StoreError::Conflict);
            }
        }
        let binding = super::runtime::read_binding(&tx, &record.inputs.binding, budget)?
            .ok_or(StoreError::Conflict)?;
        let attempt = read_attempt_with_budget(&tx, &target.attempt, budget)?;
        if binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
            || attempt.state != AttemptState::Reserved || !attempt.retains_capacity()
        { return Err(StoreError::Conflict); }
        let (execution_route,_) = super::worktrees::execution_route_with_budget(&tx,&record,&binding,budget)?;
        if !binding.identity.worktree_path.is_empty()
            || !binding.identity.machine.is_empty()
            || target.route.socket != binding.identity.socket
            || target.route.cwd != execution_route.cwd
            || (!binding.identity.workspace_id.is_empty()
                && target.route.workspace_id != binding.identity.workspace_id)
            || (!binding.identity.tab_id.is_empty()
                && target.route.tab_id != binding.identity.tab_id)
        {
            return Err(invalid(
                "launch target differs from prepared local execution",
            ));
        }
        let direct_root = if binding.identity.workspace_id.is_empty() && target.version == 2 {
            let payload: String = read_budget::one(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_creation' AND entity=?1",
                [target.operation.as_str()], budget, &[(0,1)],
                |r| r.get(0),
            )?;
            let creation: LaunchCreationIntent = serde_json::from_str(&payload)
                .map_err(|_| invalid("invalid retained creation intent"))?;
            if creation.version == 2 {
                if creation.operation != target.operation
                    || creation.attempt != target.attempt
                    || creation.session != target.session
                    || creation.route != execution_route
                    || creation.workspace_token.is_some()
                    || target.route.workspace_id.is_empty()
                    || target.route.tab_id.is_empty()
                {
                    return Err(invalid("direct root differs from prepared creation"));
                }
                let legacy: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind IN ('runtime.launch_layout','runtime.launch_workspace'))",
                    [target.operation.as_str()], |r| r.get(0),
                )?;
                // Existing exact targets are handled idempotently below. A legacy
                // workspace receipt must never authorize a direct-root conversion.
                if legacy {
                    let same: bool = tx.query_row(
                        "SELECT (SELECT count(*) FROM events WHERE entity=?1 AND kind IN ('runtime.launch_workspace','runtime.launch_target') AND payload=?2)=2 AND NOT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind='runtime.launch_layout')",
                        params![target.operation.as_str(), encode(target)?], |r| r.get(0),
                    )?;
                    if !same {
                        return Err(invalid("direct workspace ownership changed"));
                    }
                }
                true
            } else if creation.version == 1 {
                let payload: String = read_budget::one(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_workspace' AND entity=?1",
                [target.operation.as_str()], budget, &[(0,1)],
                |r| r.get(0),
            )?;
                let workspace: LaunchTarget = serde_json::from_str(&payload)
                    .map_err(|_| invalid("invalid retained workspace"))?;
                let submitted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.launch_layout' AND entity=?1 AND payload=?2)",params![target.operation.as_str(),payload],|r|r.get(0))?;
                if !submitted
                    || workspace.attempt != target.attempt
                    || workspace.session != target.session
                    || workspace.route.workspace_id != target.route.workspace_id
                    || workspace.route.pane_id == target.route.pane_id
                {
                    return Err(invalid(
                        "target does not belong to the created worker workspace",
                    ));
                }
                false
            } else {
                return Err(invalid("unsupported creation intent"));
            }
        } else {
            false
        };
        let binding_conflict: bool = read_budget::one(&tx,
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id<>?1 AND json_extract(payload,'$.identity.machine')='' AND json_extract(payload,'$.identity.socket')=?2 AND json_extract(payload,'$.identity.pane_id')=?3)",
            params![binding.id, target.route.socket, target.route.pane_id], budget, &[], |r| r.get(0))?;
        if binding_conflict { return Err(invalid("launch target belongs to another runtime binding")); }
        let staged_conflict:bool=read_budget::one(&tx,"SELECT EXISTS(SELECT 1 FROM events e JOIN attempts a ON a.id=json_extract(e.payload,'$.attempt') WHERE e.kind='runtime.launch_target' AND e.entity<>?1 AND a.termination_observed=0 AND json_extract(e.payload,'$.route.socket')=?2 AND json_extract(e.payload,'$.route.pane_id')=?3)",params![target.operation.as_str(),target.route.socket,target.route.pane_id],budget,&[],|r|r.get(0))?;
        if staged_conflict {
            return Err(invalid("launch target belongs to another retained launch"));
        }
        let payload = encode(target)?;
        let prior: Option<String> = read_budget::optional(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_target' AND entity=?1",
            [target.operation.as_str()], budget, &[], |r| r.get(0))?;
        if let Some(prior) = prior {
            if prior != payload {
                return Err(invalid("launch target is immutable after selection"));
            }
            return head(&tx);
        }
        if direct_root {
            event(
                &tx,
                "runtime.launch_workspace",
                target.operation.as_str(),
                old.revision,
                target,
            )?;
        }
        event(
            &tx,
            "runtime.launch_target",
            target.operation.as_str(),
            old.revision,
            target,
        )?;
        let result = head(&tx)?;
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(result)
    }

    /// Consume the sole gate-input opportunity. A failed or lost native reply
    /// must be reconciled by observation; even an identical replay is rejected.
    pub fn record_launch_release(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchRelease,
        now: i64,
    ) -> Result<u64> {
        self.record_launch_boundary(claim, &prepared.intent, now, false, None)
    }

    pub(crate) fn record_launch_name_with_budget(&mut self, claim: &Claim, prepared: &PreparedLaunchName, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
        self.record_launch_boundary(claim, &prepared.intent, now, true, budget)
    }

    pub fn record_launch_name(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchName,
        now: i64,
    ) -> Result<u64> {
        self.record_launch_boundary(claim, &prepared.intent, now, true, None)
    }

    pub(crate) fn record_launch_release_with_budget(&mut self, claim: &Claim, prepared: &PreparedLaunchRelease, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<u64> {
        self.record_launch_boundary(claim, &prepared.intent, now, false, budget)
    }

    fn record_launch_boundary(
        &mut self,
        claim: &Claim,
        intent: &LaunchReleaseIntent,
        now: i64,
        naming: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        self.validate_claim_with_budget(claim, now, budget)?;
        if intent.version != 1
            || intent.target.version != 2
            || intent.target.operation != claim.operation
            || intent.observed_unix_ms < intent.target.observed_unix_ms
            || intent.observed_unix_ms > now
            || now - intent.observed_unix_ms > 30_000
        {
            return Err(invalid("invalid gate release observation"));
        }
        intent
            .target
            .supervisor
            .as_ref()
            .ok_or(StoreError::Conflict)?
            .validate()
            .map_err(|_| invalid("invalid gate supervisor identity"))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let delivery = super::delivery::delivery_with_budget(&tx, &claim.operation, budget)?;
        if delivery.revision != claim.revision
            || delivery.state != DeliveryState::Claimed
            || delivery.epoch != claim.epoch
            || delivery.owner.as_deref() != Some(claim.owner.as_str())
            || delivery.lease_until_ms != Some(claim.lease_until_ms)
            || now >= claim.lease_until_ms
        {
            return Err(StoreError::Conflict);
        }
        super::approvals::validate_use_with_budget(&tx, claim, now, budget)?;
        let record = super::reservations::read_input(&tx, &claim.operation, budget)?;
        if record.attempt != intent.target.attempt { return Err(StoreError::Conflict); }
        let binding = super::runtime::read_binding(&tx, &record.inputs.binding, budget)?.ok_or(StoreError::Conflict)?;
        let attempt = read_attempt_with_budget(&tx, &record.attempt, budget)?;
        let task = read_task_with_budget(&tx, record.inputs.task.as_str(), budget)?;
        if binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
            || super::ownership::read_binding(&tx, &binding.id, budget)?.is_some()
            || attempt.state != AttemptState::Reserved || !attempt.retains_capacity()
            || task.active_attempt.as_ref() != Some(&record.attempt) { return Err(StoreError::Conflict); }
        let payload: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_target' AND entity=?1",
            [claim.operation.as_str()], budget, &[(0,1)],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<LaunchTarget>(&payload)
            .map_err(|_| invalid("invalid retained gate target"))?
            != intent.target
        {
            return Err(StoreError::Conflict);
        }
        let creation: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_creation' AND entity=?1",
            [claim.operation.as_str()], budget, &[(0,1)],
            |r| r.get(0),
        )?;
        let creation: LaunchCreationIntent = serde_json::from_str(&creation)
            .map_err(|_| invalid("invalid retained creation intent"))?;
        let (execution_route,_) = super::worktrees::execution_route_with_budget(&tx,&record,&binding,budget)?;
        if !matches!(creation.version, 1 | 2)
            || (creation.version == 2
                && (!creation.route.workspace_id.is_empty() || creation.workspace_token.is_some()))
            || creation.operation != claim.operation
            || creation.attempt != record.attempt
            || creation.session != intent.target.session
            || creation.route != execution_route
        {
            return Err(StoreError::Conflict);
        }
        let kind = if naming {
            "runtime.launch_name"
        } else {
            "runtime.launch_release"
        };
        if naming {
            let payload: String = read_budget::one(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_release' AND entity=?1",
                [claim.operation.as_str()], budget, &[(0,1)],
                |r| r.get(0),
            )?;
            let release: LaunchReleaseIntent = serde_json::from_str(&payload)
                .map_err(|_| invalid("invalid retained release intent"))?;
            if release.version != 1
                || release.target != intent.target
                || release.observed_unix_ms > intent.observed_unix_ms
            {
                return Err(StoreError::Conflict);
            }
        }
        let used: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND (kind=?2 OR kind='runtime.launch_started'))",
            rusqlite::params![claim.operation.as_str(), kind], |r| r.get(0))?;
        if used {
            return Err(invalid(
                "launch boundary already attempted; observation required",
            ));
        }
        event(&tx, kind, claim.operation.as_str(), claim.revision, intent)?;
        let result = head(&tx)?;
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(result)
    }

    /// Record submission, retain capacity, and publish ownership together. This
    /// does not claim readiness, prompt delivery, task success, or termination.
    pub fn record_launch_started(
        &mut self,
        claim: &Claim,
        prepared: &PreparedLaunchStarted,
        now: i64,
    ) -> Result<Delivery> {
        self.apply_launch_started(Some(claim), None, claim.revision, prepared, now, None)
    }

    /// Reconcile a freshly observed exact worker after a lost launch response.
    /// This never calls an external start and need not renew expired approval.
    pub fn observe_launch_started(
        &mut self,
        prepared: &PreparedLaunchStarted,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<Delivery> {
        self.observe_launch_started_with_budget(prepared, expected_revision, expected_head, now, None)
    }

    pub(crate) fn observe_launch_started_with_budget(&mut self, prepared: &PreparedLaunchStarted, revision: u64, head: u64, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<Delivery> {
        self.apply_launch_started(None, Some(head), revision, prepared, now, budget)
    }

    fn apply_launch_started(
        &mut self,
        claim: Option<&Claim>,
        expected_head: Option<u64>,
        expected_revision: u64,
        prepared: &PreparedLaunchStarted,
        now: i64,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<Delivery> {
        if let Some(budget) = budget { budget.check()?; }
        super::delivery::now_check(now)?;
        let receipt = &prepared.receipt;
        receipt.route.validate().map_err(StoreError::Invalid)?;
        if !matches!(
            (receipt.version, receipt.supervisor.is_some()),
            (1, false) | (2, true)
        ) || claim.is_some_and(|claim| receipt.operation != claim.operation)
            || receipt.observed_unix_ms < 0
            || receipt.observed_unix_ms > now
            || now - receipt.observed_unix_ms > 30_000
            || receipt.session.born_nanos >= 1_000_000_000
            || receipt.terminal.is_empty()
            || receipt.terminal.len() > 512
            || receipt.terminal.chars().any(char::is_control)
            || receipt.agent.name.is_empty()
            || receipt.agent.name.len() > 512
            || receipt.agent.name.chars().any(char::is_control)
            || receipt.route.pane_id.is_empty()
            || !receipt.route.machine.is_empty()
        {
            return Err(invalid("invalid local launch acknowledgment"));
        }
        if let Some(identity) = &receipt.supervisor {
            identity
                .validate()
                .map_err(|_| invalid("invalid supervisor incarnation"))?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if let Some(expected) = expected_head {
            if head(&tx)? != expected {
                return Err(StoreError::Conflict);
            }
        }
        let old = super::delivery::delivery_with_budget(&tx, &receipt.operation, budget)?;
        // Exact receipt replay is read-only and cannot resurrect a later state.
        let payload = encode(receipt)?;
        if old.state == DeliveryState::Confirmed
            && old.last_outcome
                == Some(Outcome::Confirmed {
                    observed_identity: payload.clone(),
                })
            && has_started_receipt(&tx, &receipt.operation, &payload)?
        {
            return Ok(old);
        }
        if old.revision != expected_revision {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_input(&tx, &receipt.operation, budget)?;
        if record.attempt != receipt.attempt { return Err(StoreError::Conflict); }
        if let Some(claim) = claim {
            if old.state != DeliveryState::Claimed
                || old.epoch != claim.epoch
                || old.owner.as_deref() != Some(claim.owner.as_str())
                || old.lease_until_ms != Some(claim.lease_until_ms)
                || now >= claim.lease_until_ms
            {
                return Err(StoreError::Conflict);
            }
            super::approvals::validate_use_with_budget(&tx, claim, now, budget)?;
        } else {
            let released: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.launch_release' AND entity=?1)",
                [receipt.operation.as_str()], |r|r.get(0))?;
            if old.attempts != 1
                || !(old.state == DeliveryState::Ambiguous
                    || (old.state == DeliveryState::Claimed && receipt.version == 2 && released))
            {
                return Err(StoreError::Conflict);
            }
            // Read/validate the durable use, without pretending observation is a
            // new approved action. Expired or revoked authority must not erase a
            // worker that already exists.
            super::approvals::validate_historical_consumption(&tx, &record, budget)?;
        }
        let profile = record
            .inputs
            .effective_profile
            .as_ref()
            .ok_or(StoreError::Conflict)?;
        let target: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_target' AND entity=?1",
            [receipt.operation.as_str()], budget, &[(0,1)],
            |r| r.get(0),
        )?;
        let target: LaunchTarget = serde_json::from_str(&target)
            .map_err(|_| StoreError::Corrupt("invalid retained launch target".into()))?;
        if !matches!(
            (target.version, target.supervisor.is_some()),
            (1, false) | (2, true)
        ) || target.attempt != receipt.attempt
            || target.operation != receipt.operation
            || target.route != receipt.route
            || target.session != receipt.session
            || target.terminal != receipt.terminal
            || (target.version == 2
                && (receipt.version != 2 || target.supervisor != receipt.supervisor))
            || target.observed_unix_ms > receipt.observed_unix_ms
        {
            return Err(invalid(
                "launch acknowledgment differs from the selected target incarnation",
            ));
        }
        if target.version == 2 {
            let released: Option<String> = read_budget::optional(&tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_release' AND entity=?1",
                [receipt.operation.as_str()], budget, &[(0,1)], |r| r.get(0))?;
            let release: LaunchReleaseIntent = serde_json::from_str(
                &released.ok_or_else(|| invalid("gated launch has no durable release intent"))?,
            )
            .map_err(|_| invalid("invalid gate release intent"))?;
            if release.version != 1
                || release.target != target
                || release.observed_unix_ms < target.observed_unix_ms
                || release.observed_unix_ms > receipt.observed_unix_ms
            {
                return Err(invalid("start observation does not match the gate release"));
            }
        }
        let mut binding = super::runtime::read_binding(&tx, &record.inputs.binding, budget)?.ok_or(StoreError::Conflict)?;
        let task = read_task_with_budget(&tx, record.inputs.task.as_str(), budget)?;
        let mut attempt = read_attempt_with_budget(&tx, &record.attempt, budget)?;
        if task.active_attempt.as_ref() != Some(&attempt.id)
            || !attempt.retains_capacity()
            || attempt.state != AttemptState::Reserved
            || binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
            || super::ownership::read_binding(&tx, &binding.id, budget)?.is_some()
        {
            return Err(StoreError::Conflict);
        }
        // The first native adapter is local and does not create repository
        // worktrees. Such inputs must use a separate resource-creation service.
        let (execution_route,worktree) = super::worktrees::execution_route_with_budget(&tx,&record,&binding,budget)?;
        if !binding.identity.worktree_path.is_empty()
            || !binding.identity.machine.is_empty()
            || receipt.agent.kind != profile.kind
            || receipt.agent.name != worker_agent_name(&record.attempt)
            || receipt.route.socket != binding.identity.socket
            || receipt.route.cwd != execution_route.cwd
            || (!binding.identity.workspace_id.is_empty()
                && receipt.route.workspace_id != binding.identity.workspace_id)
            || (!binding.identity.tab_id.is_empty()
                && receipt.route.tab_id != binding.identity.tab_id)
        {
            return Err(invalid(
                "launch acknowledgment differs from prepared local execution",
            ));
        }
        let conflict: bool = read_budget::one(&tx,
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id<>?1 AND json_extract(payload,'$.identity.machine')='' AND json_extract(payload,'$.identity.socket')=?2 AND json_extract(payload,'$.identity.pane_id')=?3)",
            params![binding.id, receipt.route.socket, receipt.route.pane_id], budget, &[], |r| r.get(0))?;
        if conflict { return Err(invalid("launch pane belongs to another runtime binding")); }
        binding.revision = binding
            .revision
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        binding.identity.workspace_id = receipt.route.workspace_id.clone();
        binding.identity.tab_id = receipt.route.tab_id.clone();
        binding.identity.pane_id = receipt.route.pane_id.clone();
        binding.identity.agent = receipt.agent.kind.clone();
        binding.identity.agent_name = receipt.agent.name.clone();
        binding.identity.cwd = receipt.route.cwd.clone();
        binding.identity.thread_dir = worker_output_path(&record.inputs,&record.attempt).map_err(StoreError::Invalid)?;
        if let Some(tree)=&worktree {
            binding.identity.repo=tree.plan.source.repository.clone();
            binding.identity.branch=tree.plan.branch.clone();
            binding.identity.worktree_path=tree.plan.path.clone();
        }
        let binding_payload = encode(&binding)?;
        tx.execute(
            "UPDATE runtime_bindings SET revision=?2,payload=?3,payload_hash=?4 WHERE id=?1",
            params![
                binding.id,
                integer(binding.revision)?,
                binding_payload,
                format!("{:x}", Sha256::digest(binding_payload.as_bytes()))
            ],
        )?;
        let previous:u64=read_budget::one(&tx,"SELECT COALESCE(MAX(revision),0) FROM events WHERE kind IN ('runtime.adopted','runtime.launched') AND entity=?1",[&binding.id],budget,&[],|r|r.get(0))?;
        let owned = RuntimeOwnership {
            binding: binding.id.clone(),
            revision: previous.checked_add(1).ok_or(StoreError::Conflict)?,
            binding_revision: binding.revision,
            identity_digest: super::ownership::identity_digest(&binding)?,
            origin: "launched".into(),
            attempt: Some(receipt.attempt.clone()),
            session: Some(receipt.session.clone()),
            worktree: worktree.as_ref().map(|tree|tree.directory.clone()),
            agent: Some(receipt.agent.clone()),
            config_digest: record.inputs.config.digest.clone(),
            observed_unix_ms: receipt.observed_unix_ms,
        };
        let owned_payload = encode(&owned)?;
        tx.execute(
            "INSERT INTO runtime_ownership VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                owned.binding,
                integer(owned.revision)?,
                integer(owned.binding_revision)?,
                receipt.attempt.as_str(),
                owned_payload,
                format!("{:x}", Sha256::digest(owned_payload.as_bytes()))
            ],
        )?;
        super::active_work::invalidate(&tx)?;
        attempt.revision = attempt
            .revision
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        attempt.state = AttemptState::Launching;
        tx.execute(
            "UPDATE attempts SET revision=?2,state='launching' WHERE id=?1",
            params![attempt.id.as_str(), integer(attempt.revision)?],
        )?;
        tx.execute(
            "DELETE FROM runtime_observations WHERE binding_id=?1",
            [&binding.id],
        )?;
        event(&tx, "runtime.launched", &binding.id, owned.revision, &owned)?;
        event(
            &tx,
            "runtime.launch_started",
            receipt.operation.as_str(),
            old.revision,
            receipt,
        )?;
        event(
            &tx,
            "attempt.changed",
            attempt.id.as_str(),
            attempt.revision,
            &attempt,
        )?;
        let result = super::delivery::update_outcome(
            &tx,
            &old,
            &Outcome::Confirmed {
                observed_identity: payload,
            },
            now,
            claim.map(|c| c.owner.as_str()).unwrap_or("launch-recovery"),
        )?;
        super::consumer_bindings::reconcile_task(&tx,record.inputs.task.as_str(),budget)?;
        #[cfg(test)]
        super::reservations::tests::crash_boundary("before_start_receipt_commit");
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(result)
    }
}
