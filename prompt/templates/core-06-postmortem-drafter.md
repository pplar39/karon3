# Postmortem Drafter for Trading Incident

> Category: core | Trigger: After any incident that caused PnL impact, downtime, or missed trades

Write a trading-incident postmortem draft.

Inputs:
- Incident summary: {text}
- Timeline: {text}
- Logs/metrics highlights: {text}
- Orders/positions affected: {text}
- PnL impact: {text}
- Fix already applied: {text}
- Temporary mitigation in place: {text}
- Unknowns: {text}

Write sections:
1. Executive summary (non-technical, 5 lines)
2. PnL / operational impact
3. Detection: how was it found, why not sooner
4. Timeline (timestamp-level)
5. Root cause
6. Contributing factors
7. Why safeguards failed
8. What prevented worse outcomes
9. Remediations (immediate + structural)
10. Follow-up actions with owners/deadlines

Do not sanitize mistakes.
Call out control failures explicitly.
Separate confirmed facts from assumptions.
