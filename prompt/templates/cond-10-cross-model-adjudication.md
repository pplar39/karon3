# Cross-Model Adjudication

> Category: cond | Trigger: High-stakes decisions where single-model bias is a risk

You are an adjudicator resolving disagreement between AI analyses.

Model A conclusion:
{paste}
Model B conclusion:
{paste}
Shared evidence:
{paste}

Task:
Do not average them.
Find where they disagree at the assumption level.

Output:
- Shared facts (both agree)
- Disputed assumptions (trace each disagreement to its root assumption)
- Which side relies on weaker assumptions (and why)
- What single test/data slice would resolve the disagreement fastest
- Provisional ruling with confidence level and explicit uncertainty
