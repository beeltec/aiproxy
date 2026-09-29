import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import {
	MoreHorizontalIcon,
	PlusIcon,
	RefreshCwIcon,
	SearchIcon,
	SlidersHorizontalIcon,
} from "lucide-react";
import { useId, useMemo, useState } from "react";
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
import { Checkbox } from "#/components/ui/checkbox";
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
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "#/components/ui/dropdown-menu";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
} from "#/components/ui/field";
import { Input } from "#/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "#/components/ui/select";
import { Switch } from "#/components/ui/switch";
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
import { modelsQuery } from "#/lib/models";

type Connection = Schemas["ConnectionView"];
type Model = Schemas["ModelView"];
type Capabilities = Schemas["Capabilities"];
type Overrides = Schemas["CapabilityOverrides"];

const connectionsQuery = queryOptions({
	queryKey: ["connections"],
	queryFn: () => call(api.GET("/connections")),
});

export const Route = createFileRoute("/_app/connections")({
	loader: ({ context }) =>
		Promise.all([
			context.queryClient.query({ ...connectionsQuery, staleTime: "static" }),
			context.queryClient.query({ ...modelsQuery, staleTime: "static" }),
		]),
	component: ConnectionsPage,
});

const KINDS = {
	openai: { label: "OpenAI", url: "https://api.openai.com/v1" },
	anthropic: { label: "Anthropic", url: "https://api.anthropic.com/v1" },
	openrouter: { label: "OpenRouter", url: "https://openrouter.ai/api/v1" },
} as const;
type Kind = keyof typeof KINDS;

function kindLabel(kind: string): string {
	return kind === "chatgpt" ? "ChatGPT" : (KINDS[kind as Kind]?.label ?? kind);
}

function ConnectionsPage() {
	const { data: connections = [] } = useQuery(connectionsQuery);
	const [editing, setEditing] = useState<Connection | "new" | null>(null);
	const [deleting, setDeleting] = useState<Connection | null>(null);

	return (
		<div className="space-y-6">
			<PageHeader
				title="Connections"
				description="API keys of other providers. Their models get the connection name as prefix, for example anthropic/claude-opus-5-5."
				actions={
					<Button onClick={() => setEditing("new")}>
						<PlusIcon />
						Add connection
					</Button>
				}
			/>
			<Tabs defaultValue="connections">
				<TabsList>
					<TabsTrigger value="connections">Connections</TabsTrigger>
					<TabsTrigger value="models">Models</TabsTrigger>
				</TabsList>
				<TabsContent value="connections" className="pt-4">
					{connections.length === 0 ? (
						<div className="rounded-xl border border-dashed px-6 py-14 text-center">
							<p className="font-medium">No connections yet</p>
							<p className="mx-auto mt-1 max-w-sm text-sm text-muted-foreground">
								Add an OpenAI, Anthropic or OpenRouter API key. Then enable the
								models that clients may use in the Models tab.
							</p>
						</div>
					) : (
						<ul className="space-y-3">
							{connections.map((connection) => (
								<ConnectionCard
									key={connection.id}
									connection={connection}
									onEdit={() => setEditing(connection)}
									onDelete={() => setDeleting(connection)}
								/>
							))}
						</ul>
					)}
				</TabsContent>
				<TabsContent value="models" className="pt-4">
					<ModelsTab />
				</TabsContent>
			</Tabs>
			<ConnectionDialog target={editing} onClose={() => setEditing(null)} />
			<DeleteDialog connection={deleting} onClose={() => setDeleting(null)} />
		</div>
	);
}

function Fact({
	label,
	children,
}: {
	label: string;
	children: React.ReactNode;
}) {
	return (
		<div className="min-w-0 space-y-0.5">
			<dt className="eyebrow">{label}</dt>
			<dd className="truncate">{children}</dd>
		</div>
	);
}

