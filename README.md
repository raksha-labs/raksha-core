# defi-surv-core

Rust data-plane workspace for low-latency DeFi surveillance.

## Workspace crates

- `common-types`: canonical event and detection contracts.
- `core-interfaces`: `ChainAdapter`, `RuleEvaluator`, `RiskScorer`, `AlertSink`.
- `chain-adapter-evm`: config-driven EVM adapter stack (live + mock) for many chains.
- `reference-price`: reference price provider abstractions.
- `rule-engine`: YAML rule loader and deterministic evaluator.
- `trace-enricher`: suspicious-event enrichment stage.
- `risk-scorer`: deterministic 0-100 risk scoring.
- `alert-client`: webhook/slack/telegram sink abstraction.
- `event-stream`: Redis Streams publisher for detections/alerts.
- `persistence`: Postgres repository for detections/alerts.
- `replay-backtest`: fixture replay assertions.

## Apps

- `detector-worker`: detection worker with adapter + rule + enrich + score + alert + persist + stream pipeline.
- `replay-cli`: replay fixtures and print pass/fail summary.

## Environment

Copy `.env.example` and set relevant values.

```bash
cp .env.example .env
```

Important runtime variables:
- `RULES_REPO_PATH`: optional explicit path to `defi-surv-rules` (otherwise auto-discovered via relative paths).
- `ETH_WS_URL`: enables ETH live adapter for protocols defined in `defi-surv-rules/chains/ethereum/protocol_config.yaml`.
- `BASE_WS_URL`: enables Base live adapter for protocols defined in `defi-surv-rules/chains/base/protocol_config.yaml`.
- `DATABASE_URL`: enables Postgres persistence.
- `REDIS_URL`: enables Redis stream publishing.
- `ALERT_MANAGER_URL`: endpoint used by alert sinks (`/dispatch` route).

If live/persistence variables are not set, worker falls back gracefully to mock/no-op modes.

## Local infra for Week 2

```bash
docker compose -f infra/local/docker-compose.yml up -d
```

## Run detector worker

```bash
/usr/local/cargo/bin/cargo run -p detector-worker
```

## Run checks

```bash
/usr/local/cargo/bin/cargo check
/usr/local/cargo/bin/cargo test --workspace
```
