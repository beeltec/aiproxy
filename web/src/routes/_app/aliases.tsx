import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import {
	ArrowRightIcon,
	CopyIcon,
	MoreHorizontalIcon,
	PlusIcon,
} from "lucide-react";
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
import { api, call, errorMessage, type Schemas } from "#/lib/api/client";
import { modelsQuery } from "#/lib/models";

type Alias = Schemas["AliasView"];

const aliasesQuery = queryOptions({
	queryKey: ["aliases"],
	queryFn: () => call(api.GET("/aliases")),
});

export const Route = createFileRoute("/_app/aliases")({
	loader: ({ context }) =>
		Promise.all([
			context.queryClient.query({ ...aliasesQuery, staleTime: "static" }),
			context.queryClient.query({ ...modelsQuery, staleTime: "static" }),
		]),
	component: AliasesPage,
});

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
const SUMMARIES = ["auto", "concise", "detailed"] as const;

function AliasesPage() {
	const { data: aliases = [] } = useQuery(aliasesQuery);
	const [editing, setEditing] = useState<Alias | "new" | null>(null);
	const [deleting, setDeleting] = useState<Alias | null>(null);

	return (
		<div className="space-y-6">
			<PageHeader
				title="Aliases"
				description="Short model names that point to a model and can set its effort, fast mode and summary."
				actions={
					<Button onClick={() => setEditing("new")}>
						<PlusIcon />
						Create alias
					</Button>
				}
			/>
			{aliases.length === 0 ? (
				<div className="rounded-xl border border-dashed px-6 py-14 text-center">
					<p className="font-medium">No aliases yet</p>
					<p className="mx-auto mt-1 max-w-md text-sm text-muted-foreground">
						An alias such as claude-sol-fast can point to chatgpt/gpt-5.6-sol
						with fast mode on. Claude Code shows it in its model list when the
						name contains "claude".
					</p>
				</div>
			) : (
				<div className="overflow-hidden rounded-xl border bg-card">
					<Table>
						<TableHeader>
							<TableRow>
								<TableHead className="pl-4">Alias</TableHead>
								<TableHead className="hidden md:table-cell">Model</TableHead>
								<TableHead>Defaults</TableHead>
								<TableHead className="w-12">
									<span className="sr-only">Actions</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{aliases.map((alias) => (
								<TableRow key={alias.id}>
									<TableCell className="max-w-0 min-w-40 pl-4">
										<p className="truncate font-mono text-xs font-medium">
											{alias.name}
										</p>
										<p className="truncate font-mono text-[0.7rem] text-muted-foreground md:hidden">
											{alias.target}
										</p>
										{alias.description && (
											<p className="truncate text-xs text-muted-foreground">
												{alias.description}
											</p>
										)}
									</TableCell>
									<TableCell className="hidden max-w-0 min-w-48 md:table-cell">
										<div className="flex items-center gap-2">
											<ArrowRightIcon className="size-3.5 shrink-0 text-muted-foreground" />
											<span className="truncate font-mono text-xs">
												{alias.target}
											</span>
											{!alias.target_enabled && (
												<Badge variant="destructive">Model off</Badge>
											)}
										</div>
									</TableCell>
									<TableCell>
										<Defaults alias={alias} />
									</TableCell>
									<TableCell>
										<DropdownMenu>
											<DropdownMenuTrigger
												render={
													<Button
														variant="ghost"
														size="icon-sm"
														aria-label={`Actions for ${alias.name}`}
													/>
												}
											>
												<MoreHorizontalIcon />
											</DropdownMenuTrigger>
											<DropdownMenuContent align="end" className="w-36">
												<DropdownMenuItem onClick={() => setEditing(alias)}>
													Edit
												</DropdownMenuItem>
												<DropdownMenuSeparator />
												<DropdownMenuItem
													variant="destructive"
													onClick={() => setDeleting(alias)}
												>
													Delete
												</DropdownMenuItem>
											</DropdownMenuContent>
										</DropdownMenu>
									</TableCell>
								</TableRow>
							))}
						</TableBody>
					</Table>
				</div>
			)}
			{aliases.length > 0 && <ClaudeCodeSetup aliases={aliases} />}
			<AliasDialog target={editing} onClose={() => setEditing(null)} />
			<DeleteDialog alias={deleting} onClose={() => setDeleting(null)} />
		</div>
	);
}

function Defaults({ alias }: { alias: Alias }) {
	const parts = [
		alias.default_effort && `effort ${alias.default_effort}`,
		alias.default_fast && "fast",
		alias.default_summary && `summary ${alias.default_summary}`,
	].filter((p): p is string => Boolean(p));
	if (parts.length === 0) {
		return <span className="text-muted-foreground">None</span>;
	}
	return (
		<div className="flex flex-wrap gap-1">
			{parts.map((part) => (
				<span
					key={part}
					className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.7rem] whitespace-nowrap"
				>
					{part}
				</span>
			))}
		</div>
	);
}

