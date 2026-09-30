import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import {
	ArrowDownIcon,
	ArrowUpIcon,
	CopyIcon,
	ExternalLinkIcon,
	MoreHorizontalIcon,
	PlusIcon,
	RefreshCwIcon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { CronField } from "#/components/cron-field";
import { PageHeader } from "#/components/page-header";
import { QuotaMeters } from "#/components/quota-meters";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "#/components/ui/tabs";
import { accountsQuery } from "#/lib/accounts";
import {
	ApiError,
	api,
	call,
	errorMessage,
	type Schemas,
} from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";
import { modelsQuery } from "#/lib/models";
import { settingsQuery } from "#/lib/settings";

type Account = Schemas["AccountView"];

export const Route = createFileRoute("/_app/subscriptions")({
	loader: ({ context }) =>
		Promise.all([
			context.queryClient.query({ ...accountsQuery, staleTime: "static" }),
			context.queryClient.query({ ...settingsQuery, staleTime: "static" }),
		]),
	component: SubscriptionsPage,
});

function SubscriptionsPage() {
	const { data: accounts = [] } = useQuery(accountsQuery);
	const { data: settings } = useQuery(settingsQuery);
	const [linking, setLinking] = useState(false);
	const [editing, setEditing] = useState<Account | null>(null);
	const [removing, setRemoving] = useState<Account | null>(null);
	const failover = settings?.failover.enabled ?? false;

	return (
		<div className="space-y-6">
			<PageHeader
				title="Subscriptions"
				description="ChatGPT accounts that the gateway uses for chatgpt/* models."
				actions={
					<Button onClick={() => setLinking(true)}>
						<PlusIcon />
						Link account
					</Button>
				}
			/>
			<p className="text-sm text-muted-foreground">
				Failover is{" "}
				<span className="font-medium text-foreground">
					{failover ? "on" : "off"}
				</span>
				.{" "}
				{failover
					? "Requests use the primary account, then the checked accounts in this order."
					: "All requests use the primary account."}{" "}
				<Link
					to="/settings"
					className="text-primary underline-offset-4 hover:underline"
				>
					Change in Settings
				</Link>
			</p>
			{accounts.length === 0 ? (
				<div className="rounded-xl border border-dashed px-6 py-14 text-center">
					<p className="font-medium">No ChatGPT account linked</p>
					<p className="mx-auto mt-1 max-w-md text-sm text-muted-foreground">
						Link an account with a ChatGPT Plus, Pro or Business plan. The
						gateway then sends requests for chatgpt/* models through it.
					</p>
				</div>
			) : (
				<ol className="space-y-3">
					{accounts.map((account, index) => (
						<AccountCard
							key={account.id}
							account={account}
							failover={failover}
							threshold={settings?.failover.threshold_percent ?? 95}
							accounts={accounts}
							index={index}
							onEdit={() => setEditing(account)}
							onRemove={() => setRemoving(account)}
							onRelink={() => setLinking(true)}
						/>
					))}
				</ol>
			)}
			<LinkDialog open={linking} onClose={() => setLinking(false)} />
			<PlanDialog account={editing} onClose={() => setEditing(null)} />
			<RemoveDialog account={removing} onClose={() => setRemoving(null)} />
		</div>
	);
}

function accountName(account: Account): string {
	return account.label || account.email || `Account ${account.id}`;
}

function AccountCard({
	account,
	failover,
	threshold,
	accounts,
	index,
	onEdit,
	onRemove,
	onRelink,
}: {
	account: Account;
	failover: boolean;
	threshold: number;
	accounts: Account[];
	index: number;
	onEdit: () => void;
	onRemove: () => void;
	onRelink: () => void;
}) {
	const queryClient = useQueryClient();
	const refresh = () =>
		queryClient.invalidateQueries({ queryKey: accountsQuery.queryKey });
	const refreshNow = useMutation({
		mutationFn: () =>
			call(
				api.POST("/chatgpt/accounts/{id}/refresh", {
					params: { path: { id: account.id } },
				}),
			),
		onSuccess: () => {
			toast.success("The token, models and usage are refreshed.");
			void refresh();
			void queryClient.invalidateQueries({ queryKey: modelsQuery.queryKey });
		},
		onError: (error) => {
			toast.error(errorMessage(error));
			void refresh();
		},
	});
	const makePrimary = useMutation({
		mutationFn: () =>
			call(
				api.POST("/chatgpt/accounts/{id}/primary", {
					params: { path: { id: account.id } },
				}),
			),
		onSuccess: () => void refresh(),
		onError: (error) => toast.error(errorMessage(error)),
	});
	const toggleFailover = useMutation({
		mutationFn: (enabled: boolean) =>
			call(
				api.PATCH("/chatgpt/accounts/{id}", {
					params: { path: { id: account.id } },
					body: {
						label: account.label,
						refresh_mode: account.refresh_mode,
						refresh_cron: account.refresh_cron,
						failover_enabled: enabled,
					},
				}),
			),
		onSuccess: () => void refresh(),
		onError: (error) => toast.error(errorMessage(error)),
	});
	const move = useMutation({
		mutationFn: (direction: -1 | 1) => {
			const ids = accounts.map((a) => a.id);
			const target = index + direction;
			[ids[index], ids[target]] = [ids[target], ids[index]];
			return call(
				api.PUT("/chatgpt/failover-order", { body: { account_ids: ids } }),
			);
		},
		onSuccess: () => void refresh(),
		onError: (error) => toast.error(errorMessage(error)),
	});
	const broken = account.status === "needs_relogin";

	return (
		<li className="rounded-xl border bg-card">
			<div className="flex flex-wrap items-start justify-between gap-3 p-4">
				<div className="min-w-0 space-y-1">
					<div className="flex flex-wrap items-center gap-2">
						<span className="truncate font-medium">{accountName(account)}</span>
						{account.plan_type && (
							<Badge variant="outline" className="capitalize">
								{account.plan_type}
							</Badge>
						)}
						{account.is_primary && <Badge>Primary</Badge>}
						{broken && <Badge variant="destructive">Needs new login</Badge>}
						{!broken && limited(account) && (
							<Badge variant="outline" className="border-meter text-meter">
								Limit reached
							</Badge>
						)}
					</div>
					{account.label && account.email && (
						<p className="text-xs text-muted-foreground">{account.email}</p>
					)}
				</div>
				<div className="flex items-center gap-1">
					{broken ? (
						<Button size="sm" onClick={onRelink}>
							Link again
						</Button>
					) : (
						<Button
							size="sm"
							variant="outline"
							disabled={refreshNow.isPending}
							onClick={() => refreshNow.mutate()}
						>
							<RefreshCwIcon
								className={refreshNow.isPending ? "animate-spin" : undefined}
							/>
							Refresh now
						</Button>
					)}
					<DropdownMenu>
						<DropdownMenuTrigger
							render={
								<Button
									variant="ghost"
									size="icon-sm"
									aria-label={`Actions for ${accountName(account)}`}
								/>
							}
						>
							<MoreHorizontalIcon />
						</DropdownMenuTrigger>
						<DropdownMenuContent align="end" className="w-48">
							{!account.is_primary && (
								<DropdownMenuItem onClick={() => makePrimary.mutate()}>
									Make primary
								</DropdownMenuItem>
							)}
							<DropdownMenuItem onClick={onEdit}>
								Name and refresh plan
							</DropdownMenuItem>
							<DropdownMenuSeparator />
							<DropdownMenuItem variant="destructive" onClick={onRemove}>
								Remove
							</DropdownMenuItem>
						</DropdownMenuContent>
					</DropdownMenu>
				</div>
			</div>
			<dl className="grid grid-cols-2 gap-x-6 gap-y-3 border-t px-4 py-3 text-sm sm:grid-cols-4 lg:grid-cols-5">
				<Fact label="Last refresh">
					<span title={formatDateTime(account.last_refresh_at)}>
						{formatRelative(account.last_refresh_at)}
					</span>
				</Fact>
				<Fact label="Next refresh">
					{account.next_refresh_at ? (
						<span title={formatDateTime(account.next_refresh_at)}>
							{formatRelative(account.next_refresh_at)}
						</span>
					) : (
						"When needed"
					)}
				</Fact>
				<Fact label="Refresh plan">
					{account.refresh_mode === "custom" ? (
						<code className="font-mono text-xs">{account.refresh_cron}</code>
					) : account.refresh_mode === "disabled" ? (
						"Off"
					) : (
						"Global plan"
					)}
				</Fact>
				<div className="min-w-0 space-y-0.5">
					<dt className="eyebrow">Models</dt>
					<dd className="flex min-w-0 items-center gap-1.5">
						<span className="font-mono tabular-nums">{account.models}</span>
						<span
							className="truncate text-muted-foreground"
							title={
								account.models_last_sync_at
									? `Refreshed ${formatDateTime(account.models_last_sync_at)}`
									: undefined
							}
						>
							{account.models_last_sync_at
								? formatRelative(account.models_last_sync_at)
								: "not refreshed"}
						</span>
					</dd>
				</div>
				{account.credits_unlimited ? (
					<Fact label="Credits">Unlimited</Fact>
				) : (
					account.has_credits &&
					account.credits_balance && (
						<Fact label="Credits">
							<span className="font-mono tabular-nums">
								{account.credits_balance}
							</span>
						</Fact>
					)
				)}
			</dl>
			<QuotaMeters account={account} threshold={failover ? threshold : null} />
			{account.last_refresh_error && account.last_refresh_failed_at && (
				<p className="border-t px-4 py-2 text-xs text-destructive">
					Refresh failed {formatRelative(account.last_refresh_failed_at)}:{" "}
					{account.last_refresh_error}
				</p>
			)}
			{account.models_last_error && (
				<p className="border-t px-4 py-2 text-xs break-words text-destructive">
					Model refresh failed: {account.models_last_error}
				</p>
			)}
			{failover && !account.is_primary && (
				<div className="flex items-center justify-between gap-3 border-t px-4 py-2">
					<Field orientation="horizontal" className="w-fit">
						<Checkbox
							id={`failover-${account.id}`}
							checked={account.failover_enabled}
							disabled={toggleFailover.isPending}
							onCheckedChange={(checked) => toggleFailover.mutate(checked)}
						/>
						<FieldLabel
							htmlFor={`failover-${account.id}`}
							className="font-normal"
						>
							Use for failover
						</FieldLabel>
					</Field>
					<div className="flex gap-1">
						<Button
							size="icon-xs"
							variant="ghost"
							aria-label="Move up"
							disabled={
								index === 0 || accounts[index - 1]?.is_primary || move.isPending
							}
							onClick={() => move.mutate(-1)}
						>
							<ArrowUpIcon />
						</Button>
						<Button
							size="icon-xs"
							variant="ghost"
							aria-label="Move down"
							disabled={index === accounts.length - 1 || move.isPending}
							onClick={() => move.mutate(1)}
						>
							<ArrowDownIcon />
						</Button>
					</div>
				</div>
			)}
		</li>
	);
}

function limited(account: Account): boolean {
	return (account.limited_until ?? 0) > Date.now() / 1000;
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

// ---------------------------------------------------------------------------------------------

function LinkDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
	return (
		<Dialog open={open} onOpenChange={(next) => !next && onClose()}>
			<DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
				{open && <LinkFlow onClose={onClose} />}
			</DialogContent>
		</Dialog>
	);
}

function LinkFlow({ onClose }: { onClose: () => void }) {
	const queryClient = useQueryClient();
	const onLinked = () => {
		void queryClient.invalidateQueries({ queryKey: accountsQuery.queryKey });
		toast.success("The account is linked.");
		onClose();
	};

	return (
		<>
			<DialogHeader>
				<DialogTitle>Link a ChatGPT account</DialogTitle>
				<DialogDescription>
					The gateway uses the subscription like the Codex CLI does. OpenAI does
					not officially support other apps, so use it at your own risk.
				</DialogDescription>
			</DialogHeader>
			<Tabs defaultValue="device">
				<TabsList className="w-full">
					<TabsTrigger value="device">Code on another device</TabsTrigger>
					<TabsTrigger value="browser">Sign in here</TabsTrigger>
				</TabsList>
				{/* Both panels stay mounted, so switching tabs keeps a started sign-in. */}
				<TabsContent value="device" keepMounted className="pt-3">
					<DeviceLink onLinked={onLinked} />
				</TabsContent>
				<TabsContent value="browser" keepMounted className="pt-3">
					<BrowserLink onLinked={onLinked} />
				</TabsContent>
			</Tabs>
			<DialogFooter>
				<Button variant="outline" onClick={onClose}>
					Close
				</Button>
			</DialogFooter>
		</>
	);
}

/** Polls a link flow until it ends. An expired flow (404) counts as failed. Closing the
 * dialog cancels the flow, so the server stops waiting for it. */
function useFlowStatus(
	flow: string | undefined,
	onLinked: () => void,
): Schemas["LinkStatus"] | undefined {
	useEffect(() => {
		if (!flow) return;
		return () => {
			void api.DELETE("/chatgpt/link/{flow}", { params: { path: { flow } } });
		};
	}, [flow]);
	const status = useQuery({
		queryKey: ["link-flow", flow],
		queryFn: () =>
			call(
				api.GET("/chatgpt/link/{flow}", {
					params: { path: { flow: flow ?? "" } },
				}),
			),
		enabled: flow !== undefined,
		refetchInterval: (query) =>
			query.state.status !== "error" && query.state.data?.status === "pending"
				? 3000
				: false,
		// A 404 means the flow expired. Other errors (for example a 502 from a proxy) are
		// temporary: the sign-in may still finish on the server.
		retry: (count, error) =>
			!(error instanceof ApiError && error.status === 404) && count < 10,
		retryDelay: 3000,
	});
	const current: Schemas["LinkStatus"] | undefined = status.error
		? { status: "failed", message: status.error.message }
		: status.data;
	const done = current?.status === "done";
	useEffect(() => {
		if (done) onLinked();
	}, [done, onLinked]);
	return current;
}

function FlowMessage({ status }: { status?: Schemas["LinkStatus"] }) {
	if (status?.status !== "failed") return null;
	return <FieldError>{status.message}</FieldError>;
}

function DeviceLink({ onLinked }: { onLinked: () => void }) {
	const start = useMutation({
		mutationFn: () => call(api.POST("/chatgpt/link/device")),
	});
	const link = start.data;
	const status = useFlowStatus(link?.flow, onLinked);

	if (!link) {
		return (
			<div className="space-y-3">
				<p className="text-sm text-muted-foreground">
					You get a code. Enter it on the OpenAI page, on any device where you
					are logged in to ChatGPT. Device login must be allowed in the ChatGPT
					security settings.
				</p>
				{start.error && <FieldError>{start.error.message}</FieldError>}
				<Button disabled={start.isPending} onClick={() => start.mutate()}>
					Get a code
				</Button>
			</div>
		);
	}

	return (
		<div className="space-y-4">
			<ol className="list-decimal space-y-3 pl-5 text-sm">
				<li>
					Open{" "}
					<a
						href={link.verification_url}
						target="_blank"
						rel="noreferrer"
						className="inline-flex items-center gap-1 text-primary underline-offset-4 hover:underline"
					>
						{link.verification_url.replace("https://", "")}
						<ExternalLinkIcon className="size-3.5" />
					</a>
				</li>
				<li>
					Enter this code:
					<div className="mt-2 flex items-center gap-2">
						<code className="rounded-lg bg-muted px-4 py-2 font-mono text-xl tracking-[0.2em]">
							{link.user_code}
						</code>
						<Button
							size="icon-sm"
							variant="ghost"
							aria-label="Copy code"
							onClick={() => {
								void navigator.clipboard.writeText(link.user_code);
								toast.success("The code is copied.");
							}}
						>
							<CopyIcon />
						</Button>
					</div>
				</li>
			</ol>
			{status?.status === "failed" ? (
				<div className="space-y-3">
					<FlowMessage status={status} />
					<Button
						variant="outline"
						disabled={start.isPending}
						onClick={() => start.mutate()}
					>
						Start again
					</Button>
				</div>
			) : (
				<p className="flex items-center gap-2 text-sm text-muted-foreground">
					<RefreshCwIcon className="size-3.5 animate-spin" />
					Waiting for the confirmation. The code is valid for 15 minutes.
				</p>
			)}
		</div>
	);
}

function BrowserLink({ onLinked }: { onLinked: () => void }) {
	const queryClient = useQueryClient();
	const [pasted, setPasted] = useState("");
	const start = useMutation({
		mutationFn: () => call(api.POST("/chatgpt/link/pkce")),
	});
	const status = useFlowStatus(start.data?.flow, onLinked);
	const complete = useMutation({
		mutationFn: (flow: string) =>
			call(
				api.POST("/chatgpt/link/{flow}/callback", {
					params: { path: { flow } },
					body: { url: pasted },
				}),
			),
		onSuccess: (result, flow) =>
			queryClient.setQueryData(["link-flow", flow], result),
	});
	const flow = start.data?.flow;
	const failed = complete.data?.status === "failed" ? complete.data : status;

	return (
		<FieldGroup>
			<p className="text-sm text-muted-foreground">
				Log in to ChatGPT in a new tab. At the end your browser opens an address
				that starts with{" "}
				<code className="font-mono text-xs">http://127.0.0.1:1455</code> and
				shows an error. That is expected: copy that whole address and paste it
				here.
			</p>
			{start.data && failed?.status !== "failed" ? (
				<Button
					variant="outline"
					className="w-fit"
					render={
						<a
							href={start.data.authorize_url}
							target="_blank"
							rel="noreferrer"
						/>
					}
				>
					<ExternalLinkIcon />
					Open the sign-in page
				</Button>
			) : (
				<Button
					variant="outline"
					className="w-fit"
					disabled={start.isPending}
					onClick={() => {
						complete.reset();
						setPasted("");
						start.mutate();
					}}
				>
					{start.data ? "Start again" : "Start the sign-in"}
				</Button>
			)}
			{start.error && <FieldError>{start.error.message}</FieldError>}
			{flow && (
				<form
					className="space-y-3"
					onSubmit={(e) => {
						e.preventDefault();
						complete.mutate(flow);
					}}
				>
					<Field>
						<FieldLabel htmlFor="callback">Address from the browser</FieldLabel>
						<Input
							id="callback"
							value={pasted}
							onChange={(e) => setPasted(e.target.value)}
							placeholder="http://127.0.0.1:1455/auth/callback?code=…"
							className="font-mono text-xs"
						/>
						<FieldDescription>
							It contains the one-time sign-in code.
						</FieldDescription>
					</Field>
					<FlowMessage status={failed} />
					{complete.error && <FieldError>{complete.error.message}</FieldError>}
					<Button type="submit" disabled={!pasted.trim() || complete.isPending}>
						Link account
					</Button>
				</form>
			)}
		</FieldGroup>
	);
}

// ---------------------------------------------------------------------------------------------

const PLAN_ITEMS = [
	{ value: "inherit", label: "Use the global plan" },
	{ value: "custom", label: "Own plan" },
	{ value: "disabled", label: "Off (refresh only when needed)" },
];

function PlanDialog({
	account,
	onClose,
}: {
	account: Account | null;
	onClose: () => void;
}) {
	return (
		<Dialog open={account !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent>
				{account && (
					<PlanForm key={account.id} account={account} onClose={onClose} />
				)}
			</DialogContent>
		</Dialog>
	);
}

function PlanForm({
	account,
	onClose,
}: {
	account: Account;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const [label, setLabel] = useState(account.label ?? "");
	const [mode, setMode] = useState(account.refresh_mode);
	const [cron, setCron] = useState(account.refresh_cron ?? "0 */12 * * *");
	const save = useMutation({
		mutationFn: () =>
			call(
				api.PATCH("/chatgpt/accounts/{id}", {
					params: { path: { id: account.id } },
					body: {
						label: label.trim() || null,
						refresh_mode: mode,
						refresh_cron: mode === "custom" ? cron : null,
						failover_enabled: account.failover_enabled,
					},
				}),
			),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: accountsQuery.queryKey });
			toast.success("The account is saved.");
			onClose();
		},
	});

	return (
		<form
			onSubmit={(e) => {
				e.preventDefault();
				save.mutate();
			}}
		>
			<DialogHeader className="mb-4">
				<DialogTitle>{accountName(account)}</DialogTitle>
				{account.label && (
					<DialogDescription>{account.email}</DialogDescription>
				)}
			</DialogHeader>
			<FieldGroup>
				<Field>
					<FieldLabel htmlFor="label">Name</FieldLabel>
					<Input
						id="label"
						value={label}
						placeholder={account.email ?? ""}
						onChange={(e) => setLabel(e.target.value)}
					/>
					<FieldDescription>
						Optional. Shown instead of the email address.
					</FieldDescription>
				</Field>
				<Field>
					<FieldLabel htmlFor="mode">Token refresh</FieldLabel>
					<Select
						items={PLAN_ITEMS}
						value={mode}
						onValueChange={(v) => v && setMode(v)}
					>
						<SelectTrigger id="mode" className="w-full">
							<SelectValue />
						</SelectTrigger>
						<SelectContent>
							{PLAN_ITEMS.map((item) => (
								<SelectItem key={item.value} value={item.value}>
									{item.label}
								</SelectItem>
							))}
						</SelectContent>
					</Select>
				</Field>
				{mode === "custom" && (
					<CronField
						id="account-cron"
						label="Plan (cron)"
						value={cron}
						onChange={setCron}
					/>
				)}
				{save.error && <FieldError>{save.error.message}</FieldError>}
				<DialogFooter>
					<Button type="button" variant="outline" onClick={onClose}>
						Cancel
					</Button>
					<Button type="submit" disabled={save.isPending}>
						Save changes
					</Button>
				</DialogFooter>
			</FieldGroup>
		</form>
	);
}

function RemoveDialog({
	account,
	onClose,
}: {
	account: Account | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/chatgpt/accounts/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: accountsQuery.queryKey });
			toast.success("The account is removed.");
			onClose();
		},
		onError: (error) => {
			toast.error(errorMessage(error));
			onClose();
		},
	});

	return (
		<AlertDialog
			open={account !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>
						Remove {account && accountName(account)}?
					</AlertDialogTitle>
					<AlertDialogDescription>
						The gateway deletes the tokens of this account. Requests do not use
						it any more. The usage history stays.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={() => account && remove.mutate(account.id)}
					>
						Remove account
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
