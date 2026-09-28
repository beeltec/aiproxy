import { useForm } from "@tanstack/react-form";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { KeyRoundIcon } from "lucide-react";
import { useState } from "react";
import { z } from "zod";
import { OrDivider } from "#/components/or-divider";
import { Plate, PlateRow } from "#/components/plate";
import { PublicLayout } from "#/components/public-layout";
import { TextField } from "#/components/text-field";
import { Button } from "#/components/ui/button";
import { FieldError, FieldGroup } from "#/components/ui/field";
import { api, call, type Schemas } from "#/lib/api/client";
import { getPasskey } from "#/lib/passkey";
import { meQuery, setupQuery } from "#/lib/session";

export const Route = createFileRoute("/login")({
	beforeLoad: async ({ context }) => {
		if (typeof window === "undefined") return;
		const setup = await context.queryClient.query(setupQuery);
		if (setup.required) throw redirect({ to: "/setup" });
		if (await context.queryClient.query({ ...meQuery, staleTime: "static" })) {
			throw redirect({ to: "/" });
		}
	},
	component: LoginPage,
});

type Methods = Schemas["Methods"];

function LoginPage() {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const [secondFactor, setSecondFactor] = useState<Methods | null>(null);

	const done = async (me: Schemas["Me"]) => {
		// Drop data of an earlier session, which may belong to another admin.
		queryClient.clear();
		queryClient.setQueryData(meQuery.queryKey, me);
		await navigate({ to: "/" });
	};

	return (
		<PublicLayout>
			<Plate aria-labelledby="login-title">
				{secondFactor ? (
					<SecondFactorStep
						methods={secondFactor}
						onDone={done}
						onBack={() => setSecondFactor(null)}
					/>
				) : (
					<PasswordStep onDone={done} onSecondFactor={setSecondFactor} />
				)}
				<div className="mt-7">
					<PlateRow label="Instance">{window.location.host}</PlateRow>
				</div>
			</Plate>
		</PublicLayout>
	);
}

const passwordSchema = z.object({
	username: z.string().min(1, "Enter your username."),
	password: z.string().min(1, "Enter your password."),
});

function PasswordStep({
	onDone,
	onSecondFactor,
}: {
	onDone: (me: Schemas["Me"]) => Promise<void>;
	onSecondFactor: (methods: Methods) => void;
}) {
	const login = useMutation({
		mutationFn: (body: z.infer<typeof passwordSchema>) =>
			call(api.POST("/auth/login", { body })),
		onSuccess: async (result) => {
			if (result.me) await onDone(result.me);
			else if (result.second_factor) onSecondFactor(result.second_factor);
		},
	});
	const passkey = useMutation({
		mutationFn: async () => {
			const { ceremony, options } = await call(
				api.POST("/auth/passkey/options"),
			);
			const credential = await getPasskey(options);
			return call(
				api.POST("/auth/passkey", { body: { ceremony, credential } }),
			);
		},
		onSuccess: onDone,
	});
	const form = useForm({
		defaultValues: { username: "", password: "" },
		validators: { onSubmit: passwordSchema },
		onSubmit: ({ value }) => login.mutateAsync(value).catch(() => undefined),
	});
	const error = login.error ?? passkey.error;

	return (
		<>
			<div className="mb-6 space-y-1">
				<p className="eyebrow">Admin console</p>
				<h1 id="login-title" className="text-xl font-semibold tracking-tight">
					Log in
				</h1>
			</div>
			<form
				noValidate
				onSubmit={(e) => {
					e.preventDefault();
					void form.handleSubmit();
				}}
			>
				<FieldGroup>
					<form.Field name="username">
						{(field) => (
							<TextField
								field={field}
								label="Username"
								autoComplete="username webauthn"
								autoFocus
							/>
						)}
					</form.Field>
					<form.Field name="password">
						{(field) => (
							<TextField
								field={field}
								label="Password"
								type="password"
								autoComplete="current-password"
							/>
						)}
					</form.Field>
					{error && <FieldError>{error.message}</FieldError>}
					<form.Subscribe selector={(s) => s.isSubmitting}>
						{(submitting) => (
							<Button type="submit" size="lg" disabled={submitting}>
								{submitting ? "Logging in…" : "Log in"}
							</Button>
						)}
					</form.Subscribe>
					<OrDivider />
					<Button
						type="button"
						variant="outline"
						size="lg"
						disabled={passkey.isPending}
						onClick={() => passkey.mutate()}
					>
						<KeyRoundIcon />
						Log in with a passkey
					</Button>
				</FieldGroup>
			</form>
		</>
	);
}

