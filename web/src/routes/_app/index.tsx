import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { ArrowRightIcon, CircleAlertIcon } from "lucide-react";
import { GaugeMark } from "#/components/brand";
import { MeterRegister } from "#/components/meter-register";
import { PageHeader } from "#/components/page-header";
import { QuotaMeters } from "#/components/quota-meters";
import { RangePicker } from "#/components/range-picker";
import { Segmented } from "#/components/segmented";
import { StripChart } from "#/components/strip-chart";
import { accountsQuery } from "#/lib/accounts";
import { formatDateTime } from "#/lib/format";
import {
	modelPricesQuery,
	priceSourcesQuery,
	SOURCE_NAMES,
} from "#/lib/pricing";
import { settingsQuery } from "#/lib/settings";
import {
	formatCount,
	formatUsd,
	GROUPS,
	type GroupKey,
	inputTokens,
	outputTokens,
	type RangeKey,
	rangeName,
	rangeSearch,
	type StatsGroup,
	statsQuery,
} from "#/lib/stats";

export const Route = createFileRoute("/_app/")({
	validateSearch: rangeSearch,
	component: OverviewPage,
});

const CHART_GROUPS = {
	model: GROUPS.model,
	key: GROUPS.key,
	upstream: GROUPS.upstream,
};

function OverviewPage() {
	const { range: preset, from, to, group } = Route.useSearch();
	const range = { range: preset, from, to };
	const navigate = useNavigate({ from: Route.fullPath });
	const chartGroup = group === "none" ? "model" : group;
	const chart = useQuery(statsQuery({ range, group: chartGroup }));
	const models = useQuery(statsQuery({ range, group: "model" }));
	const keys = useQuery(statsQuery({ range, group: "key" }));
	const stats = chart.data;

	return (
		<div className="space-y-8">
			<PageHeader
				title="Overview"
				description="What the requests through this gateway would cost at API prices, and where they went."
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
			{chart.error && (
				<p className="text-sm text-destructive">{chart.error.message}</p>
			)}
			{stats && stats.totals.requests === 0 && <Empty />}
			{stats && stats.totals.requests > 0 && (
				<>
					<Reading totals={stats.totals} range={rangeName(range)} />
					<section className="space-y-4 rounded-xl border bg-card p-5">
						<div className="flex flex-wrap items-center justify-between gap-3">
							<h2 className="font-semibold">API value over time</h2>
							<Segmented
								label="Split by"
								options={CHART_GROUPS}
								value={chartGroup}
								onChange={(next) =>
									navigate({ search: (s) => ({ ...s, group: next }) })
								}
							/>
						</div>
						<StripChart stats={stats} metric="cost" />
					</section>
					<div className="grid gap-6 lg:grid-cols-2">
						<TopList
							title="Top models"
							groups={models.data?.groups}
							total={stats.totals.cost_nano}
							usage={{ ...range, group: "model" }}
						/>
						<TopList
							title="Top API keys"
							groups={keys.data?.groups}
							total={stats.totals.cost_nano}
							usage={{ ...range, group: "key" }}
						/>
					</div>
				</>
			)}
			<div className="grid gap-6 lg:grid-cols-2">
				<Accounts />
				<Attention />
			</div>
		</div>
	);
}

function Empty() {
	return (
		<div className="flex flex-col items-center gap-3 rounded-xl border border-dashed px-6 py-16 text-center">
			<GaugeMark className="size-10 text-muted-foreground" />
			<p className="font-medium">No usage in this range</p>
			<p className="max-w-sm text-sm text-muted-foreground">
				Send a request through the gateway, or choose a longer range. Usage and
				costs show up here within a minute.
			</p>
		</div>
	);
}

/** The API value as a meter reading, with the other totals as markings next to it. */
function Reading({
	totals,
	range,
}: {
	totals: StatsGroup["totals"];
	range: string;
}) {
	const notes = [
		totals.unpriced > 0 &&
			`${formatCount(totals.unpriced)} ${totals.unpriced === 1 ? "row has" : "rows have"} no price`,
		totals.incomplete > 0 &&
			`${formatCount(totals.incomplete)} ${totals.incomplete === 1 ? "cost is" : "costs are"} estimated or incomplete`,
	].filter(Boolean);
	return (
		<section className="plate grid gap-6 p-7 sm:p-8 lg:grid-cols-[auto_minmax(0,1fr)] lg:items-center lg:gap-12">
			<div className="space-y-3">
				<p className="eyebrow">API value · {range}</p>
				<MeterRegister nano={totals.cost_nano} label="API value" />
				<p className="text-xs text-muted-foreground">
					Calculated with the API prices of each model.
				</p>
			</div>
			<dl className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-4">
				<Marking
					label="Requests"
					note={
						totals.errors > 0
							? `${formatCount(totals.errors)} failed`
							: undefined
					}
				>
					{formatCount(totals.requests)}
				</Marking>
				<Marking label="Tokens in">{formatCount(inputTokens(totals))}</Marking>
				<Marking label="Tokens out">
					{formatCount(outputTokens(totals))}
				</Marking>
				<Marking
					label="Reported"
					note={totals.reported_cost_nano > 0 ? "by providers" : undefined}
				>
					{totals.reported_cost_nano > 0
						? formatUsd(totals.reported_cost_nano)
						: "—"}
				</Marking>
				{notes.length > 0 && (
					<p className="col-span-full flex items-start gap-2 border-t border-dashed pt-3 text-xs text-muted-foreground">
						<CircleAlertIcon className="mt-px size-3.5 shrink-0 text-meter" />
						<span>
							{notes.join(" · ")}.{" "}
							<Link to="/pricing" className="text-foreground underline">
								Check the prices
							</Link>
						</span>
					</p>
				)}
			</dl>
		</section>
	);
}

