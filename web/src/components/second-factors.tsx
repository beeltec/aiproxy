import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import {
	CopyIcon,
	KeyRoundIcon,
	ListChecksIcon,
	SmartphoneIcon,
	TrashIcon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { TextField } from "#/components/text-field";
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
import { FieldError, FieldGroup } from "#/components/ui/field";
import { api, call, errorMessage, type Schemas } from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";
import { createPasskey } from "#/lib/passkey";

export const factorsQuery = queryOptions({
	queryKey: ["second-factors"],
	queryFn: () => call(api.GET("/account/second-factors")),
});

/** TOTP, passkeys and recovery codes of the logged-in admin. */
export function SecondFactors() {
	const { data } = useQuery(factorsQuery);
	const [codes, setCodes] = useState<string[] | null>(null);
	const queryClient = useQueryClient();
	const refresh = () =>
		queryClient.invalidateQueries({ queryKey: factorsQuery.queryKey });
	const onAdded = (added: Schemas["FactorAdded"]) => {
		void refresh();
		if (added.recovery_codes) setCodes(added.recovery_codes);
	};

	if (!data) return null;
	return (
		<div className="divide-y overflow-hidden rounded-xl border bg-card">
			<TotpRow active={data.totp} onAdded={onAdded} onRemoved={refresh} />
			<PasskeysRow
				passkeys={data.passkeys}
				onAdded={onAdded}
				onRemoved={refresh}
			/>
			<RecoveryRow
				left={data.recovery_codes_left}
				enabled={data.totp || data.passkeys.length > 0}
				onNewCodes={(next) => {
					void refresh();
					setCodes(next);
				}}
			/>
			<RecoveryCodesDialog codes={codes} onClose={() => setCodes(null)} />
		</div>
	);
}

function Row({
	icon: Icon,
	title,
	status,
	description,
	action,
	children,
}: {
	icon: React.ComponentType<{ className?: string }>;
	title: string;
	status?: React.ReactNode;
	description: string;
	action?: React.ReactNode;
	children?: React.ReactNode;
}) {
	return (
		<div className="space-y-3 p-5">
			<div className="flex flex-wrap items-start justify-between gap-3">
				<div className="flex gap-3">
					<Icon className="mt-0.5 size-5 text-muted-foreground" />
					<div className="space-y-0.5">
						<div className="flex items-center gap-2 font-medium">
							{title}
							{status}
						</div>
						<p className="text-sm text-muted-foreground">{description}</p>
					</div>
				</div>
				{action}
			</div>
			{children}
		</div>
	);
}

function On() {
	return <Badge variant="secondary">On</Badge>;
}

// ---------------------------------------------------------------------------------------------

function TotpRow({
	active,
	onAdded,
	onRemoved,
}: {
	active: boolean;
	onAdded: (added: Schemas["FactorAdded"]) => void;
	onRemoved: () => void;
}) {
	const [setup, setSetup] = useState<Schemas["TotpSetup"] | null>(null);
	const start = useMutation({
		mutationFn: () => call(api.POST("/account/totp")),
		onSuccess: setSetup,
		onError: (error) => toast.error(errorMessage(error)),
	});
	const remove = useMutation({
		mutationFn: () => call(api.DELETE("/account/totp")),
		onSuccess: () => {
			toast.success("The authenticator app is removed.");
			onRemoved();
		},
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<Row
			icon={SmartphoneIcon}
			title="Authenticator app"
			status={active && <On />}
			description="A 6-digit code from an app such as 1Password, Aegis or Google Authenticator."
			action={
				active ? (
					<Button
						variant="outline"
						size="sm"
						disabled={remove.isPending}
						onClick={() => remove.mutate()}
					>
						Remove
					</Button>
				) : (
					<Button
						size="sm"
						disabled={start.isPending}
						onClick={() => start.mutate()}
					>
						Set up
					</Button>
				)
			}
		>
			<TotpSetupDialog
				setup={setup}
				onClose={() => setSetup(null)}
				onConfirmed={(added) => {
					setSetup(null);
					toast.success("The authenticator app is on.");
					onAdded(added);
				}}
			/>
		</Row>
	);
}

const codeSchema = z.object({
	code: z
		.string()
		.trim()
		.regex(/^\d{6}$/, "Enter the 6 digits."),
});

function TotpSetupDialog({
	setup,
	onClose,
	onConfirmed,
}: {
	setup: Schemas["TotpSetup"] | null;
	onClose: () => void;
	onConfirmed: (added: Schemas["FactorAdded"]) => void;
}) {
	const confirm = useMutation({
		mutationFn: (code: string) =>
			call(api.POST("/account/totp/confirm", { body: { code } })),
		onSuccess: (added) => {
			form.reset();
			onConfirmed(added);
		},
	});
	const form = useForm({
		defaultValues: { code: "" },
		validators: { onSubmit: codeSchema },
		onSubmit: ({ value }) =>
			confirm.mutateAsync(value.code.trim()).catch(() => undefined),
	});
	const close = () => {
		form.reset();
		confirm.reset();
		onClose();
	};

	return (
		<Dialog open={setup !== null} onOpenChange={(next) => !next && close()}>
			<DialogContent className="sm:max-w-md">
				<DialogHeader>
					<DialogTitle>Set up the authenticator app</DialogTitle>
					<DialogDescription>
						Scan the code with your app, then enter the code the app shows.
					</DialogDescription>
				</DialogHeader>
				{setup && (
					<div className="grid gap-4 sm:grid-cols-[auto_1fr] sm:items-center">
						<img
							src={`data:image/png;base64,${setup.qr_png_base64}`}
							alt="QR code for the authenticator app"
							className="size-40 justify-self-center rounded-md bg-white p-2"
						/>
						<div className="min-w-0 space-y-1">
							<p className="eyebrow">Or enter this key</p>
							<p className="font-mono text-xs break-all">{setup.secret}</p>
						</div>
					</div>
				)}
				<form
					noValidate
					onSubmit={(e) => {
						e.preventDefault();
						void form.handleSubmit();
					}}
				>
					<FieldGroup>
						<form.Field name="code">
							{(field) => (
								<TextField
									field={field}
									label="Code from the app"
									autoComplete="one-time-code"
								/>
							)}
						</form.Field>
						{confirm.error && <FieldError>{confirm.error.message}</FieldError>}
						<DialogFooter>
							<Button type="button" variant="outline" onClick={close}>
								Cancel
							</Button>
							<form.Subscribe selector={(s) => s.isSubmitting}>
								{(submitting) => (
									<Button type="submit" disabled={submitting}>
										Turn on
									</Button>
								)}
							</form.Subscribe>
						</DialogFooter>
					</FieldGroup>
				</form>
			</DialogContent>
		</Dialog>
	);
}

// ---------------------------------------------------------------------------------------------

function PasskeysRow({
	passkeys,
	onAdded,
	onRemoved,
}: {
	passkeys: Schemas["PasskeyView"][];
	onAdded: (added: Schemas["FactorAdded"]) => void;
	onRemoved: () => void;
}) {
	const [adding, setAdding] = useState(false);
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/account/passkeys/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			toast.success("The passkey is removed.");
			onRemoved();
		},
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<Row
			icon={KeyRoundIcon}
			title="Passkeys"
			status={passkeys.length > 0 && <On />}
			description="Log in with your device, without a password. A passkey also works as the second step."
			action={
				<Button size="sm" variant="outline" onClick={() => setAdding(true)}>
					Add passkey
				</Button>
			}
		>
			{passkeys.length > 0 && (
				<ul className="divide-y rounded-lg border">
					{passkeys.map((p) => (
						<li
							key={p.id}
							className="flex items-center justify-between gap-3 px-3 py-2 text-sm"
						>
							<div className="min-w-0">
								<p className="truncate font-medium">{p.name}</p>
								<p className="text-xs text-muted-foreground">
									Added {formatDateTime(p.created_at)} ·{" "}
									{p.last_used_at
										? `used ${formatRelative(p.last_used_at)}`
										: "not used yet"}
								</p>
							</div>
							<Button
								variant="ghost"
								size="icon-sm"
								aria-label={`Remove passkey ${p.name}`}
								disabled={remove.isPending}
								onClick={() => remove.mutate(p.id)}
							>
								<TrashIcon />
							</Button>
						</li>
					))}
				</ul>
			)}
			<AddPasskeyDialog
				open={adding}
				onClose={() => setAdding(false)}
				onAdded={(added) => {
					setAdding(false);
					toast.success("The passkey is added.");
					onAdded(added);
				}}
			/>
		</Row>
	);
}

