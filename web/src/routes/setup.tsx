import { useForm } from "@tanstack/react-form";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { z } from "zod";
import { Plate } from "#/components/plate";
import { PublicLayout } from "#/components/public-layout";
import { TextField } from "#/components/text-field";
import { Button } from "#/components/ui/button";
import { FieldError, FieldGroup } from "#/components/ui/field";
import { api, call } from "#/lib/api/client";
import { meQuery, setupQuery } from "#/lib/session";
import { passwordSchema, usernameSchema } from "#/lib/validation";

export const Route = createFileRoute("/setup")({
	beforeLoad: async ({ context }) => {
		if (typeof window === "undefined") return;
		const setup = await context.queryClient.fetchQuery(setupQuery);
		if (!setup.required) throw redirect({ to: "/login" });
	},
	component: SetupPage,
});

const schema = z.object({
	token: z.string().trim().min(1, "Enter the setup token."),
	username: usernameSchema,
	password: passwordSchema,
});

function SetupPage() {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const setup = useMutation({
		mutationFn: (body: z.infer<typeof schema>) =>
			call(api.POST("/setup", { body: { ...body, token: body.token.trim() } })),
		onSuccess: async (me) => {
			queryClient.setQueryData(meQuery.queryKey, me);
			queryClient.setQueryData(setupQuery.queryKey, { required: false });
			await navigate({ to: "/" });
		},
	});
	const form = useForm({
		defaultValues: { token: "", username: "", password: "" },
		validators: { onSubmit: schema },
		onSubmit: ({ value }) => setup.mutateAsync(value).catch(() => undefined),
	});

	return (
		<PublicLayout>
			<Plate aria-labelledby="setup-title">
				<div className="mb-6 space-y-2">
					<p className="eyebrow">First start</p>
					<h1 id="setup-title" className="text-xl font-semibold tracking-tight">
						Create the first admin
					</h1>
					<p className="text-sm text-muted-foreground">
						Get a setup token on the server. It is valid for one hour.
					</p>
					<pre className="overflow-x-auto rounded-md bg-muted px-3 py-2 font-mono text-[0.7rem]">
						docker exec aiproxy aiproxy setup-token
					</pre>
				</div>
				<form
					noValidate
					onSubmit={(e) => {
						e.preventDefault();
						void form.handleSubmit();
					}}
				>
					<FieldGroup>
						<form.Field name="token">
							{(field) => (
								<TextField
									field={field}
									label="Setup token"
									autoComplete="off"
									autoFocus
								/>
							)}
						</form.Field>
						<form.Field name="username">
							{(field) => (
								<TextField
									field={field}
									label="Username"
									autoComplete="username"
								/>
							)}
						</form.Field>
						<form.Field name="password">
							{(field) => (
								<TextField
									field={field}
									label="Password"
									type="password"
									autoComplete="new-password"
									description="At least 12 characters."
								/>
							)}
						</form.Field>
						{setup.error && <FieldError>{setup.error.message}</FieldError>}
						<form.Subscribe selector={(s) => s.isSubmitting}>
							{(submitting) => (
								<Button type="submit" size="lg" disabled={submitting}>
									{submitting ? "Creating admin…" : "Create admin"}
								</Button>
							)}
						</form.Subscribe>
					</FieldGroup>
				</form>
			</Plate>
		</PublicLayout>
	);
}
