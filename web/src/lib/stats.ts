import { queryOptions } from "@tanstack/react-query";
import { z } from "zod";
import { api, call, type Schemas } from "#/lib/api/client";

export type StatsRequest = Schemas["StatsRequest"];
export type StatsResponse = Schemas["StatsResponse"];
export type StatsTotals = Schemas["StatsTotals"];
export type StatsGroup = Schemas["StatsGroup"];

/** A stats request with a range preset in place of the times. */
export type StatsParams = Omit<StatsRequest, "from" | "to" | "time_zone"> & {
	range: Range;
};

/** The times of the range are computed at each load, so a refresh shows new usage. */
export const statsQuery = ({ range, ...params }: StatsParams) =>
	queryOptions({
		queryKey: ["stats", range.range, range.from, range.to, params],
		queryFn: () => {
			const [from, to] = rangeBounds(range);
			return call(
				api.POST("/stats", {
					body: { ...params, from, to, time_zone: TIME_ZONE },
				}),
			);
		},
		refetchInterval: 60_000,
	});

const day = new Intl.DateTimeFormat("en-GB", {
	day: "numeric",
	month: "short",
	year: "numeric",
});

/** The name of a range, for example "7 days" or "1 Sept 2026 – 29 Sept 2026". */
export function rangeName(range: Range): string {
	if (range.range !== "custom" || !range.from || !range.to)
		return RANGES[range.range];
	const name = (date: string) => day.format(new Date(`${date}T00:00`));
	return range.from === range.to
		? name(range.from)
		: `${name(range.from)} – ${name(range.to)}`;
}

