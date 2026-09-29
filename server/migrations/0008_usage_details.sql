-- Web search calls of the `web_search_preview` tool; `web_search_calls` has the `web_search` tool.
ALTER TABLE usage ADD COLUMN web_search_preview_calls INTEGER NOT NULL DEFAULT 0;
-- Anthropic `usage.speed` and `usage.inference_geo`.
ALTER TABLE usage ADD COLUMN speed TEXT;
ALTER TABLE usage ADD COLUMN inference_geo TEXT;
-- Chat predicted outputs. Only for information: the tokens are part of the output.
ALTER TABLE usage ADD COLUMN accepted_prediction_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN rejected_prediction_tokens INTEGER NOT NULL DEFAULT 0;
-- The cost that the provider reported (OpenRouter), in nano-USD.
ALTER TABLE usage ADD COLUMN reported_cost_nano INTEGER;
