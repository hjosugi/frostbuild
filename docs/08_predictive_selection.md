# Predictive Test Selection

Conclusion: do not ship ML-based predictive test selection as the default.
The Rust CLI exposes `frost test --predictive` as an opt-in safe affected-set
mode so latency,
selection size, and miss-rate accounting have a CLI surface before a model is
trained.

Current implementation:

- The safe default is catalog-based affected test selection.
- `--predictive` currently uses the same no-miss affected set; it never reduces
  coverage without a replay-validated model.
- The Python reference model retains the distance-scoring experiment.

Adoption gate for a real model:

- Inference must stay under 1 ms per build on a 10k-target workspace.
- Bench JSON must report catalog-selected count, predictive-selected count, and
  known-failing-test misses.
- A model may only reduce the catalog set when CI can replay historical failures
  and quantify miss risk.

Decision:

- Keep catalog-based selection as default.
- Use the journal and CI history as training data later.
- Prefer GBDT or quantized lookup/binning first; only use a small int8 neural
  model if it improves miss-rate at the same latency.