/** A local date as `YYYY-MM-DD`. */
export function localDate(date: Date): string {
	const pad = (n: number) => String(n).padStart(2, "0");
	return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** The browser time zone: range presets and buckets use it. */
export const TIME_ZONE = Intl.DateTimeFormat().resolvedOptions().timeZone;

export const RANGES = {
	"24h": "24 hours",
	"7d": "7 days",
	"30d": "30 days",
	"90d": "90 days",
	month: "This month",
	custom: "Dates",
} as const;
export type RangeKey = keyof typeof RANGES;

/** A preset, or `custom` with local dates (`YYYY-MM-DD`, both included). */
export type Range = { range: RangeKey; from?: string; to?: string };

export const GROUPS = {
	model: "Model",
	key: "API key",
	upstream: "Upstream",
	none: "All",
} as const;
export type GroupKey = keyof typeof GROUPS;

/** Search parameters of the Overview and Usage pages. */
const date = z
	.string()
	.regex(/^\d{4}-\d{2}-\d{2}$/)
	.optional()
	.catch(undefined);

export const rangeSearch = z.object({
	range: z
		.enum(["24h", "7d", "30d", "90d", "month", "custom"])
		.default("7d")
		.catch("7d"),
	from: date,
	to: date,
	group: z
		.enum(["model", "key", "upstream", "none"])
		.default("model")
		.catch("model"),
});

/**
 * The unix seconds of a preset, in the browser time zone. Day presets start at a local
 * midnight, so the first bucket is a whole day.
 */
export function rangeBounds(
	{ range, from, to: until }: Range,
	now = new Date(),
): [number, number] {
	if (range === "custom" && from && until) {
		// Local midnights from the calendar dates; a midnight in a clock change gap becomes
		// the moment of the change.
		const midnight = (date: string, days = 0) => {
			const [year, month, day] = date.split("-").map(Number);
			return new Date(year ?? 1970, (month ?? 1) - 1, (day ?? 1) + days);
		};
		const start = midnight(from);
		const end = midnight(until, 1);
		return [
			Math.floor(start.getTime() / 1000),
			Math.floor(end.getTime() / 1000),
		];
	}
	const to = Math.ceil(now.getTime() / 1000);
	if (range === "24h") return [to - 86_400, to];
	const start = new Date(now);
	start.setHours(0, 0, 0, 0);
	if (range === "month") start.setDate(1);
	else start.setDate(start.getDate() - (Number.parseInt(range, 10) || 7) + 1);
	return [Math.floor(start.getTime() / 1000), to];
}

/** The usage categories in the order of the price lists, with their names in the UI. */
export const CATEGORIES = [
	{ key: "input_text", label: "Input", unit: "tokens" },
	{ key: "input_text_cached", label: "Cached input", unit: "tokens" },
	{ key: "cache_write_5m", label: "Cache write 5m", unit: "tokens" },
	{ key: "cache_write_1h", label: "Cache write 1h", unit: "tokens" },
	{ key: "input_audio", label: "Audio input", unit: "tokens" },
	{ key: "input_audio_cached", label: "Cached audio", unit: "tokens" },
	{ key: "input_image", label: "Image input", unit: "tokens" },
	{ key: "input_image_cached", label: "Cached image", unit: "tokens" },
	{ key: "output_text", label: "Output", unit: "tokens" },
	{ key: "output_reasoning", label: "Reasoning", unit: "tokens" },
	{ key: "output_audio", label: "Audio output", unit: "tokens" },
	{ key: "output_image", label: "Image output", unit: "tokens" },
	{ key: "web_search_calls", label: "Web searches", unit: "calls" },
	{ key: "web_search_preview_calls", label: "Preview searches", unit: "calls" },
	{ key: "images_generated", label: "Images", unit: "images" },
	{ key: "characters", label: "Speech", unit: "characters" },
	{ key: "seconds", label: "Transcription", unit: "seconds" },
] as const;

const INPUT_KEYS = CATEGORIES.slice(0, 8).map((c) => c.key);
const OUTPUT_KEYS = CATEGORIES.slice(8, 12).map((c) => c.key);

export function inputTokens(totals: StatsTotals): number {
	return INPUT_KEYS.reduce((sum, key) => sum + (totals.amounts[key] ?? 0), 0);
}

export function outputTokens(totals: StatsTotals): number {
	return OUTPUT_KEYS.reduce((sum, key) => sum + (totals.amounts[key] ?? 0), 0);
}

const usd = new Intl.NumberFormat("en-US", {
	style: "currency",
	currency: "USD",
});
const smallUsd = new Intl.NumberFormat("en-US", {
	style: "currency",
	currency: "USD",
	maximumSignificantDigits: 3,
});

/** Nano-USD as dollars. Amounts below one cent keep 3 significant digits. */
export function formatUsd(nano: number): string {
	const dollars = nano / 1e9;
	if (dollars !== 0 && Math.abs(dollars) < 0.01)
		return smallUsd.format(dollars);
	return usd.format(dollars);
}

const compact = new Intl.NumberFormat("en-GB", {
	notation: "compact",
	maximumFractionDigits: 1,
});
const whole = new Intl.NumberFormat("en-GB");

/** Large counts as 1.2M; small ones in full. */
export function formatCount(value: number): string {
	return Math.abs(value) < 10_000
		? whole.format(Math.round(value))
		: compact.format(value);
}

export function formatAmount(value: number, unit: string): string {
	if (unit === "seconds") return `${formatCount(value)} s`;
	return formatCount(value);
}

/** Colors of the chart groups; the last one is for "Other". */
export const SERIES = 6;
export function seriesColor(index: number): string {
	return `var(--series-${Math.min(index, SERIES) + 1})`;
}

/** The metric that a chart shows per bucket. */
export const METRICS = {
	cost: "Cost",
	tokens: "Tokens",
	requests: "Requests",
} as const;
export type MetricKey = keyof typeof METRICS;

export function pointValue(
	point: Schemas["StatsPoint"],
	metric: MetricKey,
): number {
	if (metric === "cost") return point.cost_nano;
	if (metric === "tokens") return point.input_tokens + point.output_tokens;
	return point.requests;
}

export function formatMetric(value: number, metric: MetricKey): string {
	return metric === "cost" ? formatUsd(value) : formatCount(value);
}
