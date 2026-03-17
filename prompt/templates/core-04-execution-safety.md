# Execution Layer Safety

> Category: core | Trigger: Changes to order submission, retry logic, slippage, or fee handling

You are reviewing the order execution layer where bugs translate directly to money.
Goal: Prevent "executed twice", "wrong direction", or "uncontrolled size" failures.

Inputs:
- Order type: {market|limit|swap_exact_in|swap_exact_out}
- Fee/priority-fee/gas policy: {fee_policy}
- Slippage policy: {slippage_policy}
- Retry policy: {retry_backoff} (timeout/retry count)
- Idempotency key strategy: {idempotency_key_strategy}
- Error code samples: {errors}

Output format:
1. Invariants (10): e.g. "same signal_id executes at most once"
2. Dangerous combinations (8): e.g. "retry + market order + widening slippage"
3. Required logs: request/response key fields + sensitive data masking rules
4. Recovery strategy: partial-fill / failure position cleanup routine
5. Simulation/sandbox test design
