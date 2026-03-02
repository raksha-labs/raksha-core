# history-worker

`history-worker` now ingests two streams into the `history` schema:

1. Operational lifecycle tables (`incidents`, `alerts`, `detections`, `feature_vectors`).
2. SimLab historical scenario packs (`scenarios/historical/*.yaml|json`) plus optional curated source datasets.

## SimLab import env vars

- `SIMLAB_SCENARIOS_DIR` (optional): defaults to `scenarios/historical`.
- `SIMLAB_CURATED_DATASETS_DIR` (optional): defaults to `datasets/curated`.
- `SIMLAB_TENANT_ID` (optional): defaults to `glider`.
- `HISTORY_ARCHIVE_DIR` (optional): defaults to `history-archive`.
- `HISTORY_BUCKET` + `HISTORY_PREFIX` (optional): used for `object_prefix`/URI metadata.
- `HISTORY_WORKER_RUN_ONCE=true`: run a single sync pass and exit.

## One-shot import example

```bash
HISTORY_WORKER_RUN_ONCE=true \
SIMLAB_SCENARIOS_DIR=./scenarios/historical \
SIMLAB_CURATED_DATASETS_DIR=../raksha-simlab/datasets/curated \
DATABASE_URL='postgres://...' \
cargo run -p history-worker
```
