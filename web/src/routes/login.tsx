import { useForm } from "@tanstack/react-form";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { z } from "zod";
import { Plate, PlateRow } from "#/components/plate";
import { PublicLayout } from "#/components/public-layout";
import { TextField } from "#/components/text-field";
import { Button } from "#/components/ui/button";
import { FieldError, FieldGroup } from "#/components/ui/field";
import { api, call } from "#/lib/api/client";
import { meQuery, setupQuery } from "#/lib/session";

export const Route = createFileRoute("/login")({
	beforeLoad: async ({ context }) => {
		if (typeof window === "undefined") return;
		const setup = await context.queryClient.fetchQuery(setupQuery);
		if (setup.required) throw redirect({ to: "/setup" });
		if (await context.queryClient.query({ ...meQuery, staleTime: "static" })) {
			throw redirect({ to: "/" });
		}
	},
	component: LoginPage,
});

const schema = z.object({
	username: z.string().min(1, "Enter your username."),
	password: z.string().min(1, "Enter your password."),
});

function LoginPage() {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const login = useMutation({
		mutationFn: (body: z.infer<typeof schema>) =>
			call(api.POST("/auth/login", { body })),
		onSuccess: async (me) => {
			// Drop data of an earlier session, which may belong to another admin.
			queryClient.clear();
			queryClient.setQueryData(meQuery.queryKey, me);
			await navigate({ to: "/" });
		},
	});
	const form = useForm({
		defaultValues: { username: "", password: "" },
		validators: { onSubmit: schema },
		onSubmit: ({ value }) => login.mutateAsync(value).catch(() => undefined),
	});

	return (
		<PublicLayout>
			<Plate aria-labelledby="login-title">
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
									autoComplete="username"
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
						{login.error && <FieldError>{login.error.message}</FieldError>}
						<form.Subscribe selector={(s) => s.isSubmitting}>
							{(submitting) => (
								<Button type="submit" size="lg" disabled={submitting}>
									{submitting ? "Logging in…" : "Log in"}
								</Button>
							)}
						</form.Subscribe>
					</FieldGroup>
				</form>
				<div className="mt-7">
					<PlateRow label="Instance">{window.location.host}</PlateRow>
				</div>
			</Plate>
		</PublicLayout>
	);
}