// ---------------------------------------------------------------------------------------------

/** Claude Code picks its model slots from these variables. */
const SLOTS = [
	{ env: "ANTHROPIC_DEFAULT_OPUS_MODEL", label: "Opus slot" },
	{ env: "ANTHROPIC_DEFAULT_SONNET_MODEL", label: "Sonnet slot" },
	{ env: "ANTHROPIC_DEFAULT_HAIKU_MODEL", label: "Haiku slot" },
	{ env: "CLAUDE_CODE_SUBAGENT_MODEL", label: "Subagents" },
] as const;
const NONE = "__none";

function ClaudeCodeSetup({ aliases }: { aliases: Alias[] }) {
	const [slots, setSlots] = useState<Record<string, string>>(() => {
		const first = aliases[0]?.name ?? NONE;
		return Object.fromEntries(SLOTS.map((s) => [s.env, first]));
	});
	const items = [
		{ value: NONE, label: "Not set" },
		...aliases.map((a) => ({ value: a.name, label: a.name })),
	];
	const lines = [
		`export ANTHROPIC_BASE_URL=${window.location.origin}`,
		"export ANTHROPIC_AUTH_TOKEN=<your gateway API key>",
		...SLOTS.filter((s) => slots[s.env] !== NONE).map(
			(s) => `export ${s.env}=${slots[s.env]}`,
		),
	];
	const text = lines.join("\n");

	return (
		<section className="rounded-xl border bg-card">
			<div className="space-y-1 p-4">
				<h2 className="font-semibold">Claude Code setup</h2>
				<p className="text-sm text-muted-foreground">
					Pick an alias for each model slot of Claude Code, then put these lines
					in your shell profile.
				</p>
			</div>
			<div className="grid gap-3 border-t p-4 sm:grid-cols-2 lg:grid-cols-4">
				{SLOTS.map((slot) => (
					<Field key={slot.env}>
						<FieldLabel htmlFor={slot.env}>{slot.label}</FieldLabel>
						<Select
							items={items}
							value={slots[slot.env]}
							onValueChange={(v) =>
								v && setSlots((s) => ({ ...s, [slot.env]: v }))
							}
						>
							<SelectTrigger id={slot.env} className="w-full">
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								{items.map((item) => (
									<SelectItem key={item.value} value={item.value}>
										{item.label}
									</SelectItem>
								))}
							</SelectContent>
						</Select>
					</Field>
				))}
			</div>
			<div className="flex items-start gap-2 border-t p-4">
				<pre className="min-w-0 flex-1 overflow-x-auto rounded-lg bg-muted p-3 font-mono text-xs">
					{text}
				</pre>
				<Button
					size="sm"
					variant="outline"
					onClick={() =>
						void navigator.clipboard
							.writeText(text)
							.then(() => toast.success("The Claude Code setup is copied."))
					}
				>
					<CopyIcon />
					Copy
				</Button>
			</div>
		</section>
	);
}

// ---------------------------------------------------------------------------------------------

const aliasSchema = z.object({
	name: z
		.string()
		.trim()
		.min(1, "Enter a name.")
		.max(100, "Use at most 100 characters.")
		.regex(/^[A-Za-z0-9._\-:@]+$/, "Use letters, digits and . _ - : @ (no /)."),
	target: z.string().min(1, "Pick a model."),
	effort: z.string(),
	fast: z.boolean(),
	summary: z.string(),
	description: z.string().trim().max(500, "Use at most 500 characters."),
});

