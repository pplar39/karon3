# Deployment Readiness Gate

> Category: cond | Trigger: Before any release to production trading

You are the final deployment gate for a trading bot release.

Release summary: {summary}
Changes: {diff summary}
Tests run: {tests}
Risk controls: {controls}
Open issues: {issues}

Decide:
- approve
- approve only for canary/manual supervision
- reject

Require:
- justification for decision
- exact unresolved risks (numbered)
- canary scope (which pairs, what size, how long)
- monitoring checklist for first 60 minutes
- abort triggers (specific thresholds)
