import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { CopyIcon, MoreHorizontalIcon, PlusIcon } from "lucide-react";
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
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "#/components/ui/table";
import { Textarea } from "#/components/ui/textarea";
import { api, call, errorMessage, type Schemas } from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";

type ApiKey = Schemas["ApiKeyView"];

const keysQuery = queryOptions({
	queryKey: ["api-keys"],
	queryFn: () => call(api.GET("/api-keys")),
});

export const Route = createFileRoute("/_app/api-keys")({
	loader: ({ context }) =>
		context.queryClient.query({ ...keysQuery, staleTime: "static" }),
	component: ApiKeysPage,
});

function ApiKeysPage() {
	const { data: keys = [] } = useQuery(keysQuery);
	const [editing, setEditing] = useState<ApiKey | "new" | null>(null);
	const [created, setCreated] = useState<string | null>(null);
	const [revoking, setRevoking] = useState<ApiKey | null>(null);

	return (
		<div className="space-y-6">
			<PageHeader
				title="API keys"
				description="Clients such as Claude Code or the OpenAI SDK use these keys to call the gateway."
				actions={
					<Button onClick={() => setEditing("new")}>
						<PlusIcon />
						Create key
					</Button>
				}
			/>
			{keys.length === 0 ? (
				<div className="rounded-xl border border-dashed px-6 py-14 text-center">
					<p className="font-medium">No API keys yet</p>
					<p className="mx-auto mt-1 max-w-sm text-sm text-muted-foreground">
						Create a key for each client. Then you can see the usage per key and
						revoke one key without touching the others.
					</p>
				</div>
			) : (
				<div className="overflow-hidden rounded-xl border bg-card">
					<Table>
						<TableHeader>
							<TableRow>
								<TableHead className="pl-4">Key</TableHead>
								<TableHead>Status</TableHead>
								<TableHead className="hidden md:table-cell">Limits</TableHead>
								<TableHead className="hidden lg:table-cell">Models</TableHead>
								<TableHead>Last used</TableHead>
								<TableHead className="w-12">
									<span className="sr-only">Actions</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{keys.map((key) => (
								<TableRow
									key={key.id}
									className={key.revoked_at ? "opacity-60" : undefined}
								>
									<TableCell className="max-w-56 pl-4">
										<p className="truncate font-medium">{key.name}</p>
										<p className="font-mono text-[0.7rem] text-muted-foreground">
											{key.prefix}…
										</p>
									</TableCell>
									<TableCell>
										<KeyStatus apiKey={key} />
									</TableCell>
									<TableCell className="hidden text-muted-foreground md:table-cell">
										{describeLimits(key)}
									</TableCell>
									<TableCell className="hidden max-w-64 lg:table-cell">
										<Patterns patterns={key.allowlist} />
									</TableCell>
									<TableCell className="text-muted-foreground">
										{key.last_used_at
											? formatRelative(key.last_used_at)
											: "Never"}
									</TableCell>
									<TableCell>
										{!key.revoked_at && (
											<DropdownMenu>
												<DropdownMenuTrigger
													render={
														<Button
															variant="ghost"
															size="icon-sm"
															aria-label={`Actions for ${key.name}`}
														/>
													}
												>
													<MoreHorizontalIcon />
												</DropdownMenuTrigger>
												<DropdownMenuContent align="end" className="w-40">
													<DropdownMenuItem onClick={() => setEditing(key)}>
														Edit
													</DropdownMenuItem>
													<DropdownMenuSeparator />
													<DropdownMenuItem
														variant="destructive"
														onClick={() => setRevoking(key)}
													>
														Revoke
													</DropdownMenuItem>
												</DropdownMenuContent>
											</DropdownMenu>
										)}
									</TableCell>
								</TableRow>
							))}
						</TableBody>
					</Table>
				</div>
			)}
			<KeyDialog
				target={editing}
				onClose={() => setEditing(null)}
				onCreated={(key) => {
					setEditing(null);
					setCreated(key);
				}}
			/>
			<CreatedKeyDialog apiKey={created} onClose={() => setCreated(null)} />
			<RevokeDialog apiKey={revoking} onClose={() => setRevoking(null)} />
		</div>
	);
}

