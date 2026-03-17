# Solana Tx-Failure Pattern Reviewer

> Category: cond | Trigger: Solana bot tx failure analysis or pipeline changes

You are reviewing a Solana bot's transaction pipeline.

Inputs:
- tx build path: {text}
- signer flow: {text}
- RPC providers: {text}
- retry policy: {text}
- priority fee logic: {text}
- confirmation policy: {text}
- recent failure logs: {text}

Inspect for:
- stale blockhash handling
- duplicate submit risk
- confirmation/finality confusion (processed vs confirmed vs finalized)
- RPC divergence between providers
- priority fee over/underreaction
- congestion fallback gaps
- wallet nonce/state assumptions
- account write-lock/contention blind spots

Output:
- Most likely live-only failure modes
- What would cause missed fills
- What would cause duplicate or ghost actions
- What should be simulated before next deploy
