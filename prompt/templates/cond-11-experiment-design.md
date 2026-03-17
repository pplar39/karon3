# Experiment Design for Strategy Change

> Category: cond | Trigger: Any proposed strategy/parameter change before live deployment

Design a falsifiable experiment plan for this strategy change.

Current system: {summary}
Proposed change: {change}
Suspected benefit: {benefit}
Risk: {risk}

Design:
- Primary metric
- Guardrail metrics (6): metrics that trigger immediate halt if they worsen
- Segmentation (by market regime, time, symbol)
- Ablations
- Minimum sample size logic (qualitative reasoning if exact calculation unavailable)
- Rollback thresholds (specific numbers)
- What result counts as "no evidence of benefit"
- What result counts as "too risky despite apparent alpha"

Output as a concrete step-by-step test plan, not theory.
