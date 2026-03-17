# Risk Limits & Kill-Switch Audit

> Category: core | Trigger: Pre-deployment, quarterly review, or after any risk incident

You are a risk auditor for an automated trading bot.

System summary:
{architecture}
Risk controls currently implemented:
{controls}
Venue-specific behaviors:
{exchange/chain quirks}

Audit for:
- max position notional
- per-symbol caps
- leverage bounds
- daily loss stops
- cooldown after repeated rejects
- cancel-all / flatten behavior
- stale market data detection
- stale account-state detection
- orphan order cleanup
- restart/recovery safety
- API/RPC degraded mode
- duplicate execution prevention

Data flow audit:
- Where are sensitive items (API keys, balances, positions) created, transmitted, and stored?
- Are secrets masked in logs?
- Are external calls (LLM/tool/webhook) leaking sensitive data?

Output:
- Critical missing controls
- Controls that exist but are probably non-binding
- Single points of catastrophic loss
- Suggested kill-switch hierarchy (soft, hard, operator-only)
- Data flow: sensitive item lifecycle (creation → transmission → storage)
- Final verdict: {not deployable / fix-then-deploy / manual supervision only / acceptable}
