# Documentation

`llm-router` routes Anthropic Messages API traffic to different Anthropic-compatible
upstreams based on the request model. This directory holds the operational and reference
documentation; the internal design lives in [ARCHITECTURE.md](../ARCHITECTURE.md).

| Document | Read it when |
|---|---|
| [Configuration](configuration.md) | Writing or reviewing a config file; env variables; validation rules |
| [Routing and transformation](routing.md) | Deciding how models map to upstreams; rewriting IDs; dropping headers or fields |
| [Observability](observability.md) | Scraping metrics; reading logs; writing PromQL |
| [Deployment](deployment.md) | Docker, Compose, Kubernetes, public exposure, probes |
| [Development](development.md) | Building, testing, linting, changing the code |
| [Troubleshooting](troubleshooting.md) | A request fails, is slow, or metrics look wrong |
| [Claude Code integration](claude-code.md) | Pointing Claude Code at the router and delegating to open models |

Start here if you are new:

1. [ARCHITECTURE.md](../ARCHITECTURE.md) — what the process does, end to end.
2. [Configuration](configuration.md) — the smallest working file.
3. [Claude Code integration](claude-code.md) — the intended client setup.