function KeyStatus({ apiKey }: { apiKey: ApiKey }) {
	const now = Date.now() / 1000;
	if (apiKey.revoked_at) return <Badge variant="outline">Revoked</Badge>;
	if (apiKey.expires_at && apiKey.expires_at <= now) {
		return <Badge variant="destructive">Expired</Badge>;
	}
	if (apiKey.expires_at) {
		return (
			<Badge variant="secondary" title={formatDateTime(apiKey.expires_at)}>
				Expires {formatRelative(apiKey.expires_at)}
			</Badge>
		);
	}
	return <Badge variant="secondary">Active</Badge>;
}

function describeLimits(key: ApiKey): string {
	const parts = [
		key.rpm_limit && `${key.rpm_limit.toLocaleString()} req/min`,
		key.tpm_limit && `${key.tpm_limit.toLocaleString()} tokens/min`,
		`${key.concurrency_limit ?? 8} parallel`,
	];
	return parts.filter(Boolean).join(" · ");
}

function Patterns({ patterns }: { patterns: string[] }) {
	if (patterns.length === 0) {
		return <span className="text-muted-foreground">All models</span>;
	}
	return (
		<div className="flex flex-wrap gap-1">
			{patterns.slice(0, 3).map((p) => (
				<code
					key={p}
					className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.7rem]"
				>
					{p}
				</code>
			))}
			{patterns.length > 3 && (
				<span className="text-xs text-muted-foreground">
					+{patterns.length - 3}
				</span>
			)}
		</div>
	);
}

// ---------------------------------------------------------------------------------------------

const EXPIRY_OPTIONS = {
	keep: "Keep the current date",
	never: "Never",
	"30": "In 30 days",
	"90": "In 90 days",
	"365": "In 1 year",
} as const;
type Expiry = keyof typeof EXPIRY_OPTIONS;

const optionalLimit = (max: number) =>
	z
		.string()
		.trim()
		.refine(
			(v) =>
				v === "" || (/^\d+$/.test(v) && Number(v) >= 1 && Number(v) <= max),
			`Leave empty, or enter 1 to ${max.toLocaleString()}.`,
		);

const PATTERN = /^[A-Za-z0-9._\-:/@*]{1,200}$/;

const keySchema = z.object({
	name: z
		.string()
		.trim()
		.min(1, "Enter a name.")
		.max(100, "Use at most 100 characters."),
	expiry: z.string(),
	rpm: optionalLimit(1_000_000_000),
	tpm: optionalLimit(1_000_000_000),
	concurrency: optionalLimit(1000),
	allowlist: z
		.string()
		.refine(
			(v) => splitPatterns(v).every((p) => PATTERN.test(p)),
			"Use one pattern per line: letters, digits, . _ - : / @ and * as wildcard.",
		)
		.refine((v) => splitPatterns(v).length <= 100, "Use at most 100 patterns."),
});

function splitPatterns(text: string): string[] {
	return text
		.split("\n")
		.map((line) => line.trim())
		.filter(Boolean);
}

function toSettings(
	value: z.infer<typeof keySchema>,
	current: ApiKey | null,
): Schemas["KeySettings"] {
	const limit = (v: string) => (v.trim() === "" ? null : Number(v));
	const expiry = value.expiry as Expiry;
	const expires_at =
		expiry === "keep"
			? (current?.expires_at ?? null)
			: expiry === "never"
				? null
				: Math.floor(Date.now() / 1000) + Number(expiry) * 86400;
	return {
		name: value.name.trim(),
		expires_at,
		rpm_limit: limit(value.rpm),
		tpm_limit: limit(value.tpm),
		concurrency_limit: limit(value.concurrency),
		allowlist: splitPatterns(value.allowlist),
	};
}