function ConnectionCard({
	connection,
	onEdit,
	onDelete,
}: {
	connection: Connection;
	onEdit: () => void;
	onDelete: () => void;
}) {
	const queryClient = useQueryClient();
	const sync = useMutation({
		mutationFn: () =>
			call(
				api.POST("/connections/{id}/sync", {
					params: { path: { id: connection.id } },
				}),
			),
		onSuccess: (updated) => {
			toast.success(`${updated.models} models loaded.`);
		},
		onError: (error) => toast.error(errorMessage(error)),
		onSettled: () => {
			void queryClient.invalidateQueries({
				queryKey: connectionsQuery.queryKey,
			});
			void queryClient.invalidateQueries({ queryKey: modelsQuery.queryKey });
		},
	});

	return (
		<li className="rounded-xl border bg-card">
			<div className="flex flex-wrap items-start justify-between gap-3 p-4">
				<div className="min-w-0 space-y-1">
					<div className="flex flex-wrap items-center gap-2">
						<span className="truncate font-medium">
							{connection.display_name}
						</span>
						{connection.display_name !== kindLabel(connection.kind) && (
							<Badge variant="outline">{kindLabel(connection.kind)}</Badge>
						)}
						{connection.last_error && (
							<Badge variant="destructive">Model list failed</Badge>
						)}
					</div>
					<p className="font-mono text-xs text-muted-foreground">
						<span className="text-foreground">{connection.slug}</span>/…
					</p>
				</div>
				<div className="flex items-center gap-1">
					<Button
						size="sm"
						variant="outline"
						disabled={sync.isPending}
						onClick={() => sync.mutate()}
					>
						<RefreshCwIcon
							className={sync.isPending ? "animate-spin" : undefined}
						/>
						Load models
					</Button>
					<DropdownMenu>
						<DropdownMenuTrigger
							render={
								<Button
									variant="ghost"
									size="icon-sm"
									aria-label={`Actions for ${connection.display_name}`}
								/>
							}
						>
							<MoreHorizontalIcon />
						</DropdownMenuTrigger>
						<DropdownMenuContent align="end" className="w-44">
							<DropdownMenuItem onClick={onEdit}>
								Name, key and URL
							</DropdownMenuItem>
							<DropdownMenuSeparator />
							<DropdownMenuItem variant="destructive" onClick={onDelete}>
								Delete
							</DropdownMenuItem>
						</DropdownMenuContent>
					</DropdownMenu>
				</div>
			</div>
			<dl className="grid grid-cols-2 gap-x-6 gap-y-3 border-t px-4 py-3 text-sm sm:grid-cols-4">
				<Fact label="Models">
					<span className="font-mono tabular-nums">
						{connection.enabled_models}
					</span>
					<span className="text-muted-foreground">
						{" "}
						of {connection.models} enabled
					</span>
				</Fact>
				<Fact label="API key">
					<span className="font-mono text-xs">
						••••{connection.api_key_last4}
					</span>
				</Fact>
				<Fact label="Endpoint">
					<span
						className="font-mono text-xs"
						title={connection.base_url ?? KINDS[connection.kind as Kind]?.url}
					>
						{connection.base_url
							? new URL(connection.base_url).host
							: "Official"}
					</span>
				</Fact>
				<Fact label="Last model list">
					{connection.last_sync_at ? (
						<span title={formatDateTime(connection.last_sync_at)}>
							{formatRelative(connection.last_sync_at)}
						</span>
					) : (
						"Never"
					)}
				</Fact>
			</dl>
			{connection.last_error && (
				<p className="border-t px-4 py-3 text-sm break-words text-destructive">
					{connection.last_error}
				</p>
			)}
		</li>
	);
}

// ---------------------------------------------------------------------------------------------

const SLUG = /^[a-z0-9][a-z0-9-]{0,39}$/;

const connectionSchema = z.object({
	kind: z.string(),
	slug: z.string().trim(),
	display_name: z
		.string()
		.trim()
		.min(1, "Enter a name.")
		.max(100, "Use at most 100 characters."),
	api_key: z.string().trim(),
	base_url: z
		.string()
		.trim()
		.refine(
			(v) => v === "" || /^https?:\/\/\S+$/.test(v),
			"Enter a URL that starts with https://, or leave it empty.",
		),
});