function Marking({
	label,
	note,
	children,
}: {
	label: string;
	note?: string;
	children: React.ReactNode;
}) {
	return (
		<div className="min-w-0 space-y-1">
			<dt className="eyebrow">{label}</dt>
			<dd className="truncate font-mono text-lg tabular-nums">{children}</dd>
			{note && <dd className="text-xs text-muted-foreground">{note}</dd>}
		</div>
	);
}

/** The largest groups by API value, with their share as a bar. */
function TopList({
	title,
	groups,
	total,
	usage,
}: {
	title: string;
	groups: StatsGroup[] | undefined;
	total: number;
	/** The Usage page with the same range and grouping. */
	usage: { range: RangeKey; from?: string; to?: string; group: GroupKey };
}) {
	const top = (groups ?? []).slice(0, 6);
	return (
		<section className="rounded-xl border bg-card">
			<div className="flex items-center justify-between border-b px-5 py-3">
				<h2 className="font-semibold">{title}</h2>
				<Link
					to="/usage"
					search={usage}
					className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
				>
					All usage <ArrowRightIcon className="size-3" />
				</Link>
			</div>
			<ul className="divide-y">
				{top.map((group) => {
					const share = total > 0 ? group.totals.cost_nano / total : 0;
					return (
						<li key={group.key} className="space-y-1.5 px-5 py-2.5">
							<div className="flex items-baseline justify-between gap-3 text-sm">
								<span className="min-w-0 truncate">{group.label}</span>
								<span className="shrink-0 font-mono text-xs tabular-nums">
									{formatUsd(group.totals.cost_nano)}
									<span className="text-muted-foreground">
										{" "}
										· {formatCount(group.totals.requests)} req
									</span>
								</span>
							</div>
							<div aria-hidden="true" className="h-1 rounded-full bg-muted">
								<div
									className="h-full rounded-full bg-primary/70"
									style={{ width: `${Math.max(share * 100, 0.5)}%` }}
								/>
							</div>
						</li>
					);
				})}
			</ul>
		</section>
	);
}

function Accounts() {
	const { data: accounts = [] } = useQuery(accountsQuery);
	const { data: settings } = useQuery(settingsQuery);
	const failover = settings?.failover.enabled ?? false;
	return (
		<section className="rounded-xl border bg-card">
			<div className="flex items-center justify-between border-b px-5 py-3">
				<h2 className="font-semibold">Subscription limits</h2>
				<Link
					to="/subscriptions"
					className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
				>
					Subscriptions <ArrowRightIcon className="size-3" />
				</Link>
			</div>
			{accounts.length === 0 ? (
				<p className="px-5 py-4 text-sm text-muted-foreground">
					No ChatGPT account is linked.
				</p>
			) : (
				<ul className="divide-y">
					{accounts.map((account) => (
						<li key={account.id}>
							<p className="px-4 pt-3 text-sm font-medium">
								{account.label ?? account.email ?? `Account ${account.id}`}
							</p>
							<QuotaMeters
								account={account}
								threshold={
									failover
										? (settings?.failover.threshold_percent ?? null)
										: null
								}
							/>
						</li>
					))}
				</ul>
			)}
		</section>
	);
}

/** Things that stop requests or make costs unknown. */
function Attention() {
	const { data: accounts = [] } = useQuery(accountsQuery);
	const { data: sources = [] } = useQuery(priceSourcesQuery);
	const { data: prices = [] } = useQuery(modelPricesQuery);
	const unpriced = prices.filter((model) => !model.price);
	const items: {
		key: string;
		text: string;
		to: "/subscriptions" | "/pricing";
	}[] = [
		...accounts
			.filter((a) => a.status === "needs_relogin")
			.map((a) => ({
				key: `relogin-${a.id}`,
				text: `${a.label ?? a.email ?? `Account ${a.id}`} needs a new login.`,
				to: "/subscriptions" as const,
			})),
		...accounts
			.filter((a) => (a.limited_until ?? 0) > Date.now() / 1000)
			.map((a) => ({
				key: `limited-${a.id}`,
				text: `${a.label ?? a.email ?? `Account ${a.id}`} reached its usage limit until ${formatDateTime(a.limited_until ?? 0)}.`,
				to: "/subscriptions" as const,
			})),
		...sources
			.filter((s) => s.status === "error")
			.map((s) => ({
				key: `source-${s.source}`,
				text: `The ${SOURCE_NAMES[s.source] ?? s.source} price list did not load.`,
				to: "/pricing" as const,
			})),
		...(unpriced.length > 0
			? [
					{
						key: "unpriced",
						text: `${unpriced.length} enabled ${unpriced.length === 1 ? "model has" : "models have"} no price: ${unpriced
							.slice(0, 3)
							.map((m) => m.model)
							.join(", ")}${unpriced.length > 3 ? " …" : ""}`,
						to: "/pricing" as const,
					},
				]
			: []),
	];
	return (
		<section className="rounded-xl border bg-card">
			<h2 className="border-b px-5 py-3 font-semibold">Needs attention</h2>
			{items.length === 0 ? (
				<p className="px-5 py-4 text-sm text-muted-foreground">
					Nothing needs attention.
				</p>
			) : (
				<ul className="divide-y">
					{items.map((item) => (
						<li key={item.key}>
							<Link
								to={item.to}
								className="flex items-start gap-2.5 px-5 py-3 text-sm hover:bg-muted/60"
							>
								<CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-meter" />
								<span className="min-w-0 flex-1">{item.text}</span>
								<ArrowRightIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
							</Link>
						</li>
					))}
				</ul>
			)}
		</section>
	);
}