function KeyDialog({
	target,
	onClose,
	onCreated,
}: {
	target: ApiKey | "new" | null;
	onClose: () => void;
	onCreated: (key: string) => void;
}) {
	const current = target === "new" ? null : target;
	return (
		<Dialog open={target !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="sm:max-w-lg">
				{target !== null && (
					// Remount per key, so the form starts with that key's values.
					<KeyForm
						key={current?.id ?? "new"}
						current={current}
						onClose={onClose}
						onCreated={onCreated}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}

function KeyForm({
	current,
	onClose,
	onCreated,
}: {
	current: ApiKey | null;
	onClose: () => void;
	onCreated: (key: string) => void;
}) {
	const queryClient = useQueryClient();
	const save = useMutation({
		mutationFn: async (value: z.infer<typeof keySchema>) => {
			const body = toSettings(value, current);
			if (current) {
				await call(
					api.PUT("/api-keys/{id}", {
						params: { path: { id: current.id } },
						body,
					}),
				);
				return null;
			}
			return (await call(api.POST("/api-keys", { body }))).key;
		},
		onSuccess: (key) => {
			void queryClient.invalidateQueries({ queryKey: keysQuery.queryKey });
			if (key) onCreated(key);
			else {
				toast.success("The key is saved.");
				onClose();
			}
		},
	});
	const form = useForm({
		defaultValues: {
			name: current?.name ?? "",
			expiry: current ? "keep" : "never",
			rpm: current?.rpm_limit?.toString() ?? "",
			tpm: current?.tpm_limit?.toString() ?? "",
			concurrency: current?.concurrency_limit?.toString() ?? "",
			allowlist: current?.allowlist.join("\n") ?? "",
		},
		validators: { onSubmit: keySchema },
		onSubmit: ({ value }) => save.mutateAsync(value).catch(() => undefined),
	});
	const expiryItems = Object.entries(EXPIRY_OPTIONS)
		.filter(([value]) => current || value !== "keep")
		.map(([value, label]) => ({
			value,
			label:
				value === "keep" && current
					? `Keep: ${current.expires_at ? formatDateTime(current.expires_at) : "never"}`
					: label,
		}));

	return (
		<>
			<DialogHeader>
				<DialogTitle>
					{current ? `Edit ${current.name}` : "Create API key"}
				</DialogTitle>
				<DialogDescription>
					{current
						? "Changes apply to the next request."
						: "You see the key only once, right after you create it."}
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
								description="For example the client or the device."
							/>
						)}
					</form.Field>
					<form.Field name="expiry">
						{(field) => (
							<Field>
								<FieldLabel htmlFor="expiry">Expires</FieldLabel>
								<Select
									items={expiryItems}
									value={field.state.value}
									onValueChange={(v) => v && field.handleChange(v)}
								>
									<SelectTrigger id="expiry" className="w-full">
										<SelectValue />
									</SelectTrigger>
									<SelectContent>
										{expiryItems.map((item) => (
											<SelectItem key={item.value} value={item.value}>
												{item.label}
											</SelectItem>
										))}
									</SelectContent>
								</Select>
							</Field>
						)}
					</form.Field>
					<div className="grid gap-4 sm:grid-cols-3">
						<form.Field name="rpm">
							{(field) => <TextField field={field} label="Requests / min" />}
						</form.Field>
						<form.Field name="tpm">
							{(field) => <TextField field={field} label="Tokens / min" />}
						</form.Field>
						<form.Field name="concurrency">
							{(field) => <TextField field={field} label="Parallel" />}
						</form.Field>
					</div>
					<FieldDescription className="-mt-3">
						Empty means no limit. Parallel requests default to 8.
					</FieldDescription>
					<form.Field name="allowlist">
						{(field) => {
							const invalid =
								field.state.meta.isTouched && !field.state.meta.isValid;
							return (
								<Field data-invalid={invalid || undefined}>
									<FieldLabel htmlFor="allowlist">Allowed models</FieldLabel>
									<Textarea
										id="allowlist"
										rows={3}
										className="font-mono text-xs md:text-xs"
										placeholder={"chatgpt/*\nclaude-sol"}
										value={field.state.value}
										onBlur={field.handleBlur}
										onChange={(e) => field.handleChange(e.target.value)}
										aria-invalid={invalid || undefined}
									/>
									{invalid ? (
										<FieldError errors={field.state.meta.errors} />
									) : (
										<FieldDescription>
											One pattern per line, * is a wildcard. Empty allows all
											models.
										</FieldDescription>
									)}
								</Field>
							);
						}}
					</form.Field>
					{save.error && <FieldError>{save.error.message}</FieldError>}
					<DialogFooter>
						<Button type="button" variant="outline" onClick={onClose}>
							Cancel
						</Button>
						<form.Subscribe selector={(s) => s.isSubmitting}>
							{(submitting) => (
								<Button type="submit" disabled={submitting}>
									{current ? "Save changes" : "Create key"}
								</Button>
							)}
						</form.Subscribe>
					</DialogFooter>
				</FieldGroup>
			</form>
		</>
	);
}

function CreatedKeyDialog({
	apiKey,
	onClose,
}: {
	apiKey: string | null;
	onClose: () => void;
}) {
	const origin = window.location.origin;
	const copy = async (text: string, what: string) => {
		await navigator.clipboard.writeText(text);
		toast.success(`${what} is copied.`);
	};
	const claudeCode = `export ANTHROPIC_BASE_URL=${origin}\nexport ANTHROPIC_AUTH_TOKEN=${apiKey}`;
	const openai = `export OPENAI_BASE_URL=${origin}/v1\nexport OPENAI_API_KEY=${apiKey}`;

	return (
		<Dialog open={apiKey !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="sm:max-w-lg" showCloseButton={false}>
				<DialogHeader>
					<DialogTitle>Copy your new key</DialogTitle>
					<DialogDescription>
						This is the only time you see the full key. Store it in the client
						now.
					</DialogDescription>
				</DialogHeader>
				<div className="flex min-w-0 items-center gap-2 rounded-lg bg-muted p-2 pl-3">
					<code className="min-w-0 flex-1 font-mono text-xs break-all">
						{apiKey}
					</code>
					<Button
						size="sm"
						variant="outline"
						onClick={() => apiKey && void copy(apiKey, "The key")}
					>
						<CopyIcon />
						Copy
					</Button>
				</div>
				<Snippet
					title="Claude Code"
					text={claudeCode}
					onCopy={() => void copy(claudeCode, "The Claude Code setup")}
				/>
				<Snippet
					title="OpenAI SDKs and tools"
					text={openai}
					onCopy={() => void copy(openai, "The OpenAI setup")}
				/>
				<DialogFooter>
					<Button onClick={onClose}>Done</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
}

function Snippet({
	title,
	text,
	onCopy,
}: {
	title: string;
	text: string;
	onCopy: () => void;
}) {
	return (
		<div className="min-w-0 space-y-1.5">
			<div className="flex items-center justify-between">
				<span className="eyebrow">{title}</span>
				<Button size="xs" variant="ghost" onClick={onCopy}>
					<CopyIcon />
					Copy
				</Button>
			</div>
			<pre className="overflow-x-auto rounded-lg border bg-background px-3 py-2 font-mono text-[0.7rem] leading-relaxed">
				{text}
			</pre>
		</div>
	);
}

function RevokeDialog({
	apiKey,
	onClose,
}: {
	apiKey: ApiKey | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const revoke = useMutation({
		mutationFn: (id: number) =>
			call(api.POST("/api-keys/{id}/revoke", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: keysQuery.queryKey });
			toast.success(`${apiKey?.name} is revoked.`);
			onClose();
		},
		onError: (error) => {
			toast.error(errorMessage(error));
			onClose();
		},
	});

	return (
		<AlertDialog
			open={apiKey !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>Revoke {apiKey?.name}?</AlertDialogTitle>
					<AlertDialogDescription>
						Clients with this key stop working at once. You cannot undo it. The
						usage of the key stays visible.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={revoke.isPending}
						onClick={() => apiKey && revoke.mutate(apiKey.id)}
					>
						Revoke key
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
