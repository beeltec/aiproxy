import { useForm } from "@tanstack/react-form";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { MoreHorizontalIcon, PlusIcon, RefreshCwIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { PageHeader } from "#/components/page-header";
import { TextField } from "#/components/text-field";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "#/components/ui/alert-dialog";
import { Badge } from "#/components/ui/badge";
import { Button } from "#/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "#/components/ui/dialog";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "#/components/ui/dropdown-menu";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "#/components/ui/field";
import { Input } from "#/components/ui/input";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "#/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "#/components/ui/tabs";
import { api, call, errorMessage, type Schemas } from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";
import {
	modelPricesQuery,
	overridesQuery,
	priceSourcesQuery,
	recomputeQuery,
	SOURCE_NAMES,
} from "#/lib/pricing";
import { formatCount } from "#/lib/stats";
import { cn } from "#/lib/utils";

export const Route = createFileRoute("/_app/pricing")({
	loader: ({ context }) =>
		Promise.all([
			context.queryClient.query({ ...priceSourcesQuery, staleTime: "static" }),
			context.queryClient.query({ ...modelPricesQuery, staleTime: "static" }),
		]),
	component: PricingPage,
});

type Prices = Schemas["Prices"];
type ModelPrice = Schemas["ModelPrice"];
type Override = Schemas["OverrideView"];

const price = new Intl.NumberFormat("en-US", {
	style: "currency",
	currency: "USD",
	maximumFractionDigits: 4,
});

/** USD per token as USD per 1M tokens. */
function perMillion(usd: number | null | undefined): string {
	return usd === null || usd === undefined ? "—" : price.format(usd * 1e6);
}

function PricingPage() {
	const queryClient = useQueryClient();
	const sync = useMutation({
		mutationFn: () => call(api.POST("/pricing/sync")),
		onSuccess: (sources) => {
			queryClient.setQueryData(priceSourcesQuery.queryKey, sources);
			void queryClient.invalidateQueries({ queryKey: ["pricing"] });
			toast.success("The price lists are loaded.");
		},
		onError: (error) => toast.error(errorMessage(error)),
	});
	return (
		<div className="space-y-8">
			<PageHeader
				title="Pricing"
				description="The API prices that give each request its value. They come from public price lists; an override replaces them for one model."
				actions={
					<Button
						variant="outline"
						disabled={sync.isPending}
						onClick={() => sync.mutate()}
					>
						<RefreshCwIcon className={cn(sync.isPending && "animate-spin")} />
						{sync.isPending ? "Loading prices…" : "Load prices now"}
					</Button>
				}
			/>
			<Sources />
			<Tabs defaultValue="models">
				<TabsList>
					<TabsTrigger value="models">Model prices</TabsTrigger>
					<TabsTrigger value="overrides">Overrides</TabsTrigger>
					<TabsTrigger value="recompute">Recompute</TabsTrigger>
				</TabsList>
				<TabsContent value="models" className="pt-4">
					<ModelPrices />
				</TabsContent>
				<TabsContent value="overrides" className="pt-4">
					<Overrides />
				</TabsContent>
				<TabsContent value="recompute" className="pt-4">
					<Recompute />
				</TabsContent>
			</Tabs>
		</div>
	);
}

function Sources() {
	const { data: sources = [] } = useQuery(priceSourcesQuery);
	return (
		<div className="grid gap-4 sm:grid-cols-3">
			{["litellm", "models_dev", "openrouter"].map((name) => {
				const source = sources.find((s) => s.source === name);
				const failed = source?.status === "error";
				return (
					<section
						key={name}
						className="space-y-2 rounded-xl border bg-card px-5 py-4"
					>
						<div className="flex items-center justify-between gap-2">
							<h2 className="font-semibold">{SOURCE_NAMES[name]}</h2>
							<span className="flex items-center gap-1.5 text-xs text-muted-foreground">
								<span
									aria-hidden="true"
									className={cn(
										"size-2 rounded-full",
										!source
											? "bg-muted-foreground/40"
											: failed
												? "bg-destructive"
												: "bg-ok",
									)}
								/>
								{!source ? "Not loaded" : failed ? "Failed" : "Loaded"}
							</span>
						</div>
						<p className="font-mono text-xs text-muted-foreground">
							{source ? `${formatCount(source.entries)} models` : "—"}
							{source?.fetched_at && ` · ${formatRelative(source.fetched_at)}`}
						</p>
						{failed && source?.error && (
							<p className="text-xs break-words text-destructive">
								{source.error}
							</p>
						)}
					</section>
				);
			})}
		</div>
	);
}

/** Short names for the price rules beyond the standard token prices. */
function extras(prices: Prices): string[] {
	const out: string[] = [];
	if (prices.variable) out.push("variable");
	if (prices.priority) out.push("priority");
	if (prices.flex) out.push("flex");
	if ((prices.context_tiers ?? []).length > 0) out.push("long context");
	if (prices.fast || prices.fast_multiplier) out.push("fast");
	if (Object.keys(prices.geo ?? {}).length > 0) out.push("geography");
	if (prices.web_search || prices.web_search_preview) out.push("web search");
	if ((prices.images ?? []).length > 0) out.push("per image");
	if (prices.per_character) out.push("per character");
	if (prices.per_second) out.push("per second");
	if (prices.partial) out.push("partial");
	return out;
}

type Editing =
	| { kind: "new"; matchKey?: string; prices?: Prices }
	| { kind: "edit"; override: Override };

function ModelPrices() {
	const { data: models = [] } = useQuery(modelPricesQuery);
	const { data: overrides = [] } = useQuery(overridesQuery);
	const [editing, setEditing] = useState<Editing | null>(null);
	const edit = (model: ModelPrice) => {
		const own = overrides.find((o) => o.match_key === model.price?.key);
		if (model.price?.source === "override" && own)
			setEditing({ kind: "edit", override: own });
		else
			setEditing({
				kind: "new",
				matchKey: model.model,
				prices: model.price?.prices,
			});
	};
	return (
		<section className="rounded-xl border bg-card">
			<p className="border-b px-5 py-3 text-sm text-muted-foreground">
				The prices that apply to each enabled model now, per 1M tokens.
			</p>
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Model</TableHead>
						<TableHead>From</TableHead>
						<TableHead className="text-right">Input</TableHead>
						<TableHead className="text-right">Cached</TableHead>
						<TableHead className="text-right">Output</TableHead>
						<TableHead>Also</TableHead>
						<TableHead className="w-10">
							<span className="sr-only">Actions</span>
						</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{models.map((model) => {
						const standard = model.price?.prices.standard;
						return (
							<TableRow key={model.model}>
								<TableCell className="max-w-72 truncate font-mono text-xs">
									{model.model}
								</TableCell>
								<TableCell>
									{model.price ? (
										<Badge
											variant={
												model.price.source === "override"
													? "default"
													: "secondary"
											}
										>
											{SOURCE_NAMES[model.price.source] ?? model.price.source}
										</Badge>
									) : (
										<span className="text-xs text-meter">No price</span>
									)}
								</TableCell>
								<TableCell className="text-right font-mono text-xs tabular-nums">
									{perMillion(standard?.input_text)}
								</TableCell>
								<TableCell className="text-right font-mono text-xs tabular-nums">
									{perMillion(standard?.input_text_cached)}
								</TableCell>
								<TableCell className="text-right font-mono text-xs tabular-nums">
									{perMillion(standard?.output_text)}
								</TableCell>
								<TableCell className="text-xs text-muted-foreground">
									{model.price
										? extras(model.price.prices).join(", ") || "—"
										: "—"}
								</TableCell>
								<TableCell>
									<Button
										variant="ghost"
										size="sm"
										onClick={() => edit(model)}
										aria-label={`Set the price of ${model.model}`}
									>
										{model.price?.source === "override" ? "Edit" : "Override"}
									</Button>
								</TableCell>
							</TableRow>
						);
					})}
				</TableBody>
			</Table>
			<OverrideDialog editing={editing} onClose={() => setEditing(null)} />
		</section>
	);
}

function Overrides() {
	const { data: overrides = [] } = useQuery(overridesQuery);
	const [editing, setEditing] = useState<Editing | null>(null);
	const [deleting, setDeleting] = useState<Override | null>(null);
	return (
		<section className="space-y-4">
			<div className="flex flex-wrap items-center justify-between gap-3">
				<p className="max-w-prose text-sm text-muted-foreground">
					An override applies to new requests at once. Stored requests keep
					their old price until you recompute them.
				</p>
				<Button onClick={() => setEditing({ kind: "new" })}>
					<PlusIcon />
					Add override
				</Button>
			</div>
			{overrides.length === 0 ? (
				<p className="rounded-xl border border-dashed px-6 py-10 text-center text-sm text-muted-foreground">
					No overrides. All models use the prices of the public lists.
				</p>
			) : (
				<ul className="divide-y rounded-xl border bg-card">
					{overrides.map((override) => (
						<li key={override.id} className="flex items-center gap-4 px-5 py-3">
							<div className="min-w-0 flex-1 space-y-0.5">
								<p className="truncate font-mono text-sm">
									{override.match_key}
								</p>
								<p className="text-xs text-muted-foreground">
									Input {perMillion(override.prices.standard.input_text)} ·
									Output {perMillion(override.prices.standard.output_text)} per
									1M · {override.created_by ?? "An admin"},{" "}
									{formatDateTime(override.created_at)}
								</p>
							</div>
							<DropdownMenu>
								<DropdownMenuTrigger
									render={
										<Button
											variant="ghost"
											size="icon-sm"
											aria-label={`Actions for ${override.match_key}`}
										>
											<MoreHorizontalIcon />
										</Button>
									}
								/>
								<DropdownMenuContent align="end">
									<DropdownMenuItem
										onClick={() => setEditing({ kind: "edit", override })}
									>
										Edit prices
									</DropdownMenuItem>
									<DropdownMenuItem
										variant="destructive"
										onClick={() => setDeleting(override)}
									>
										Delete override
									</DropdownMenuItem>
								</DropdownMenuContent>
							</DropdownMenu>
						</li>
					))}
				</ul>
			)}
			<OverrideDialog editing={editing} onClose={() => setEditing(null)} />
			<DeleteOverride override={deleting} onClose={() => setDeleting(null)} />
		</section>
	);
}

/** The price fields of the form. Token prices are per 1M tokens; `scale` turns the value into USD per unit. */
const TOKEN_FIELDS = [
	{ key: "input_text", label: "Input" },
	{ key: "input_text_cached", label: "Cached input" },
	{ key: "cache_write_5m", label: "Cache write (5 min)" },
	{ key: "cache_write_1h", label: "Cache write (1 hour)" },
	{ key: "input_audio", label: "Audio input" },
	{ key: "input_audio_cached", label: "Cached audio input" },
	{ key: "input_image", label: "Image input" },
	{ key: "input_image_cached", label: "Cached image input" },
	{ key: "output_text", label: "Output" },
	{ key: "output_reasoning", label: "Reasoning" },
	{ key: "output_audio", label: "Audio output" },
	{ key: "output_image", label: "Image output" },
] as const;

const UNIT_FIELDS = [
	{ key: "web_search", label: "Web search, per 1,000 calls", scale: 1000 },
	{
		key: "web_search_preview",
		label: "Preview web search, per 1,000 calls",
		scale: 1000,
	},
	{ key: "per_character", label: "Speech, per 1M characters", scale: 1e6 },
	{ key: "per_second", label: "Transcription, per minute", scale: 60 },
] as const;

type TokenKey = (typeof TOKEN_FIELDS)[number]["key"];
type UnitKey = (typeof UNIT_FIELDS)[number]["key"];
type PriceKey = TokenKey | UnitKey;

const amount = z
	.string()
	.trim()
	.refine(
		(v) => v === "" || (Number.isFinite(Number(v)) && Number(v) >= 0),
		"Enter a price of 0 or more.",
	);

const overrideSchema = z.object({
	match_key: z
		.string()
		.trim()
		.min(1, "Enter a model.")
		.max(200, "Use at most 200 characters.")
		.regex(/^\S+\/\S+$/, "Use a name such as openai/gpt-5 or chatgpt/gpt-5.5."),
	...(Object.fromEntries(
		[...TOKEN_FIELDS, ...UNIT_FIELDS].map((f) => [f.key, amount]),
	) as Record<PriceKey, typeof amount>),
});

/** A price as the text of its form field (per 1M tokens, or per the unit of the field). */
function toField(usd: number | null | undefined, scale: number): string {
	if (usd === null || usd === undefined) return "";
	return String(Number((usd * scale).toPrecision(10)));
}

function fromField(text: string, scale: number): number | undefined {
	return text.trim() === "" ? undefined : Number(text) / scale;
}

function OverrideDialog({
	editing,
	onClose,
}: {
	editing: Editing | null;
	onClose: () => void;
}) {
	return (
		<Dialog open={editing !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
				{editing && (
					<OverrideForm
						key={
							editing.kind === "edit"
								? editing.override.id
								: (editing.matchKey ?? "new")
						}
						editing={editing}
						onClose={onClose}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}

function OverrideForm({
	editing,
	onClose,
}: {
	editing: Editing;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const base: Prices =
		editing.kind === "edit"
			? editing.override.prices
			: (editing.prices ?? { standard: {} });
	const save = useMutation({
		mutationFn: (value: z.infer<typeof overrideSchema>) => {
			// Tier, context and image prices of the base stay as they are.
			const standard = { ...base.standard };
			for (const f of TOKEN_FIELDS)
				standard[f.key] = fromField(value[f.key], 1e6);
			const prices: Prices = { ...base, standard };
			for (const f of UNIT_FIELDS)
				prices[f.key] = fromField(value[f.key], f.scale);
			if (editing.kind === "edit") {
				return call(
					api.PUT("/pricing/overrides/{id}", {
						params: { path: { id: editing.override.id } },
						body: { prices },
					}),
				);
			}
			return call(
				api.POST("/pricing/overrides", {
					body: { match_key: value.match_key, prices },
				}),
			);
		},
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: ["pricing"] });
			toast.success(
				editing.kind === "edit"
					? "The override is saved."
					: "The override is added.",
			);
			onClose();
		},
	});
	const form = useForm({
		defaultValues: {
			match_key:
				editing.kind === "edit"
					? editing.override.match_key
					: (editing.matchKey ?? ""),
			...(Object.fromEntries([
				...TOKEN_FIELDS.map((f) => [f.key, toField(base.standard[f.key], 1e6)]),
				...UNIT_FIELDS.map((f) => [f.key, toField(base[f.key], f.scale)]),
			]) as Record<PriceKey, string>),
		},
		validators: { onSubmit: overrideSchema },
		onSubmit: ({ value }) => save.mutateAsync(value).catch(() => undefined),
	});
	const kept = extras({
		...base,
		web_search: null,
		web_search_preview: null,
		per_character: null,
		per_second: null,
	});

	return (
		<>
			<DialogHeader>
				<DialogTitle>
					{editing.kind === "edit"
						? `Prices of ${editing.override.match_key}`
						: "Add override"}
				</DialogTitle>
				<DialogDescription>
					Enter prices in USD. An empty field has no price, so its usage makes
					the cost incomplete.
				</DialogDescription>
			</DialogHeader>
			<form
				noValidate
				onSubmit={(e) => {
					e.preventDefault();
					void form.handleSubmit();
				}}
			>
				<FieldGroup>
					{editing.kind === "new" && (
						<form.Field name="match_key">
							{(field) => (
								<TextField
									field={field}
									label="Model"
									autoComplete="off"
									description="A model as clients use it (chatgpt/gpt-5.5, anthropic/claude-sonnet-5) or a list key (openai/gpt-5) for all connections."
								/>
							)}
						</form.Field>
					)}
					<FieldSet>
						<FieldLegend variant="label">Tokens, per 1M</FieldLegend>
						<div className="grid gap-3 sm:grid-cols-3">
							{TOKEN_FIELDS.map((f) => (
								<form.Field key={f.key} name={f.key}>
									{(field) => <PriceInput field={field} label={f.label} />}
								</form.Field>
							))}
						</div>
						<FieldDescription>
							Reasoning without a price costs like output.
						</FieldDescription>
					</FieldSet>
					<FieldSet>
						<FieldLegend variant="label">Other units</FieldLegend>
						<div className="grid gap-3 sm:grid-cols-2">
							{UNIT_FIELDS.map((f) => (
								<form.Field key={f.key} name={f.key}>
									{(field) => <PriceInput field={field} label={f.label} />}
								</form.Field>
							))}
						</div>
					</FieldSet>
					{kept.length > 0 && (
						<p className="text-xs text-muted-foreground">
							These rules of the copied price stay as they are:{" "}
							{kept.join(", ")}.
						</p>
					)}
					{save.error && <FieldError>{save.error.message}</FieldError>}
				</FieldGroup>
				<DialogFooter className="mt-6">
					<Button type="button" variant="outline" onClick={onClose}>
						Cancel
					</Button>
					<Button type="submit" disabled={save.isPending}>
						{editing.kind === "edit" ? "Save prices" : "Add override"}
					</Button>
				</DialogFooter>
			</form>
		</>
	);
}

function PriceInput({
	field,
	label,
}: {
	field: {
		name: string;
		state: {
			value: string;
			meta: { isTouched: boolean; isValid: boolean; errors: unknown[] };
		};
		handleBlur: () => void;
		handleChange: (value: string) => void;
	};
	label: string;
}) {
	const invalid = field.state.meta.isTouched && !field.state.meta.isValid;
	return (
		<Field data-invalid={invalid || undefined}>
			<FieldLabel htmlFor={field.name} className="text-xs">
				{label}
			</FieldLabel>
			<div className="relative">
				<span className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-sm text-muted-foreground">
					$
				</span>
				<Input
					id={field.name}
					inputMode="decimal"
					className="pl-6 font-mono text-xs"
					value={field.state.value}
					onBlur={field.handleBlur}
					onChange={(e) => field.handleChange(e.target.value)}
					aria-invalid={invalid || undefined}
				/>
			</div>
			{invalid && (
				<FieldError
					errors={field.state.meta.errors as { message?: string }[]}
				/>
			)}
		</Field>
	);
}

function DeleteOverride({
	override,
	onClose,
}: {
	override: Override | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/pricing/overrides/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: ["pricing"] });
			toast.success("The override is deleted.");
			onClose();
		},
		onError: (error) => toast.error(errorMessage(error)),
	});
	return (
		<AlertDialog
			open={override !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>
						Delete the override of {override?.match_key}?
					</AlertDialogTitle>
					<AlertDialogDescription>
						New requests use the prices of the public lists again. Stored
						requests keep this price until you recompute them.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={() => override && remove.mutate(override.id)}
					>
						Delete override
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

/** The local date of unix seconds, as the value of a date input. */
function dateValue(seconds: number): string {
	const date = new Date(seconds * 1000);
	const pad = (n: number) => String(n).padStart(2, "0");
	return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

function Recompute() {
	const queryClient = useQueryClient();
	const { data: status } = useQuery(recomputeQuery);
	const today = Math.floor(Date.now() / 1000);
	const [from, setFrom] = useState(dateValue(today - 29 * 86_400));
	const [to, setTo] = useState(dateValue(today));
	const start = useMutation({
		mutationFn: () => {
			// Local midnights; the end date is included.
			const begin = new Date(`${from}T00:00`).getTime() / 1000;
			const end = new Date(`${to}T00:00`).getTime() / 1000 + 86_400;
			return call(
				api.POST("/pricing/recompute", { body: { from: begin, to: end } }),
			);
		},
		onSuccess: (next) => {
			queryClient.setQueryData(recomputeQuery.queryKey, next);
			toast.success("The recompute has started.");
		},
		onError: (error) => toast.error(errorMessage(error)),
	});
	const done = status && status.total > 0 ? status.done / status.total : 0;

	return (
		<section className="grid gap-6 rounded-xl border bg-card p-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
			<form
				className="space-y-4"
				onSubmit={(e) => {
					e.preventDefault();
					start.mutate();
				}}
			>
				<p className="text-sm text-muted-foreground">
					Calculates the API value of stored requests again with the current
					prices. Use it after you add or change an override, or for requests
					from before a price was known.
				</p>
				<div className="grid gap-3 sm:grid-cols-2">
					<Field>
						<FieldLabel htmlFor="recompute-from">From</FieldLabel>
						<Input
							id="recompute-from"
							type="date"
							value={from}
							max={to}
							onChange={(e) => setFrom(e.target.value)}
						/>
					</Field>
					<Field>
						<FieldLabel htmlFor="recompute-to">To (included)</FieldLabel>
						<Input
							id="recompute-to"
							type="date"
							value={to}
							min={from}
							onChange={(e) => setTo(e.target.value)}
						/>
					</Field>
				</div>
				<Button
					type="submit"
					disabled={start.isPending || status?.running || !from || !to}
				>
					Recompute costs
				</Button>
			</form>
			<div className="space-y-3 lg:border-l lg:pl-6">
				<p className="eyebrow">Last recompute</p>
				{!status?.started_at ? (
					<p className="text-sm text-muted-foreground">
						No recompute has run since the server started.
					</p>
				) : (
					<>
						<div className="flex items-baseline justify-between gap-2 font-mono text-xs tabular-nums">
							<span>
								{formatCount(status.done)} of {formatCount(status.total)} rows
							</span>
							<span className="text-muted-foreground">
								{status.running
									? "Running…"
									: status.error
										? "Failed"
										: `Done ${formatRelative(status.finished_at ?? status.started_at)}`}
							</span>
						</div>
						<div
							role="progressbar"
							aria-label="Recompute progress"
							aria-valuemin={0}
							aria-valuemax={100}
							aria-valuenow={Math.round(done * 100)}
							className="h-1.5 rounded-full bg-muted"
						>
							<div
								className="h-full rounded-full bg-meter transition-[width]"
								style={{ width: `${done * 100}%` }}
							/>
						</div>
						{status.from && status.to && (
							<p className="text-xs text-muted-foreground">
								Requests from {formatDateTime(status.from)} to{" "}
								{formatDateTime(status.to)}.
							</p>
						)}
						{status.error && (
							<p className="text-xs text-destructive">{status.error}</p>
						)}
					</>
				)}
			</div>
		</section>
	);
}
