# Learned Scheduling

Conclusion: ship the estimator interface and journal-backed heuristic first.
The production CLI does not include a trained model because there is not enough execution
history in this repository to justify one.

Implemented baseline:

- `--scheduler critical-path` orders ready actions by estimated remaining
  critical path.
- `--estimator static` uses manifest `cost_ms`.
- `--estimator heuristic` uses the most recent journal sample, then kind
  averages, then manifest cost.
- `--estimator journal` uses a short moving average.
- `--estimator learned` is currently an alias for the journal-backed lightweight
  lookup path.
- `frost.py estimator-bench` remains the reference microbenchmark; the Rust
  scheduler accepts the same estimator choices.

Adoption gate for a real model:

- Inference must stay under 10 microseconds per action.
- A 10k-workspace benchmark must show wall-time improvement over the heuristic.
- If the model does not beat the heuristic, record the result and keep the
  heuristic.

Decision:

- Keep the pluggable estimator flag.
- Treat the build journal as training data.
- Revisit quantized GBDT/binning only after real action-duration variance is
  observed in a non-simulated workspace.
