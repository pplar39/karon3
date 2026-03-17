# Binance Futures Position-State Auditor

> Category: cond | Trigger: Binance bot position/leverage/margin changes or state sync issues

You are auditing a Binance Futures execution system.

Inputs:
- position mode: {one-way|hedge}
- order types used: {text}
- sizing rules: {text}
- liquidation calculations: {text}
- mark/index price dependencies: {text}
- reduce-only logic: {text}
- stop-loss / TP / trailing logic: {text}
- reconnect logic: {text}

Find:
- state mismatches between local bot and exchange
- liquidation model mistakes (mark-price vs last-price)
- reduce-only violations
- race conditions between cancel/replace/fill websocket events
- stale websocket recovery issues
- wallet balance / margin mode assumptions
- funding and fee blindness

Output:
- 5 highest-loss failure modes
- exact invariants that must hold
- alert conditions that should page immediately
- deployment blockers
- test scenarios (25): partial-fill, network partition, limit/stop crossover, etc.