const nameSchema = z.object({
	name: z
		.string()
		.trim()
		.min(1, "Enter a name.")
		.max(64, "Use at most 64 characters."),
});

function AddPasskeyDialog({
	open,
	onClose,
	onAdded,
}: {
	open: boolean;
	onClose: () => void;
	onAdded: (added: Schemas["FactorAdded"]) => void;
}) {
	const add = useMutation({
		mutationFn: async (name: string) => {
			const { ceremony, options } = await call(
				api.POST("/account/passkeys/options"),
			);
			const credential = await createPasskey(options);
			return call(
				api.POST("/account/passkeys", {
					body: { ceremony, name, credential },
				}),
			);
		},
		onSuccess: (added) => {
			form.reset();
			onAdded(added);
		},
	});
	const form = useForm({
		defaultValues: { name: "" },
		validators: { onSubmit: nameSchema },
		onSubmit: ({ value }) =>
			add.mutateAsync(value.name.trim()).catch(() => undefined),
	});
	const close = () => {
		form.reset();
		add.reset();
		onClose();
	};

	return (
		<Dialog open={open} onOpenChange={(next) => !next && close()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>Add a passkey</DialogTitle>
					<DialogDescription>
						Give it a name you recognize, for example “MacBook” or “YubiKey”.
						Your browser asks you to confirm next.
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
								<TextField field={field} label="Name" autoComplete="off" />
							)}
						</form.Field>
						{add.error && <FieldError>{add.error.message}</FieldError>}
						<DialogFooter>
							<Button type="button" variant="outline" onClick={close}>
								Cancel
							</Button>
							<form.Subscribe selector={(s) => s.isSubmitting}>
								{(submitting) => (
									<Button type="submit" disabled={submitting}>
										Continue
									</Button>
								)}
							</form.Subscribe>
						</DialogFooter>
					</FieldGroup>
				</form>
			</DialogContent>
		</Dialog>
	);
}

