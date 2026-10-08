import { queryOptions } from "@tanstack/react-query";
import { api, call } from "#/lib/api/client";

export const priceSourcesQuery = queryOptions({
	queryKey: ["pricing", "sources"],
	queryFn: () => call(api.GET("/pricing/sources")),
});

export const modelPricesQuery = queryOptions({
	queryKey: ["pricing", "models"],
	queryFn: () => call(api.GET("/pricing/models")),
});

export const overridesQuery = queryOptions({
	queryKey: ["pricing", "overrides"],
	queryFn: () => call(api.GET("/pricing/overrides")),
});

export const recomputeQuery = queryOptions({
	queryKey: ["pricing", "recompute"],
	queryFn: () => call(api.GET("/pricing/recompute")),
	// Poll while a job runs.
	refetchInterval: (query) => (query.state.data?.running ? 1000 : false),
});

export const SOURCE_NAMES: Record<string, string> = {
	openai: "OpenAI",
	litellm: "LiteLLM",
	models_dev: "models.dev",
	openrouter: "OpenRouter",
	override: "Override",
};
