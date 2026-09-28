import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { toast } from "sonner";
import { z } from "zod";
import { PageHeader } from "#/components/page-header";
import { factorsQuery, SecondFactors } from "#/components/second-factors";
import { Section } from "#/components/section";
import { TextField } from "#/components/text-field";
import { Badge } from "#/components/ui/badge";
import { Button } from "#/components/ui/button";
import { FieldError, FieldGroup } from "#/components/ui/field";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "#/components/ui/table";
import { api, call, errorMessage } from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";
import { passwordSchema } from "#/lib/validation";

const sessionsQuery = queryOptions({
	queryKey: ["sessions"],
	queryFn: () => call(api.GET("/account/sessions")),
});

export const Route = createFileRoute("/_app/security")({
	loader: ({ context }) =>
		Promise.all([
			context.queryClient.query({ ...sessionsQuery, staleTime: "static" }),
			context.queryClient.query({ ...factorsQuery, staleTime: "static" }),
		]),
	component: SecurityPage,
});

function SecurityPage() {
	return (
		<div className="space-y-10">
			<PageHeader
				title="Security"
				description="Your password, the second login step, and the devices where you are logged in."
			/>
			<Section
				title="Password"
				description="Changing it logs you out on all other devices."
			>
				<ChangePasswordForm />
			</Section>
			<Section
				title="Two-step login"
				description="After the password, the login asks for one more proof. Passkeys also work without a password."
			>
				<SecondFactors />
			</Section>
			<Section
				title="Sessions"
				description="End a session to log out that device."
			>
				<SessionsTable />
			</Section>
		</div>
	);
}

const passwordFormSchema = z
	.object({
		current: z.string().min(1, "Enter your current password."),
		next: passwordSchema,
		repeat: z.string(),
	})
	.refine((v) => v.next === v.repeat, {
		message: "The passwords are not the same.",
		path: ["repeat"],
	});

function ChangePasswordForm() {
	const queryClient = useQueryClient();
	const change = useMutation({
		mutationFn: (v: z.infer<typeof passwordFormSchema>) =>
			call(
				api.POST("/account/password", {
					body: { current_password: v.current, new_password: v.next },
				}),
			),
		onSuccess: () => {
			toast.success("Your password is changed.");
			form.reset();
			void queryClient.invalidateQueries({ queryKey: sessionsQuery.queryKey });
		},
	});
	const form = useForm({
		defaultValues: { current: "", next: "", repeat: "" },
		validators: { onSubmit: passwordFormSchema },
		onSubmit: ({ value }) => change.mutateAsync(value).catch(() => undefined),
	});

	return (
		<form
			noValidate
			className="max-w-sm rounded-xl border bg-card p-5"
			onSubmit={(e) => {
				e.preventDefault();
				void form.handleSubmit();
			}}
		>
			<FieldGroup>
				<form.Field name="current">
					{(field) => (
						<TextField
							field={field}
							label="Current password"
							type="password"
							autoComplete="current-password"
						/>
					)}
				</form.Field>
				<form.Field name="next">
					{(field) => (
						<TextField
							field={field}
							label="New password"
							type="password"
							autoComplete="new-password"
							description="At least 12 characters."
						/>
					)}
				</form.Field>
				<form.Field name="repeat">
					{(field) => (
						<TextField
							field={field}
							label="Repeat new password"
							type="password"
							autoComplete="new-password"
						/>
					)}
				</form.Field>
				{change.error && <FieldError>{change.error.message}</FieldError>}
				<form.Subscribe selector={(s) => s.isSubmitting}>
					{(submitting) => (
						<Button type="submit" disabled={submitting} className="self-start">
							Change password
						</Button>
					)}
				</form.Subscribe>
			</FieldGroup>
		</form>
	);
}

function SessionsTable() {
	const queryClient = useQueryClient();
	const { data: sessions = [] } = useQuery(sessionsQuery);
	const end = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/account/sessions/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			toast.success("The session is ended.");
			void queryClient.invalidateQueries({ queryKey: sessionsQuery.queryKey });
		},
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<div className="overflow-hidden rounded-xl border bg-card">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead className="pl-4">Device</TableHead>
						<TableHead>Last active</TableHead>
						<TableHead className="hidden sm:table-cell">Started</TableHead>
						<TableHead className="w-24">
							<span className="sr-only">Actions</span>
						</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{sessions.map((s) => (
						<TableRow key={s.id}>
							<TableCell className="max-w-64 pl-4">
								<span
									className="block truncate"
									title={s.user_agent ?? undefined}
								>
									{describeDevice(s.user_agent)}
								</span>
								<span className="font-mono text-[0.7rem] text-muted-foreground">
									{s.ip ?? "Unknown IP"}
								</span>
							</TableCell>
							<TableCell className="text-muted-foreground">
								{s.current ? (
									<Badge variant="secondary">This device</Badge>
								) : (
									formatRelative(s.last_seen_at)
								)}
							</TableCell>
							<TableCell className="hidden text-muted-foreground sm:table-cell">
								{formatDateTime(s.created_at)}
							</TableCell>
							<TableCell className="pr-4 text-right">
								{!s.current && (
									<Button
										variant="outline"
										size="sm"
										disabled={end.isPending}
										onClick={() => end.mutate(s.id)}
									>
										End
									</Button>
								)}
							</TableCell>
						</TableRow>
					))}
				</TableBody>
			</Table>
		</div>
	);
}

/** Short device name from the user agent, for example "Firefox on macOS". */
function describeDevice(userAgent: string | null | undefined): string {
	if (!userAgent) return "Unknown device";
	const browser =
		[
			["Edg/", "Edge"],
			["Firefox/", "Firefox"],
			["Chrome/", "Chrome"],
			["Safari/", "Safari"],
			["curl/", "curl"],
		].find(([marker]) => userAgent.includes(marker))?.[1] ?? "Browser";
	const os = [
		["iPhone", "iOS"],
		["iPad", "iPadOS"],
		["Android", "Android"],
		["Windows", "Windows"],
		["Mac OS X", "macOS"],
		["Linux", "Linux"],
	].find(([marker]) => userAgent.includes(marker))?.[1];
	return os ? `${browser} on ${os}` : browser;
}
