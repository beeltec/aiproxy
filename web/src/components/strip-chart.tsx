import { useState } from "react";
import {
	formatMetric,
	METRICS,
	type MetricKey,
	pointValue,
	SERIES,
	type StatsResponse,
	seriesColor,
	TIME_ZONE,
} from "#/lib/stats";
import { cn } from "#/lib/utils";

type Series = { key: string; label: string; color: string; values: number[] };

/** The largest groups by the metric, and the rest as "Other". */
function series(stats: StatsResponse, metric: MetricKey): Series[] {
	const groups = stats.groups
		.map((group) => {
			const values = group.series.map((point) => pointValue(point, metric));
			return { group, values, total: values.reduce((a, b) => a + b, 0) };
		})
		.filter((g) => g.total > 0)
		.sort((a, b) => b.total - a.total);
	const shown: Series[] = groups
		.slice(0, SERIES)
		.map(({ group, values }, index) => ({
			key: group.key,
			label: group.label,
			color: seriesColor(index),
			values,
		}));
	const rest = groups.slice(SERIES);
	if (rest.length > 0) {
		shown.push({
			key: "other",
			label: `${rest.length} more`,
			color: seriesColor(SERIES),
			values: stats.buckets.map((_, i) =>
				rest.reduce((sum, g) => sum + (g.values[i] ?? 0), 0),
			),
		});
	}
	return shown;
}

/** A top value for the scale: 1, 2 or 5 times a power of ten. */
function niceMax(value: number): number {
	if (value <= 0) return 1;
	const power = 10 ** Math.floor(Math.log10(value));
	const step = [1, 2, 5, 10].find((f) => f * power >= value) ?? 10;
	return step * power;
}

function bucketLabel(start: number, bucket: string, long = false): string {
	const options: Intl.DateTimeFormatOptions =
		bucket === "hour"
			? long
				? { weekday: "short", hour: "2-digit", minute: "2-digit" }
				: { hour: "2-digit", minute: "2-digit" }
			: bucket === "month"
				? { month: "short", year: "numeric" }
				: { day: "numeric", month: "short" };
	const label = new Intl.DateTimeFormat("en-GB", {
		...options,
		timeZone: TIME_ZONE,
	}).format(start * 1000);
	return bucket === "week" && long ? `Week of ${label}` : label;
}

/**
 * Stacked bars per time bucket on graph paper, like the chart of a strip recorder. The bars
 * show the largest groups; hovering or focusing a bar shows its values.
 */
export function StripChart({
	stats,
	metric,
	className,
}: {
	stats: StatsResponse;
	metric: MetricKey;
	className?: string;
}) {
	const [active, setActive] = useState<number | null>(null);
	const lines = series(stats, metric);
	const columns = stats.buckets.map((_, i) =>
		lines.reduce((sum, s) => sum + (s.values[i] ?? 0), 0),
	);
	const max = niceMax(Math.max(0, ...columns));
	const total = columns.reduce((a, b) => a + b, 0);
	const every = Math.max(1, Math.ceil(stats.buckets.length / 6));

	return (
		<figure className={cn("space-y-3", className)}>
			<div className="relative">
				<div className="graph-paper relative flex h-56 items-end gap-px overflow-hidden rounded-lg border px-2 pt-6 sm:gap-[3px]">
					<span className="eyebrow absolute top-2 right-2">
						{formatMetric(max, metric)}
					</span>
					<span
						aria-hidden="true"
						className="absolute inset-x-0 top-6 border-t border-dashed border-foreground/15"
					/>
					{total === 0 && (
						<p className="absolute inset-0 flex items-center justify-center text-sm text-muted-foreground">
							No usage in this range.
						</p>
					)}
					{stats.buckets.map((start, index) => (
						<button
							key={start}
							type="button"
							aria-label={`${bucketLabel(start, stats.bucket, true)}: ${formatMetric(columns[index] ?? 0, metric)}`}
							onMouseEnter={() => setActive(index)}
							onMouseLeave={() => setActive(null)}
							onFocus={() => setActive(index)}
							onBlur={() => setActive(null)}
							className={cn(
								"group relative flex h-full min-w-0 flex-1 flex-col-reverse rounded-t-[2px] outline-none",
								"focus-visible:ring-2 focus-visible:ring-ring",
								active === index && "bg-foreground/5",
							)}
						>
							{lines.map((line) => {
								const value = line.values[index] ?? 0;
								if (value <= 0) return null;
								return (
									<span
										key={line.key}
										className="w-full first:rounded-b-none last:rounded-t-[2px]"
										style={{
											height: `${(value / max) * 100}%`,
											backgroundColor: line.color,
										}}
									/>
								);
							})}
						</button>
					))}
				</div>
				{active !== null && columns[active] !== undefined && (
					<ChartTip
						stats={stats}
						index={active}
						lines={lines}
						metric={metric}
						total={columns[active] ?? 0}
					/>
				)}
			</div>
			<div className="flex justify-between gap-2 px-2 font-mono text-[0.625rem] text-muted-foreground">
				{stats.buckets
					.filter((_, i) => i % every === 0)
					.map((start) => (
						<span key={start}>{bucketLabel(start, stats.bucket)}</span>
					))}
			</div>
			{lines.length > 0 && (
				<figcaption className="flex flex-wrap gap-x-4 gap-y-1.5 text-xs">
					{lines.map((line) => (
						<span key={line.key} className="flex min-w-0 items-center gap-1.5">
							<span
								className="size-2.5 shrink-0 rounded-[2px]"
								style={{ backgroundColor: line.color }}
							/>
							<span className="truncate">{line.label}</span>
						</span>
					))}
				</figcaption>
			)}
		</figure>
	);
}

function ChartTip({
	stats,
	index,
	lines,
	metric,
	total,
}: {
	stats: StatsResponse;
	index: number;
	lines: Series[];
	metric: MetricKey;
	total: number;
}) {
	const start = stats.buckets[index] ?? 0;
	// The tip stays on the side of the chart away from the bar.
	const right = index > stats.buckets.length / 2;
	const rows = lines.filter((line) => (line.values[index] ?? 0) > 0);
	return (
		<div
			role="status"
			className={cn(
				"pointer-events-none absolute top-2 z-10 w-60 rounded-md border bg-popover p-3 text-xs shadow-md",
				right ? "left-3" : "right-3",
			)}
		>
			<div className="flex items-baseline justify-between gap-2 border-b pb-1.5">
				<span className="font-medium">
					{bucketLabel(start, stats.bucket, true)}
				</span>
				<span className="font-mono tabular-nums">
					{formatMetric(total, metric)}
				</span>
			</div>
			{rows.length === 0 ? (
				<p className="pt-1.5 text-muted-foreground">
					No {METRICS[metric].toLowerCase()} here.
				</p>
			) : (
				<ul className="space-y-1 pt-1.5">
					{rows.map((line) => (
						<li key={line.key} className="flex items-center gap-1.5">
							<span
								className="size-2 shrink-0 rounded-[2px]"
								style={{ backgroundColor: line.color }}
							/>
							<span className="min-w-0 flex-1 truncate">{line.label}</span>
							<span className="font-mono tabular-nums">
								{formatMetric(line.values[index] ?? 0, metric)}
							</span>
						</li>
					))}
				</ul>
			)}
		</div>
	);
}
