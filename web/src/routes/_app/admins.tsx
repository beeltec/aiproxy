import { useForm } from "@tanstack/react-form";
import {
	queryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { MoreHorizontalIcon, PlusIcon } from "lucide-react";
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
import { FieldError, FieldGroup } from "#/components/ui/field";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "#/components/ui/table";
import { api, call, errorMessage, type Schemas } from "#/lib/api/client";
import { formatDateTime, formatRelative } from "#/lib/format";
import { meQuery } from "#/lib/session";
import { passwordSchema, usernameSchema } from "#/lib/validation";

type Admin = Schemas["AdminView"];

const adminsQuery = queryOptions({
	queryKey: ["admins"],
	queryFn: () => call(api.GET("/admins")),
});

export const Route = createFileRoute("/_app/admins")({
	loader: ({ context }) =>
		context.queryClient.query({ ...adminsQuery, staleTime: "static" }),
	component: AdminsPage,
});

function AdminsPage() {
	const { data: admins = [] } = useQuery(adminsQuery);
	const { data: me } = useQuery(meQuery);
	const [adding, setAdding] = useState(false);
	const [resetting, setResetting] = useState<Admin | null>(null);
	const [deleting, setDeleting] = useState<Admin | null>(null);

	return (
		<div className="space-y-6">
			<PageHeader
				title="Admins"
				description="People who can log in to this dashboard. All admins have the same rights."
				actions={
					<Button onClick={() => setAdding(true)}>
						<PlusIcon />
						Add admin
					</Button>
				}
			/>
			<div className="overflow-hidden rounded-xl border bg-card">
				<Table>
					<TableHeader>
						<TableRow>
							<TableHead className="pl-4">Username</TableHead>
							<TableHead>Status</TableHead>
							<TableHead>Last login</TableHead>
							<TableHead className="hidden sm:table-cell">Created</TableHead>
							<TableHead className="w-12">
								<span className="sr-only">Actions</span>
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{admins.map((admin) => (
							<TableRow key={admin.id}>
								<TableCell className="pl-4 font-medium">
									{admin.username}
									{admin.id === me?.id && (
										<span className="eyebrow ml-2">You</span>
									)}
								</TableCell>
								<TableCell>
									{admin.disabled ? (
										<Badge variant="outline">Disabled</Badge>
									) : (
										<Badge variant="secondary">Enabled</Badge>
									)}
								</TableCell>
								<TableCell className="text-muted-foreground">
									{admin.last_login_at
										? formatRelative(admin.last_login_at)
										: "Never"}
								</TableCell>
								<TableCell className="hidden text-muted-foreground sm:table-cell">
									{formatDateTime(admin.created_at)}
								</TableCell>
								<TableCell>
									<AdminActions
										admin={admin}
										onResetPassword={() => setResetting(admin)}
										onDelete={() => setDeleting(admin)}
									/>
								</TableCell>
							</TableRow>
						))}
					</TableBody>
				</Table>
			</div>
			<AddAdminDialog open={adding} onOpenChange={setAdding} />
			<ResetPasswordDialog
				admin={resetting}
				onClose={() => setResetting(null)}
			/>
			<DeleteAdminDialog admin={deleting} onClose={() => setDeleting(null)} />
		</div>
	);
}

function AdminActions({
	admin,
	onResetPassword,
	onDelete,
}: {
	admin: Admin;
	onResetPassword: () => void;
	onDelete: () => void;
}) {
	const queryClient = useQueryClient();
	const toggle = useMutation({
		mutationFn: () =>
			call(
				api.PATCH("/admins/{id}", {
					params: { path: { id: admin.id } },
					body: { disabled: !admin.disabled },
				}),
			),
		onSuccess: (updated) => {
			void queryClient.invalidateQueries({ queryKey: adminsQuery.queryKey });
			toast.success(
				updated.disabled
					? `${updated.username} is disabled.`
					: `${updated.username} is enabled.`,
			);
		},
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<DropdownMenu>
			<DropdownMenuTrigger
				render={
					<Button
						variant="ghost"
						size="icon-sm"
						aria-label={`Actions for ${admin.username}`}
					/>
				}
			>
				<MoreHorizontalIcon />
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end" className="w-48">
				<DropdownMenuItem onClick={onResetPassword}>
					Set new password
				</DropdownMenuItem>
				<DropdownMenuItem onClick={() => toggle.mutate()}>
					{admin.disabled ? "Enable" : "Disable"}
				</DropdownMenuItem>
				<DropdownMenuSeparator />
				<DropdownMenuItem variant="destructive" onClick={onDelete}>
					Delete
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

const addSchema = z.object({
	username: usernameSchema,
	password: passwordSchema,
});

function AddAdminDialog({
	open,
	onOpenChange,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
}) {
	const queryClient = useQueryClient();
	const create = useMutation({
		mutationFn: (body: z.infer<typeof addSchema>) =>
			call(api.POST("/admins", { body })),
		onSuccess: (admin) => {
			void queryClient.invalidateQueries({ queryKey: adminsQuery.queryKey });
			toast.success(`${admin.username} is added.`);
			close();
		},
	});
	const form = useForm({
		defaultValues: { username: "", password: "" },
		validators: { onSubmit: addSchema },
		onSubmit: ({ value }) => create.mutateAsync(value).catch(() => undefined),
	});
	const close = () => {
		onOpenChange(false);
		form.reset();
		create.reset();
	};

	return (
		<Dialog
			open={open}
			onOpenChange={(next) => (next ? onOpenChange(true) : close())}
		>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>Add admin</DialogTitle>
					<DialogDescription>
						Give the new admin their username and password in a safe way.
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
						<form.Field name="username">
							{(field) => (
								<TextField field={field} label="Username" autoComplete="off" />
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
						{create.error && <FieldError>{create.error.message}</FieldError>}
						<DialogFooter>
							<Button type="button" variant="outline" onClick={close}>
								Cancel
							</Button>
							<form.Subscribe selector={(s) => s.isSubmitting}>
								{(submitting) => (
									<Button type="submit" disabled={submitting}>
										Add admin
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

const resetSchema = z.object({ password: passwordSchema });

function ResetPasswordDialog({
	admin,
	onClose,
}: {
	admin: Admin | null;
	onClose: () => void;
}) {
	const reset = useMutation({
		mutationFn: (password: string) =>
			call(
				api.POST("/admins/{id}/password", {
					params: { path: { id: admin?.id ?? 0 } },
					body: { password },
				}),
			),
		onSuccess: () => {
			toast.success(`The password of ${admin?.username} is changed.`);
			close();
		},
	});
	const form = useForm({
		defaultValues: { password: "" },
		validators: { onSubmit: resetSchema },
		onSubmit: ({ value }) =>
			reset.mutateAsync(value.password).catch(() => undefined),
	});
	const close = () => {
		onClose();
		form.reset();
		reset.reset();
	};

	return (
		<Dialog open={admin !== null} onOpenChange={(next) => !next && close()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>Set new password for {admin?.username}</DialogTitle>
					<DialogDescription>
						This logs {admin?.username} out on all devices.
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
						<form.Field name="password">
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
						{reset.error && <FieldError>{reset.error.message}</FieldError>}
						<DialogFooter>
							<Button type="button" variant="outline" onClick={close}>
								Cancel
							</Button>
							<form.Subscribe selector={(s) => s.isSubmitting}>
								{(submitting) => (
									<Button type="submit" disabled={submitting}>
										Set password
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

function DeleteAdminDialog({
	admin,
	onClose,
}: {
	admin: Admin | null;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const remove = useMutation({
		mutationFn: (id: number) =>
			call(api.DELETE("/admins/{id}", { params: { path: { id } } })),
		onSuccess: () => {
			void queryClient.invalidateQueries({ queryKey: adminsQuery.queryKey });
			toast.success(`${admin?.username} is deleted.`);
			onClose();
		},
		onError: (error) => {
			toast.error(errorMessage(error));
			onClose();
		},
	});

	return (
		<AlertDialog
			open={admin !== null}
			onOpenChange={(next) => !next && onClose()}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>Delete {admin?.username}?</AlertDialogTitle>
					<AlertDialogDescription>
						{admin?.username} cannot log in after this. You cannot undo it.
					</AlertDialogDescription>
				</AlertDialogHeader>
				<AlertDialogFooter>
					<AlertDialogCancel>Cancel</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={() => admin && remove.mutate(admin.id)}
					>
						Delete admin
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