function AliasDialog({
	target,
	onClose,
}: {
	target: Alias | "new" | null;
	onClose: () => void;
}) {
	const current = target === "new" ? null : target;
	return (
		<Dialog open={target !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
				{target !== null && (
					<AliasForm
						key={current?.id ?? "new"}
						current={current}
						onClose={onClose}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}

function AliasForm({
	current,
	onClose,
}: {
	current: Alias | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const { data: models = [] } = useQuery(modelsQuery);
	const save = useMutation({
		mutationFn: async (value: z.infer<typeof aliasSchema>) => {
			const body = {
				name: value.name,
				target: value.target,
				default_effort: value.effort === NONE ? null : value.effort,
				default_fast: value.fast,
				default_summary: value.summary === NONE ? null : value.summary,
				description: value.description || null,
			};
			if (current) {
				return call(
					api.PUT("/aliases/{id}", {
						params: { path: { id: current.id } },
						body,
					}),
				);
			}
			return call(api.POST("/aliases", { body }));
		},
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: aliasesQuery.queryKey });
			toast.success(current ? "The alias is saved." : "The alias is created.");
			onClose();
		},
	});
	const form = useForm({
		defaultValues: {
			name: current?.name ?? "",
			target: current?.target ?? "",
			effort: current?.default_effort ?? NONE,
			fast: current?.default_fast ?? false,
			summary: current?.default_summary ?? NONE,
			description: current?.description ?? "",
		},
		validators: { onSubmit: aliasSchema },
		onSubmit: ({ value }) => save.mutateAsync(value).catch(() => undefined),
	});
	// Enabled models, plus the current target so that an alias of a disabled model stays editable.
	const targets = models
		.filter((m) => m.enabled || m.name === current?.target)
		.map((m) => ({ value: m.name, label: m.name }));
	const effortItems = [
		{ value: NONE, label: "As the client sends it" },
		...EFFORTS.map((e) => ({ value: e, label: e })),
	];
	const summaryItems = [
		{ value: NONE, label: "As the client sends it" },
		...SUMMARIES.map((s) => ({ value: s, label: s })),
	];

	return (
		<>
			<DialogHeader>
				<DialogTitle>
					{current ? `Edit ${current.name}` : "Create alias"}
				</DialogTitle>
				<DialogDescription>
					Defaults apply only when the client does not set the value itself.
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
					<form.Field name="name">
						{(field) => (
							<TextField
								field={field}
								label="Name"
								autoComplete="off"
								description="Clients send this as the model name."
							/>
						)}
					</form.Field>
					<form.Field name="target">
						{(field) => {
							const invalid =
								field.state.meta.isTouched && !field.state.meta.isValid;
							return (
								<Field data-invalid={invalid || undefined}>
									<FieldLabel htmlFor="target">Model</FieldLabel>
									<Select
										items={targets}
										value={field.state.value || null}
										onValueChange={(v) => v && field.handleChange(v)}
									>
										<SelectTrigger
											id="target"
											className="w-full font-mono text-xs"
											aria-invalid={invalid || undefined}
										>
											<SelectValue placeholder="Pick an enabled model" />
										</SelectTrigger>
										<SelectContent>
											{targets.map((item) => (
												<SelectItem
													key={item.value}
													value={item.value}
													className="font-mono text-xs"
												>
													{item.label}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
									{invalid ? (
										<FieldError errors={field.state.meta.errors} />
									) : (
										<FieldDescription>
											Only enabled models are listed. Enable more in Connections
											→ Models.
										</FieldDescription>
									)}
								</Field>
							);
						}}
					</form.Field>
					<div className="grid gap-4 sm:grid-cols-2">
						<form.Field name="effort">
							{(field) => (
								<Field>
									<FieldLabel htmlFor="effort">Reasoning effort</FieldLabel>
									<Select
										items={effortItems}
										value={field.state.value}
										onValueChange={(v) => v && field.handleChange(v)}
									>
										<SelectTrigger id="effort" className="w-full">
											<SelectValue />
										</SelectTrigger>
										<SelectContent>
											{effortItems.map((item) => (
												<SelectItem key={item.value} value={item.value}>
													{item.label}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</Field>
							)}
						</form.Field>
						<form.Field name="summary">
							{(field) => (
								<Field>
									<FieldLabel htmlFor="summary">Reasoning summary</FieldLabel>
									<Select
										items={summaryItems}
										value={field.state.value}
										onValueChange={(v) => v && field.handleChange(v)}
									>
										<SelectTrigger id="summary" className="w-full">
											<SelectValue />
										</SelectTrigger>
										<SelectContent>
											{summaryItems.map((item) => (
												<SelectItem key={item.value} value={item.value}>
													{item.label}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</Field>
							)}
						</form.Field>
					</div>
					<form.Field name="fast">
						{(field) => (
							<div className="flex items-start justify-between gap-4">
								<div className="space-y-0.5">
									<FieldLabel htmlFor="fast">Fast mode</FieldLabel>
									<p className="text-sm text-muted-foreground">
										Priority tier on OpenAI and ChatGPT, fast mode on Anthropic
										models that have it.
									</p>
								</div>
								<Switch
									id="fast"
									checked={field.state.value}
									onCheckedChange={(v) => field.handleChange(v)}
								/>
							</div>
						)}
					</form.Field>
					<form.Field name="description">
						{(field) => (
							<TextField
								field={field}
								label="Description (optional)"
								autoComplete="off"
							/>
						)}
					</form.Field>
					{save.error && <FieldError>{save.error.message}</FieldError>}
					<DialogFooter>
						<Button type="button" variant="outline" onClick={onClose}>
							Cancel
						</Button>
						<form.Subscribe selector={(s) => s.isSubmitting}>
							{(submitting) => (
								<Button type="submit" disabled={submitting}>
									{current ? "Save changes" : "Create alias"}
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
	alias,
	onClose,
}: {
	alias: Alias | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/aliases/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: aliasesQuery.queryKey });
			toast.success("The alias is deleted.");
			onClose();
		},
		onError: (error) => toast.error(errorMessage(error)),
	});
	return (
		<AlertDialog
			open={alias !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>Delete {alias?.name}?</AlertDialogTitle>
					<AlertDialogDescription>
						Clients that send this name get "model not found" from then on.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={() => alias && remove.mutate(alias.id)}
					>
						Delete alias
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
