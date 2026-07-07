# Model Scorecard — Test Fixture (Tier Mismatch)

This scorecard has a different tier for sonnet-5 than conductor.toml.

## Live Roster

| Model | Dispatch ID | Tier (owns) | Ceiling | Reliability | Notes |
|---|---|---|---|---|---|
| **sonnet-5** | claude-sonnet-5 | **Senior** | L | high | Tier mismatch: config says Lead |
| **opus-4.8** | claude-opus-4-8 | **Lead** | XL | high | Test fixture |
| **gpt-5.5** | openai-codex/gpt-5.5 | **Senior** | L | high | Test fixture |
| **minimax-m3** | opencode-go/minimax-m3 | **Senior** | M | high | Test fixture |
| **qwen3.7-max** | opencode-go/qwen3.7-max | **Senior** | M | high | Test fixture |
| **glm-5.2** | opencode-go/glm-5.2 | **Senior** | M | good | Test fixture |
| **glm-5.1** | opencode-go/glm-5.1 | **Junior** | S | unproven | Test fixture |
| **mimo-v2.5** | opencode-go/mimo-v2.5 | **Junior** | S | unproven | Test fixture |
| **qwen3.6-plus** | opencode-go/qwen3.6-plus | **Junior** | S | unproven | Test fixture |
| **deepseek-v4-flash** | opencode-go/deepseek-v4-flash | **Junior** | S | unproven | Test fixture |
| **gemini-3.5-flash-free** | google-ai-studio/gemini-3.5-flash | **Junior** | S | unproven | Test fixture |
| **agy-gemini-3.5-flash-free** | Gemini 3.5 Flash (High) | **Junior** | S | unproven | Test fixture |
| **nw-glm-5.2** | neuralwatt/glm-5.2 | **Senior** | M | good | Test fixture |
| **nw-glm-5.2-short** | neuralwatt/glm-5.2-short | **Senior** | M | good | Test fixture |
| **nw-glm-5.2-fast** | neuralwatt/glm-5.2-fast | **Junior** | S | good | Test fixture |
| **nw-glm-5.2-short-fast** | neuralwatt/glm-5.2-short-fast | **Junior** | S | good | Test fixture |
| **nw-kimi-k2.6** | neuralwatt/kimi-k2.6 | **Senior** | M | good | Test fixture |
| **nw-kimi-k2.6-fast** | neuralwatt/kimi-k2.6-fast | **Junior** | S | good | Test fixture |
