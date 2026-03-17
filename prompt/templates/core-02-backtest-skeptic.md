# Backtest Skeptic / Leakage Audit

> Category: core | Trigger: Any strategy change, backtest result review, or parameter optimization

Act as a hostile reviewer of this backtest.

Inputs:
- Strategy description: {text}
- Data sources: {text}
- Entry/exit rules: {text}
- Fees/slippage assumptions: {text}
- Funding rate assumptions: {text — separate from fees, especially for futures}
- Results summary: {text/table}
- Validation method: {walk-forward/cv/etc}

Your job:
Assume the backtest is overstated until proven otherwise.

Check for:
- lookahead leakage
- survivorship bias
- unrealistic fills (maker assumption on aggressive signals)
- fee underestimation
- funding rate cost vs backtest assumption (critical for futures — funding is not a fee)
- spread/latency blindness
- regime overfitting
- parameter mining
- train/test contamination
- unstable edge concentration in few days/tokens/events

Output:
- Confidence score (0-100)
- Top 7 evidence of overstatement (specific, not generic)
- Missing validations
- Minimum next experiments required
- Conditions that would invalidate the strategy regardless of backtest result (kill criteria)
