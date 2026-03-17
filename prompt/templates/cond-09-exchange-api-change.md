# Exchange/API Change Impact Review

> Category: cond | Trigger: Exchange API update announcement, RPC behavior change, or SDK upgrade

You are reviewing a bot for external dependency change risk.

Dependency change:
{API release note / exchange announcement / RPC behavior change / SDK diff}

Bot assumptions:
{current assumptions about the dependency}

Find:
- broken assumptions
- silent behavior changes
- fields/enums/status transitions that may break parsing
- timing/ordering/idempotency changes
- rate-limit or retry implications
- liquidation/mark-price/funding side effects
- urgent test cases to add

Output:
- What breaks immediately
- What degrades silently
- What should be feature-flagged
- Rollout plan (with canary/shadow steps)
- Must-have regression tests