// ---------------------------------------------------------------------------------------------

function RecoveryRow({
	left,
	enabled,
	onNewCodes,
}: {
	left: number;
	enabled: boolean;
	onNewCodes: (codes: string[]) => void;
}) {
	const renew = useMutation({
		mutationFn: () => call(api.POST("/account/recovery-codes")),
		onSuccess: (result) => onNewCodes(result.recovery_codes),
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<Row
			icon={ListChecksIcon}
			title="Recovery codes"
			status={
				enabled && (
					<Badge variant={left <= 2 ? "destructive" : "secondary"}>
						{left} left
					</Badge>
				)
			}
			description={
				enabled
					? "Use one when you lose access to your app and passkeys. Each code works once."
					: "You get recovery codes when you add the first app or passkey."
			}
			action={
				enabled && (
					<Button
						variant="outline"
						size="sm"
						disabled={renew.isPending}
						onClick={() => renew.mutate()}
					>
						New codes
					</Button>
				)
			}
		/>
	);
}

function RecoveryCodesDialog({
	codes,
	onClose,
}: {
	codes: string[] | null;
	onClose: () => void;
}) {
	const copy = async () => {
		await navigator.clipboard.writeText((codes ?? []).join("\n"));
		toast.success("The codes are copied.");
	};

	return (
		<Dialog open={codes !== null} onOpenChange={(next) => !next && onClose()}>
			<DialogContent showCloseButton={false}>
				<DialogHeader>
					<DialogTitle>Save your recovery codes</DialogTitle>
					<DialogDescription>
						Keep them in a safe place, for example your password manager. You
						see them only now. Older codes do not work any more.
					</DialogDescription>
				</DialogHeader>
				<ol className="grid grid-cols-2 gap-x-6 gap-y-1.5 rounded-lg bg-muted px-4 py-3 font-mono text-sm">
					{codes?.map((code) => (
						<li key={code}>{code}</li>
					))}
				</ol>
				<DialogFooter>
					<Button variant="outline" onClick={() => void copy()}>
						<CopyIcon />
						Copy
					</Button>
					<Button onClick={onClose}>I saved them</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
}
