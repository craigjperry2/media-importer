---
name: implement-milestone-plan
description: Implement a supplied milestone implementation plan, then independently audit and remediate the repository in fresh agent contexts until no material gaps remain. Use when the user provides a concrete plan link or path and asks Codex to implement it, verify a previous implementation, close implementation gaps, or run an implement-review-remediate convergence loop. Reject missing, inaccessible, or non-implementation-plan inputs before changing the repository.
---

# Implement Milestone Plan

Run a sequential implementation and independent-audit loop. Keep the parent
thread focused on orchestration, decisions, and the final report. Use a new
agent thread for every implementation and audit pass; never run two writing
agents concurrently.

## Require a valid plan

Treat the user-supplied plan locator as the only required input. Resolve and
read it before editing files or spawning an implementation agent.

Accept the input only when it identifies an accessible implementation plan
with enough concrete scope, intended behavior, and verification guidance to
act without inventing major product or architecture decisions. Plans may use
prose, milestones, checklists, or ordered steps.

Exit immediately with `INVALID_PLAN` and make no repository changes when:

- no plan locator was supplied;
- the artifact cannot be found, opened, or authenticated;
- the artifact is merely an idea, status note, issue title, or requirements
  fragment rather than an actionable implementation plan; or
- core scope is too ambiguous to implement safely.

Explain which condition failed and what the user must supply. Do not search for
a substitute plan or draft one unless the user separately asks for that work.

## Establish the run

1. Read every applicable `AGENTS.md` file and treat it as repository authority.
2. Record the plan locator and a short plan identity for the final report.
3. Start an in-thread deviation ledger. For each deviation record the plan
   reference, chosen action, rationale, affected files or behavior, and
   verification evidence.
4. Preserve user decisions separately and pass them to later agents as binding
   constraints.
5. Use an active goal when the user invoked this workflow through `/goal`.
   Do not create or broaden a goal implicitly.

## Spawn fresh role agents

Spawn the repository-scoped custom agents by name:

- `milestone_implementer` for implementation and remediation;
- `milestone_auditor` for independent verification.

Use a fresh context for every pass (`fork_turns="none"` when the agent tool
supports it). Give each agent the plan locator, applicable user decisions, its
single bounded assignment, and the minimum state required for that pass. Do
not reuse a completed agent with a follow-up task.

The parent must not implement fixes itself. It must wait for each role agent,
route the result, maintain the decision and deviation ledgers, and report
progress. Keep implementation passes sequential so only one agent writes to
the worktree at a time.

## Run the convergence loop

### 1. Implement

For the first pass, ask a fresh `milestone_implementer` to implement the entire
validated plan. For later passes, give a fresh implementer:

- the original plan;
- binding human decisions;
- the latest auditor remediation plan;
- relevant unresolved gaps; and
- the accumulated deviation ledger.

Require the implementer to return changed files, validation run and results,
deviations, unresolved blockers, and any decision request. Do not claim the
plan is complete based only on this response.

### 2. Audit independently

After every implementation pass, ask a fresh `milestone_auditor` to inspect
the repository against the original plan and binding human decisions. Treat
implementation claims and the deviation ledger as untrusted context to verify,
not as evidence of completion.

Require the first line of the audit to be exactly one verdict:

- `VERDICT: CONVERGED`
- `VERDICT: CONVERGED_WITH_MINOR_GAPS`
- `VERDICT: REMEDIATION_REQUIRED`
- `VERDICT: HUMAN_DECISION_REQUIRED`

Require evidence with file references and validation results. When remediation
is required, require an ordered plan whose items include priority, violated
plan requirement, evidence, expected change, and verification.

### 3. Route the verdict

- On `CONVERGED`, stop the loop.
- On `CONVERGED_WITH_MINOR_GAPS`, stop the loop and preserve the minor gaps in
  the final report.
- On `REMEDIATION_REQUIRED`, spawn a fresh implementer with the prioritized
  remediation plan, then audit again.
- On `HUMAN_DECISION_REQUIRED`, pause and ask the user. Resume only after the
  user decides, record the decision, and pass it to a fresh implementer.

Treat a gap as minor only when it violates no explicit acceptance criterion
and has no meaningful effect on observable behavior, security or privacy,
stored data, compatibility, public interfaces, or architectural boundaries.
Optional polish and non-required hardening may be minor. If uncertain, treat
the gap as material.

Pause for human review when the same material gap survives two consecutive
remediation audits, two agents reach incompatible conclusions about plan
intent, or progress requires expanding the supplied scope.

## Govern decisions

Require a human decision before proceeding when a choice would materially
change:

- user-visible behavior or acceptance criteria;
- public interfaces or compatibility guarantees;
- persisted data, migrations, or destructive behavior;
- security, privacy, permissions, or trust boundaries;
- architecture or repository responsibility boundaries;
- dependencies, licensing, deployment, or operational commitments; or
- the plan's scope or explicit assumptions.

Present a compact decision brief containing the question, evidence, viable
options, recommended option, and consequences. Do not let a child agent make
the choice implicitly.

Allow agents to choose local implementation details that preserve the plan and
repository conventions, including internal naming, small refactors, test
arrangement, and equivalent library usage. Add any departure from the written
plan to the deviation ledger even when it is lightweight.

## Report completion

Return a self-contained final report containing:

- plan identity and final verdict;
- number of implementation and audit passes;
- validation commands and outcomes;
- remaining minor gaps;
- human decisions applied;
- the complete deviation ledger; and
- blockers or unverified claims, if any.

Never commit, push, open a pull request, or alter unrelated work unless the
user explicitly requested it.