type Mode = "totp" | "passkey" | "recovery";

const HINTS: Record<Mode, string> = {
	totp: "Enter the 6-digit code from your authenticator app.",
	passkey: "Use one of your passkeys.",
	recovery: "Enter one of your recovery codes. Each code works once.",
};

const codeSchema = z.object({
	code: z.string().trim().min(1, "Enter the code."),
});

function SecondFactorStep({
	methods,
	onDone,
	onBack,
}: {
	methods: Methods;
	onDone: (me: Schemas["Me"]) => Promise<void>;
	onBack: () => void;
}) {
	const [mode, setMode] = useState<Mode>(
		methods.totp ? "totp" : methods.passkey ? "passkey" : "recovery",
	);
	const verify = useMutation({
		mutationFn: (code: string) =>
			mode === "totp"
				? call(api.POST("/auth/second-factor/totp", { body: { code } }))
				: call(
						api.POST("/auth/second-factor/recovery-code", { body: { code } }),
					),
		onSuccess: onDone,
	});
	const passkey = useMutation({
		mutationFn: async () => {
			const { ceremony, options } = await call(
				api.POST("/auth/second-factor/passkey/options"),
			);
			const credential = await getPasskey(options);
			return call(
				api.POST("/auth/second-factor/passkey", {
					body: { ceremony, credential },
				}),
			);
		},
		onSuccess: onDone,
	});
	const form = useForm({
		defaultValues: { code: "" },
		validators: { onSubmit: codeSchema },
		onSubmit: ({ value }) =>
			verify.mutateAsync(value.code.trim()).catch(() => undefined),
	});
	const switchMode = (next: Mode) => {
		setMode(next);
		form.reset();
		verify.reset();
	};
	const error = verify.error ?? passkey.error;
	const showCodeForm = mode !== "passkey";

	return (
		<>
			<div className="mb-6 space-y-1">
				<p className="eyebrow">Step 2 of 2</p>
				<h1 id="login-title" className="text-xl font-semibold tracking-tight">
					Confirm it is you
				</h1>
				<p className="text-sm text-muted-foreground">{HINTS[mode]}</p>
			</div>
			<FieldGroup>
				{methods.passkey && (
					<Button
						type="button"
						size="lg"
						variant={mode === "passkey" ? "default" : "outline"}
						disabled={passkey.isPending}
						onClick={() => passkey.mutate()}
					>
						<KeyRoundIcon />
						Use a passkey
					</Button>
				)}
				{methods.passkey && showCodeForm && <OrDivider />}
				{showCodeForm && (
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
										key={mode}
										field={field}
										label={mode === "totp" ? "Code" : "Recovery code"}
										autoComplete="one-time-code"
										autoFocus
									/>
								)}
							</form.Field>
							<form.Subscribe selector={(s) => s.isSubmitting}>
								{(submitting) => (
									<Button type="submit" size="lg" disabled={submitting}>
										Confirm
									</Button>
								)}
							</form.Subscribe>
						</FieldGroup>
					</form>
				)}
				{error && <FieldError>{error.message}</FieldError>}
				<div className="flex flex-wrap justify-between gap-2 text-sm">
					<Button
						type="button"
						variant="link"
						className="px-0"
						onClick={onBack}
					>
						Back
					</Button>
					{methods.recovery_code && mode !== "recovery" && (
						<Button
							type="button"
							variant="link"
							className="px-0"
							onClick={() => switchMode("recovery")}
						>
							Use a recovery code
						</Button>
					)}
					{methods.totp && mode === "recovery" && (
						<Button
							type="button"
							variant="link"
							className="px-0"
							onClick={() => switchMode("totp")}
						>
							Use the authenticator app
						</Button>
					)}
				</div>
			</FieldGroup>
		</>
	);
}
