import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
	ChevronsUpDownIcon,
	GaugeIcon,
	LogOutIcon,
	ShieldCheckIcon,
	UsersIcon,
} from "lucide-react";
import { toast } from "sonner";
import { Wordmark } from "#/components/brand";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "#/components/ui/dropdown-menu";
import { api, call, errorMessage } from "#/lib/api/client";
import { meQuery } from "#/lib/session";

type NavItem = {
	to: "/" | "/admins" | "/security";
	label: string;
	icon: React.ComponentType<{ className?: string }>;
};

const NAV: { group: string; items: NavItem[] }[] = [
	{
		group: "Monitor",
		items: [{ to: "/", label: "Overview", icon: GaugeIcon }],
	},
	{
		group: "Instance",
		items: [{ to: "/admins", label: "Admins", icon: UsersIcon }],
	},
];

export function AppShell({
	username,
	children,
}: {
	username: string;
	children: React.ReactNode;
}) {
	return (
		<div className="min-h-dvh md:grid md:grid-cols-[15rem_1fr]">
			<aside className="flex flex-col border-b bg-card md:sticky md:top-0 md:h-dvh md:border-r md:border-b-0">
				<div className="flex h-14 items-center justify-between px-5">
					<Link to="/" className="rounded-sm">
						<Wordmark />
					</Link>
					<div className="md:hidden">
						<AccountMenu username={username} compact />
					</div>
				</div>
				<nav
					aria-label="Main"
					className="flex gap-1 overflow-x-auto px-3 pb-3 md:flex-1 md:flex-col md:gap-5 md:pt-4 md:pb-0"
				>
					{NAV.map(({ group, items }) => (
						<div key={group} className="flex gap-1 md:flex-col">
							<span className="eyebrow hidden px-2 pb-1 md:block">{group}</span>
							{items.map(({ to, label, icon: Icon }) => (
								<Link
									key={to}
									to={to}
									activeOptions={{ exact: to === "/" }}
									className="flex shrink-0 items-center gap-2.5 rounded-md px-2 py-1.5 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground data-[status=active]:bg-muted data-[status=active]:font-medium data-[status=active]:text-foreground"
								>
									<Icon className="size-4" />
									{label}
								</Link>
							))}
						</div>
					))}
				</nav>
				<div className="hidden border-t p-3 md:block">
					<AccountMenu username={username} />
				</div>
			</aside>
			<main className="mx-auto w-full max-w-6xl px-4 py-8 sm:px-8">
				{children}
			</main>
		</div>
	);
}

function AccountMenu({
	username,
	compact = false,
}: {
	username: string;
	compact?: boolean;
}) {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const logout = useMutation({
		mutationFn: () => call(api.POST("/auth/logout")),
		onSuccess: async () => {
			queryClient.clear();
			queryClient.setQueryData(meQuery.queryKey, null);
			await navigate({ to: "/login" });
		},
		onError: (error) => toast.error(errorMessage(error)),
	});

	return (
		<DropdownMenu>
			<DropdownMenuTrigger
				aria-label={`Account menu for ${username}`}
				className="flex w-full items-center justify-between gap-2 rounded-md px-2 py-2 text-left text-sm hover:bg-muted"
			>
				<span className="min-w-0">
					{!compact && <span className="eyebrow block">Logged in as</span>}
					<span className="block truncate font-medium">{username}</span>
				</span>
				<ChevronsUpDownIcon className="size-4 text-muted-foreground" />
			</DropdownMenuTrigger>
			<DropdownMenuContent
				side={compact ? "bottom" : "top"}
				align="end"
				className="w-52"
			>
				<DropdownMenuItem render={<Link to="/security" />} className="gap-2">
					<ShieldCheckIcon className="size-4" />
					Security
				</DropdownMenuItem>
				<DropdownMenuItem className="gap-2" onClick={() => logout.mutate()}>
					<LogOutIcon className="size-4" />
					Log out
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}
