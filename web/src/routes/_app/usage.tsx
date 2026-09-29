import { useQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { FilterIcon, XIcon } from "lucide-react";
import { useState } from "react";
import { PageHeader } from "#/components/page-header";
import { RangePicker } from "#/components/range-picker";
import { Segmented } from "#/components/segmented";
import { StripChart } from "#/components/strip-chart";
import { Button } from "#/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuCheckboxItem,
	DropdownMenuContent,
	DropdownMenuGroup,
	DropdownMenuLabel,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "#/components/ui/dropdown-menu";
import {
	Table,
	TableBody,
	TableCell,
	TableFooter,
	TableHead,
	TableHeader,
	TableRow,
} from "#/components/ui/table";
import {
	CATEGORIES,
	formatAmount,
	formatCount,
	formatUsd,
	GROUPS,
	METRICS,
	type MetricKey,
	type Range,
	rangeSearch,
	type StatsTotals,
	statsQuery,
} from "#/lib/stats";

export const Route = createFileRoute("/_app/usage")({
	validateSearch: rangeSearch,
	component: UsagePage,
});

type Filters = { keys: number[]; models: string[]; upstreams: string[] };
const NO_FILTERS: Filters = { keys: [], models: [], upstreams: [] };

function UsagePage() {
	const { range: preset, from, to, group } = Route.useSearch();
	const range = { range: preset, from, to };
	const navigate = useNavigate({ from: Route.fullPath });
	const [metric, setMetric] = useState<MetricKey>("cost");
	const [filters, setFilters] = useState<Filters>(NO_FILTERS);
	const { data: stats, error } = useQuery(
		statsQuery({ range, group, ...filters }),
	);

	return (
		<div className="space-y-6">
			<PageHeader
				title="Usage"
				description="Requests, tokens and their API value, split by model, API key or upstream."
				actions={
					<RangePicker
						value={range}
						onChange={(next) =>
							navigate({
								search: (s) => ({
									...s,
									from: undefined,
									to: undefined,
									...next,
								}),
							})
						}
					/>
				}
			/>
			<div className="flex flex-wrap items-center gap-3">
				<Segmented
					label="Group by"
					options={GROUPS}
					value={group}
					onChange={(next) =>
						navigate({ search: (s) => ({ ...s, group: next }) })
					}
				/>
				<Segmented
					label="Chart shows"
					options={METRICS}
					value={metric}
					onChange={setMetric}
				/>
				<FilterMenu range={range} filters={filters} onChange={setFilters} />
			</div>
			{error && <p className="text-sm text-destructive">{error.message}</p>}
			{stats && (
				<>
					<section className="rounded-xl border bg-card p-5">
						<StripChart stats={stats} metric={metric} />
					</section>
					<UsageTable
						groups={stats.groups}
						totals={stats.totals}
						grouped={group !== "none"}
					/>
					{Object.keys(stats.rejected).length > 0 && (
						<p className="text-xs text-muted-foreground">
							Refused before routing:{" "}
							{Object.entries(stats.rejected)
								.map(
									([reason, count]) =>
										`${formatCount(count)} ${REASONS[reason] ?? reason}`,
								)
								.join(", ")}
							. These requests had no valid key or were over a limit, so they
							are not in the table.
						</p>
					)}
				</>
			)}
		</div>
	);
}

const REASONS: Record<string, string> = {
	no_key: "without a key",
	bad_key: "with an unknown key",
	revoked: "with a revoked key",
	expired: "with an expired key",
	rate_limited: "over a rate limit",
	not_allowed: "for a model the key may not use",
	too_large: "too large",
};

/**
 * One row per group with the categories that have usage in the range. Each cell has the
 * amount and, below it, its API value.
 */
function UsageTable({
	groups,
	totals,
	grouped,
}: {
	groups: { key: string; label: string; totals: StatsTotals }[];
	totals: StatsTotals;
	grouped: boolean;
}) {
	const columns = CATEGORIES.filter((c) => (totals.amounts[c.key] ?? 0) > 0);
	if (totals.requests === 0) {
		return (
			<p className="rounded-xl border border-dashed px-6 py-10 text-center text-sm text-muted-foreground">
				No requests in this range. Choose a longer range or other filters.
			</p>
		);
	}
	return (
		<section className="rounded-xl border bg-card">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead className="sticky left-0 z-10 bg-card">
							{grouped ? "Group" : "Requests"}
						</TableHead>
						<TableHead className="text-right">Requests</TableHead>
						<TableHead className="text-right whitespace-nowrap">
							API value
						</TableHead>
						<TableHead className="text-right whitespace-nowrap">
							Reported
						</TableHead>
						{columns.map((c) => (
							<TableHead key={c.key} className="text-right whitespace-nowrap">
								{c.label}
							</TableHead>
						))}
					</TableRow>
				</TableHeader>
				<TableBody>
					{groups.map((group) => (
						<Row
							key={group.key}
							label={group.label}
							totals={group.totals}
							columns={columns}
						/>
					))}
				</TableBody>
				{grouped && groups.length > 1 && (
					<TableFooter>
						<Row label="Total" totals={totals} columns={columns} />
					</TableFooter>
				)}
			</Table>
		</section>
	);
}

function Row({
	label,
	totals,
	columns,
}: {
	label: string;
	totals: StatsTotals;
	columns: readonly (typeof CATEGORIES)[number][];
}) {
	return (
		<TableRow>
			<TableCell className="sticky left-0 z-10 max-w-64 truncate bg-card font-medium">
				{label}
			</TableCell>
			<TableCell className="text-right font-mono text-xs tabular-nums">
				{formatCount(totals.requests)}
				{totals.errors > 0 && (
					<div className="text-muted-foreground">
						{formatCount(totals.errors)} failed
					</div>
				)}
			</TableCell>
			<TableCell className="text-right font-mono text-xs tabular-nums">
				<span className="font-medium">{formatUsd(totals.cost_nano)}</span>
				{(totals.unpriced > 0 || totals.incomplete > 0) && (
					<div
						className="text-meter"
						title="Some rows have no price, estimated usage or missing prices."
					>
						{totals.unpriced > 0
							? `${formatCount(totals.unpriced)} unpriced`
							: `${formatCount(totals.incomplete)} incomplete`}
					</div>
				)}
			</TableCell>
			<TableCell className="text-right font-mono text-xs tabular-nums">
				{totals.reported_cost_nano > 0
					? formatUsd(totals.reported_cost_nano)
					: "—"}
			</TableCell>
			{columns.map((c) => {
				const amount = totals.amounts[c.key] ?? 0;
				const cost = totals.costs[c.key] ?? 0;
				return (
					<TableCell
						key={c.key}
						className="text-right font-mono text-xs tabular-nums"
					>
						{amount > 0 ? formatAmount(amount, c.unit) : "—"}
						{cost > 0 && (
							<div className="text-muted-foreground">{formatUsd(cost)}</div>
						)}
					</TableCell>
				);
			})}
		</TableRow>
	);
}

/**
 * Filters by API key, model and upstream. The choices are the groups that have usage in the
 * range.
 */
function FilterMenu({
	range,
	filters,
	onChange,
}: {
	range: Range;
	filters: Filters;
	onChange: (filters: Filters) => void;
}) {
	const keys = useQuery(statsQuery({ range, group: "key" })).data?.groups ?? [];
	const models =
		useQuery(statsQuery({ range, group: "model" })).data?.groups ?? [];
	const upstreams =
		useQuery(statsQuery({ range, group: "upstream" })).data?.groups ?? [];
	const count =
		filters.keys.length + filters.models.length + filters.upstreams.length;
	const toggle = <T,>(list: T[], value: T) =>
		list.includes(value) ? list.filter((v) => v !== value) : [...list, value];

	return (
		<div className="flex items-center gap-1">
			<DropdownMenu>
				<DropdownMenuTrigger
					render={
						<Button variant="outline" size="sm">
							<FilterIcon />
							{count > 0
								? `${count} ${count === 1 ? "filter" : "filters"}`
								: "Filter"}
						</Button>
					}
				/>
				<DropdownMenuContent
					align="start"
					className="max-h-96 w-72 overflow-y-auto"
				>
					<DropdownMenuGroup>
						<DropdownMenuLabel>API keys</DropdownMenuLabel>
						{keys
							.filter((g) => g.key !== "deleted")
							.map((g) => (
								<DropdownMenuCheckboxItem
									key={g.key}
									checked={filters.keys.includes(Number(g.key))}
									onCheckedChange={() =>
										onChange({
											...filters,
											keys: toggle(filters.keys, Number(g.key)),
										})
									}
								>
									<span className="truncate">{g.label}</span>
								</DropdownMenuCheckboxItem>
							))}
					</DropdownMenuGroup>
					<DropdownMenuSeparator />
					<DropdownMenuGroup>
						<DropdownMenuLabel>Models</DropdownMenuLabel>
						{models.map((g) => (
							<DropdownMenuCheckboxItem
								key={g.key}
								checked={filters.models.includes(g.key)}
								onCheckedChange={() =>
									onChange({
										...filters,
										models: toggle(filters.models, g.key),
									})
								}
							>
								<span className="truncate font-mono text-xs">{g.label}</span>
							</DropdownMenuCheckboxItem>
						))}
					</DropdownMenuGroup>
					<DropdownMenuSeparator />
					<DropdownMenuGroup>
						<DropdownMenuLabel>Upstreams</DropdownMenuLabel>
						{upstreams.map((g) => (
							<DropdownMenuCheckboxItem
								key={g.key}
								checked={filters.upstreams.includes(g.key)}
								onCheckedChange={() =>
									onChange({
										...filters,
										upstreams: toggle(filters.upstreams, g.key),
									})
								}
							>
								<span className="truncate">{g.label}</span>
							</DropdownMenuCheckboxItem>
						))}
					</DropdownMenuGroup>
				</DropdownMenuContent>
			</DropdownMenu>
			{count > 0 && (
				<Button variant="ghost" size="sm" onClick={() => onChange(NO_FILTERS)}>
					<XIcon />
					Clear
				</Button>
			)}
		</div>
	);
}