function ConnectionDialog({
	target,
	onClose,
}: {
	target: Connection | "new" | null;
	onClose: () => void;
}) {
	const current = target === "new" ? null : target;
	return (
		<Dialog open={target !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
				{target !== null && (
					<ConnectionForm
						key={current?.id ?? "new"}
						current={current}
						onClose={onClose}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}

function ConnectionForm({
	current,
	onClose,
}: {
	current: Connection | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const save = useMutation({
		mutationFn: async (value: z.infer<typeof connectionSchema>) => {
			const base_url = value.base_url === "" ? null : value.base_url;
			if (current) {
				return call(
					api.PATCH("/connections/{id}", {
						params: { path: { id: current.id } },
						body: {
							display_name: value.display_name,
							api_key: value.api_key || null,
							base_url,
						},
					}),
				);
			}
			return call(
				api.POST("/connections", {
					body: {
						kind: value.kind,
						slug: value.slug,
						display_name: value.display_name,
						api_key: value.api_key,
						base_url,
					},
				}),
			);
		},
		onSuccess: (saved) => {
			void queryClient.invalidateQueries({
				queryKey: connectionsQuery.queryKey,
			});
			void queryClient.invalidateQueries({ queryKey: modelsQuery.queryKey });
			if (current) toast.success("The connection is saved.");
			else if (saved.last_error)
				toast.warning("The connection is saved, but its model list failed.");
			else
				toast.success(
					`The connection is added with ${saved.models} models. Enable them in the Models tab.`,
				);
			onClose();
		},
	});
	const form = useForm({
		defaultValues: {
			kind: current?.kind ?? "anthropic",
			slug: current?.slug ?? "anthropic",
			display_name: current?.display_name ?? "Anthropic",
			api_key: "",
			base_url: current?.base_url ?? "",
		},
		validators: {
			onSubmit: connectionSchema.superRefine((value, ctx) => {
				if (!current && !SLUG.test(value.slug)) {
					ctx.addIssue({
						code: "custom",
						path: ["slug"],
						message:
							"Use 1 to 40 lower-case letters, digits or -, starting with a letter or digit.",
					});
				}
				if (!current && value.slug === "chatgpt") {
					ctx.addIssue({
						code: "custom",
						path: ["slug"],
						message: "The prefix chatgpt is reserved.",
					});
				}
				if (!current && value.api_key === "") {
					ctx.addIssue({
						code: "custom",
						path: ["api_key"],
						message: "Enter the API key.",
					});
				}
			}),
		},
		onSubmit: ({ value }) => save.mutateAsync(value).catch(() => undefined),
	});
	const kindItems = Object.entries(KINDS).map(([value, { label }]) => ({
		value,
		label,
	}));

	return (
		<>
			<DialogHeader>
				<DialogTitle>
					{current ? `Edit ${current.display_name}` : "Add connection"}
				</DialogTitle>
				<DialogDescription>
					{current
						? "The prefix stays the same, so API key model lists and aliases keep working."
						: "The gateway checks the key by loading the model list. New models start disabled."}
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
					{!current && (
						<form.Field name="kind">
							{(field) => (
								<Field>
									<FieldLabel htmlFor="kind">Provider</FieldLabel>
									<Select
										items={kindItems}
										value={field.state.value}
										onValueChange={(v) => {
											if (!v) return;
											// Suggest a prefix and a name until the admin types their own.
											const previous = KINDS[field.state.value as Kind];
											const next = KINDS[v as Kind];
											if (form.getFieldValue("slug") === field.state.value) {
												form.setFieldValue("slug", v);
											}
											if (
												form.getFieldValue("display_name") === previous?.label
											) {
												form.setFieldValue("display_name", next.label);
											}
											field.handleChange(v);
										}}
									>
										<SelectTrigger id="kind" className="w-full">
											<SelectValue />
										</SelectTrigger>
										<SelectContent>
											{kindItems.map((item) => (
												<SelectItem key={item.value} value={item.value}>
													{item.label}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</Field>
							)}
						</form.Field>
					)}
					{!current && (
						<form.Field name="slug">
							{(field) => (
								<TextField
									field={field}
									label="Model prefix"
									autoComplete="off"
									description="Clients write it before the model id. It cannot change later."
								/>
							)}
						</form.Field>
					)}
					<form.Field name="display_name">
						{(field) => (
							<TextField field={field} label="Name" autoComplete="off" />
						)}
					</form.Field>
					<form.Field name="api_key">
						{(field) => (
							<TextField
								field={field}
								label="API key"
								type="password"
								autoComplete="off"
								description={
									current
										? `Leave empty to keep the key ending in ${current.api_key_last4}.`
										: "Stored encrypted. The dashboard shows only the last 4 characters."
								}
							/>
						)}
					</form.Field>
					<form.Subscribe selector={(s) => s.values.kind}>
						{(kind) => (
							<form.Field name="base_url">
								{(field) => (
									<TextField
										field={field}
										label="Base URL (optional)"
										autoComplete="off"
										description={`Empty uses ${KINDS[kind as Kind]?.url ?? "the official API"}.`}
									/>
								)}
							</form.Field>
						)}
					</form.Subscribe>
					{save.error && <FieldError>{save.error.message}</FieldError>}
					<DialogFooter>
						<Button type="button" variant="outline" onClick={onClose}>
							Cancel
						</Button>
						<form.Subscribe selector={(s) => s.isSubmitting}>
							{(submitting) => (
								<Button type="submit" disabled={submitting}>
									{submitting
										? current
											? "Saving…"
											: "Loading models…"
										: current
											? "Save changes"
											: "Add connection"}
								</Button>
							)}
						</form.Subscribe>
					</DialogFooter>
				</FieldGroup>
			</form>
		</>
	);
}

function DeleteDialog({
	connection,
	onClose,
}: {
	connection: Connection | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/connections/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({
				queryKey: connectionsQuery.queryKey,
			});
			void queryClient.invalidateQueries({ queryKey: modelsQuery.queryKey });
			toast.success("The connection is deleted.");
			onClose();
		},
		onError: (error) => toast.error(errorMessage(error)),
	});
	return (
		<AlertDialog
			open={connection !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>
						Delete {connection?.display_name}?
					</AlertDialogTitle>
					<AlertDialogDescription>
						Its models disappear from the gateway at once. Aliases that point to
						them stop working. The usage history stays.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={() => connection && remove.mutate(connection.id)}
					>
						Delete connection
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

// ---------------------------------------------------------------------------------------------
// Models

const PAGE = 150;

function ModelsTab() {
	const { data: models = [] } = useQuery(modelsQuery);
	const [source, setSource] = useState("all");
	const [search, setSearch] = useState("");
	const [enabledOnly, setEnabledOnly] = useState(false);
	const [shown, setShown] = useState(PAGE);
	const [editing, setEditing] = useState<Model | null>(null);

	const sources = useMemo(() => {
		const list = new Map<string, { kind: string; total: number; on: number }>();
		for (const model of models) {
			const entry = list.get(model.source) ?? {
				kind: model.source_kind,
				total: 0,
				on: 0,
			};
			entry.total += 1;
			if (model.enabled) entry.on += 1;
			list.set(model.source, entry);
		}
		return [...list.entries()];
	}, [models]);
	const filtered = useMemo(() => {
		const words = search.toLowerCase().split(/\s+/).filter(Boolean);
		return models.filter(
			(model) =>
				(source === "all" || model.source === source) &&
				(!enabledOnly || model.enabled) &&
				words.every(
					(w) =>
						model.name.toLowerCase().includes(w) ||
						(model.display_name ?? "").toLowerCase().includes(w),
				),
		);
	}, [models, source, search, enabledOnly]);
	const sourceItems = [
		{ value: "all", label: `All sources (${models.length})` },
		...sources.map(([name, s]) => ({
			value: name,
			label: `${name} · ${s.on} of ${s.total} on`,
		})),
	];

	if (models.length === 0) {
		return (
			<div className="rounded-xl border border-dashed px-6 py-14 text-center">
				<p className="font-medium">No models yet</p>
				<p className="mx-auto mt-1 max-w-sm text-sm text-muted-foreground">
					Link a ChatGPT account or add a connection. Their model lists show up
					here.
				</p>
			</div>
		);
	}

	return (
		<div className="space-y-3">
			<div className="flex flex-wrap items-center gap-2">
				<div className="relative min-w-48 flex-1">
					<SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
					<Input
						aria-label="Search models"
						placeholder="Search models"
						className="pl-8"
						value={search}
						onChange={(e) => {
							setSearch(e.target.value);
							setShown(PAGE);
						}}
					/>
				</div>
				<Select
					items={sourceItems}
					value={source}
					onValueChange={(v) => {
						if (v) setSource(v);
						setShown(PAGE);
					}}
				>
					<SelectTrigger aria-label="Source" className="w-56">
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{sourceItems.map((item) => (
							<SelectItem key={item.value} value={item.value}>
								{item.label}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
				<Field orientation="horizontal" className="w-fit">
					<Switch
						id="enabled-only"
						size="sm"
						checked={enabledOnly}
						onCheckedChange={(checked) => setEnabledOnly(checked)}
					/>
					<FieldLabel htmlFor="enabled-only">Enabled only</FieldLabel>
				</Field>
			</div>
			<div className="overflow-hidden rounded-xl border bg-card">
				<Table>
					<TableHeader>
						<TableRow>
							<TableHead className="w-14 pl-4">On</TableHead>
							<TableHead>Model</TableHead>
							<TableHead className="hidden md:table-cell">Can do</TableHead>
							<TableHead className="w-12">
								<span className="sr-only">Capabilities</span>
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{filtered.slice(0, shown).map((model) => (
							<ModelRow
								key={model.id}
								model={model}
								onEdit={() => setEditing(model)}
							/>
						))}
						{filtered.length === 0 && (
							<TableRow>
								<TableCell
									colSpan={4}
									className="py-10 text-center text-muted-foreground"
								>
									No model matches.
								</TableCell>
							</TableRow>
						)}
					</TableBody>
				</Table>
			</div>
			{filtered.length > shown && (
				<div className="text-center">
					<Button variant="outline" onClick={() => setShown(shown + PAGE)}>
						Show more ({filtered.length - shown} left)
					</Button>
				</div>
			)}
			<CapabilityDialog model={editing} onClose={() => setEditing(null)} />
		</div>
	);
}

function useSaveModel() {
	const queryClient = useQueryClient();
	return useMutation({
		mutationFn: (update: {
			model: Model;
			enabled: boolean;
			overrides: Overrides;
		}) =>
			call(
				api.PUT("/models/{id}", {
					params: { path: { id: update.model.id } },
					body: {
						enabled: update.enabled,
						capability_overrides: update.overrides,
					},
				}),
			),
		// A toggle changes one flag; showing it at once is safe, and an error reverts it.
		onMutate: async (update) => {
			await queryClient.cancelQueries({ queryKey: modelsQuery.queryKey });
			const before = queryClient.getQueryData(modelsQuery.queryKey);
			queryClient.setQueryData(modelsQuery.queryKey, (list) =>
				list?.map((m) =>
					m.id === update.model.id ? { ...m, enabled: update.enabled } : m,
				),
			);
			return { before };
		},
		onError: (error, _update, context) => {
			if (context?.before)
				queryClient.setQueryData(modelsQuery.queryKey, context.before);
			toast.error(errorMessage(error));
		},
		onSettled: () => {
			void queryClient.invalidateQueries({ queryKey: modelsQuery.queryKey });
			void queryClient.invalidateQueries({
				queryKey: connectionsQuery.queryKey,
			});
		},
	});
}

function ModelRow({ model, onEdit }: { model: Model; onEdit: () => void }) {
	const save = useSaveModel();
	const changed = Object.keys(model.capability_overrides).length > 0;
	return (
		<TableRow>
			<TableCell className="pl-4">
				<Switch
					size="sm"
					aria-label={`Enable ${model.name}`}
					checked={model.enabled}
					// One save at a time, so an older save cannot win over a newer choice.
					disabled={save.isPending}
					onCheckedChange={(enabled) =>
						save.mutate({
							model,
							enabled,
							overrides: model.capability_overrides,
						})
					}
				/>
			</TableCell>
			<TableCell className="max-w-0 min-w-48">
				<p className="truncate font-mono text-xs" title={model.name}>
					<span className="text-muted-foreground">{model.source}/</span>
					{model.upstream_id}
				</p>
				{model.display_name && model.display_name !== model.upstream_id && (
					<p className="truncate text-xs text-muted-foreground">
						{model.display_name}
					</p>
				)}
			</TableCell>
			<TableCell className="hidden md:table-cell">
				<CapabilityChips capabilities={model.effective} changed={changed} />
			</TableCell>
			<TableCell>
				<Button
					variant="ghost"
					size="icon-sm"
					aria-label={`Capabilities of ${model.name}`}
					onClick={onEdit}
				>
					<SlidersHorizontalIcon />
				</Button>
			</TableCell>
		</TableRow>
	);
}

function formatTokens(n: number): string {
	return n >= 1_000_000
		? `${Math.round(n / 100_000) / 10}M`
		: n >= 1000
			? `${Math.round(n / 1000)}k`
			: String(n);
}

function Chip({
	children,
	title,
}: {
	children: React.ReactNode;
	title?: string;
}) {
	return (
		<span
			title={title}
			className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.7rem] whitespace-nowrap"
		>
			{children}
		</span>
	);
}

function CapabilityChips({
	capabilities,
	changed,
}: {
	capabilities: Capabilities;
	changed: boolean;
}) {
	const inputs = (capabilities.input ?? []).filter((kind) => kind !== "text");
	const efforts = capabilities.efforts ?? [];
	return (
		<div className="flex flex-wrap items-center gap-1">
			{capabilities.mode && capabilities.mode !== "chat" && (
				<Chip title="Model kind">{capabilities.mode}</Chip>
			)}
			{inputs.map((kind) => (
				<Chip key={kind} title="Input kind">
					{kind}
				</Chip>
			))}
			{efforts.length > 0 && (
				<Chip title={`Efforts: ${efforts.join(", ")}`}>
					effort {efforts[0]}–{efforts[efforts.length - 1]}
				</Chip>
			)}
			{capabilities.fast && <Chip title="Fast mode">fast</Chip>}
			{capabilities.context_window && (
				<Chip title="Context window">
					{formatTokens(capabilities.context_window)} ctx
				</Chip>
			)}
			{capabilities.endpoints && capabilities.endpoints.length === 1 && (
				<Chip title="Only this OpenAI endpoint">
					{capabilities.endpoints[0]} only
				</Chip>
			)}
			{changed && (
				<Badge variant="outline" className="border-meter text-meter">
					Changed
				</Badge>
			)}
		</div>
	);
}

// ---------------------------------------------------------------------------------------------

const INPUT_KINDS = ["text", "image", "file", "audio", "video"] as const;
const EFFORTS = [
	"none",
	"minimal",
	"low",
	"medium",
	"high",
	"xhigh",
	"max",
	"ultra",
] as const;

/** The editable part of the capabilities, as the dialog works with it. */
type Draft = {
	input: string[];
	efforts: string[];
	fast: boolean;
	context: string;
	maxOutput: string;
	chat: boolean;
	responses: boolean;
	chatTools: boolean;
	forcedTools: boolean;
	alwaysThinks: boolean;
};

function toDraft(c: Capabilities): Draft {
	return {
		input: c.input ?? [],
		efforts: c.efforts ?? [],
		fast: c.fast ?? false,
		context: c.context_window?.toString() ?? "",
		maxOutput: c.max_output?.toString() ?? "",
		chat: !c.endpoints || c.endpoints.includes("chat"),
		responses: !c.endpoints || c.endpoints.includes("responses"),
		chatTools: c.chat_tools ?? true,
		forcedTools: c.forced_tools_with_thinking ?? true,
		alwaysThinks: c.thinking_always_on ?? false,
	};
}

const sameList = (a: string[], b: string[]) =>
	a.length === b.length && a.every((x) => b.includes(x));

/**
 * Only the values that differ from the synced capabilities become overrides. Overrides that
 * the form does not show stay as they are.
 */
function toOverrides(
	draft: Draft,
	synced: Capabilities,
	kind: string,
	current: Overrides,
): Overrides {
	const base = toDraft(synced);
	const out: Overrides = current.thinking ? { thinking: current.thinking } : {};
	if (!sameList(draft.input, base.input)) out.input = draft.input;
	if (!sameList(draft.efforts, base.efforts)) out.efforts = draft.efforts;
	if (draft.fast !== base.fast) out.fast = draft.fast;
	if (draft.context !== base.context && draft.context !== "")
		out.context_window = Number(draft.context);
	if (draft.maxOutput !== base.maxOutput && draft.maxOutput !== "")
		out.max_output = Number(draft.maxOutput);
	if (kind === "openai") {
		if (draft.chat !== base.chat || draft.responses !== base.responses) {
			out.endpoints = [
				...(draft.chat ? ["chat"] : []),
				...(draft.responses ? ["responses"] : []),
			];
		}
		if (draft.chatTools !== base.chatTools) out.chat_tools = draft.chatTools;
	}
	if (kind === "anthropic" && draft.forcedTools !== base.forcedTools)
		out.forced_tools_with_thinking = draft.forcedTools;
	if (kind === "anthropic" && draft.alwaysThinks !== base.alwaysThinks)
		out.thinking_always_on = draft.alwaysThinks;
	return out;
}

function CapabilityDialog({
	model,
	onClose,
}: {
	model: Model | null;
	onClose: () => void;
}) {
	return (
		<Dialog open={model !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
				{model && (
					<CapabilityForm key={model.id} model={model} onClose={onClose} />
				)}
			</DialogContent>
		</Dialog>
	);
}

function CheckList({
	label,
	options,
	value,
	onChange,
	description,
}: {
	label: string;
	options: readonly string[];
	value: string[];
	onChange: (next: string[]) => void;
	description: string;
}) {
	const id = useId();
	return (
		<Field>
			<FieldLabel>{label}</FieldLabel>
			<div className="flex flex-wrap gap-x-4 gap-y-2">
				{options.map((option) => (
					<Field
						key={option}
						orientation="horizontal"
						className="w-fit gap-1.5"
					>
						<Checkbox
							id={`${id}-${option}`}
							checked={value.includes(option)}
							onCheckedChange={(checked) =>
								onChange(
									checked
										? options.filter((o) => o === option || value.includes(o))
										: value.filter((v) => v !== option),
								)
							}
						/>
						<FieldLabel
							htmlFor={`${id}-${option}`}
							className="font-mono text-xs font-normal"
						>
							{option}
						</FieldLabel>
					</Field>
				))}
			</div>
			<FieldDescription>{description}</FieldDescription>
		</Field>
	);
}

function Toggle({
	label,
	description,
	checked,
	onChange,
}: {
	label: string;
	description: string;
	checked: boolean;
	onChange: (checked: boolean) => void;
}) {
	const id = useId();
	return (
		<div className="flex items-start justify-between gap-4">
			<div className="space-y-0.5">
				<FieldLabel htmlFor={id}>{label}</FieldLabel>
				<p className="text-sm text-muted-foreground">{description}</p>
			</div>
			<Switch id={id} checked={checked} onCheckedChange={onChange} />
		</div>
	);
}

function CapabilityForm({
	model,
	onClose,
}: {
	model: Model;
	onClose: () => void;
}) {
	const save = useSaveModel();
	const [draft, setDraft] = useState<Draft>(() => toDraft(model.effective));
	const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
		setDraft((d) => ({ ...d, [key]: value }));
	const number = (v: string) => v === "" || (/^\d+$/.test(v) && Number(v) >= 1);
	const valid = number(draft.context) && number(draft.maxOutput);
	const kind = model.source_kind;
	const store = (overrides: Overrides, message: string) =>
		save.mutate(
			{ model, enabled: model.enabled, overrides },
			{
				onSuccess: () => {
					toast.success(message);
					onClose();
				},
			},
		);

	return (
		<>
			<DialogHeader>
				<DialogTitle className="font-mono text-base break-all">
					{model.name}
				</DialogTitle>
				<DialogDescription>
					The gateway uses these values to check inputs and to translate
					requests. Change them when the synced values are wrong or missing.
				</DialogDescription>
			</DialogHeader>
			<FieldGroup>
				<CheckList
					label="Input kinds"
					options={INPUT_KINDS}
					value={draft.input}
					onChange={(v) => set("input", v)}
					description="None checked means unknown: the provider decides."
				/>
				<CheckList
					label="Reasoning efforts"
					options={EFFORTS}
					value={draft.efforts}
					onChange={(v) => set("efforts", v)}
					description="Other efforts change to the nearest one checked."
				/>
				<div className="grid gap-4 sm:grid-cols-2">
					<Field data-invalid={!number(draft.context) || undefined}>
						<FieldLabel htmlFor="context">Context window</FieldLabel>
						<Input
							id="context"
							inputMode="numeric"
							value={draft.context}
							onChange={(e) => set("context", e.target.value.trim())}
						/>
					</Field>
					<Field data-invalid={!number(draft.maxOutput) || undefined}>
						<FieldLabel htmlFor="max-output">Max output tokens</FieldLabel>
						<Input
							id="max-output"
							inputMode="numeric"
							value={draft.maxOutput}
							onChange={(e) => set("maxOutput", e.target.value.trim())}
						/>
					</Field>
				</div>
				<Toggle
					label="Fast mode"
					description="Requests with fast mode or the priority tier use it."
					checked={draft.fast}
					onChange={(v) => set("fast", v)}
				/>
				{kind === "openai" && (
					<>
						<CheckList
							label="Endpoints"
							options={["chat", "responses"]}
							value={[
								...(draft.chat ? ["chat"] : []),
								...(draft.responses ? ["responses"] : []),
							]}
							onChange={(v) => {
								set("chat", v.includes("chat"));
								set("responses", v.includes("responses"));
							}}
							description="Chat-only models get Responses requests translated to Chat."
						/>
						<Toggle
							label="Function tools on Chat"
							description="Off: Chat requests with tools go to Responses."
							checked={draft.chatTools}
							onChange={(v) => set("chatTools", v)}
						/>
					</>
				)}
				{kind === "anthropic" && (
					<>
						<Toggle
							label="Forced tools with thinking"
							description="Off: a forced tool choice turns thinking off for that request (or fails when the model always thinks)."
							checked={draft.forcedTools}
							onChange={(v) => set("forcedTools", v)}
						/>
						<Toggle
							label="Thinking always on"
							description="The model cannot turn thinking off. Effort none sends no thinking field."
							checked={draft.alwaysThinks}
							onChange={(v) => set("alwaysThinks", v)}
						/>
					</>
				)}
				{save.error && <FieldError>{save.error.message}</FieldError>}
				<DialogFooter className="sm:justify-between">
					<Button
						variant="ghost"
						disabled={
							save.isPending ||
							Object.keys(model.capability_overrides).length === 0
						}
						onClick={() => store({}, "The synced values apply again.")}
					>
						Use synced values
					</Button>
					<div className="flex gap-2">
						<Button variant="outline" onClick={onClose}>
							Cancel
						</Button>
						<Button
							disabled={!valid || save.isPending}
							onClick={() =>
								store(
									toOverrides(
										draft,
										model.capabilities,
										kind,
										model.capability_overrides,
									),
									"The capabilities are saved.",
								)
							}
						>
							Save
						</Button>
					</div>
				</DialogFooter>
			</FieldGroup>
		</>
	);
}
