import { useQuery } from "@tanstack/react-query";
import {
	createFileRoute,
	Navigate,
	Outlet,
	redirect,
} from "@tanstack/react-router";
import { AppShell } from "#/components/app-shell";
import { meQuery, setupQuery } from "#/lib/session";

export const Route = createFileRoute("/_app")({
	beforeLoad: async ({ context }) => {
		// The SPA shell is prerendered at build time, without a session.
		if (typeof window === "undefined") return;
		if (await context.queryClient.query({ ...meQuery, staleTime: "static" }))
			return;
		const setup = await context.queryClient.fetchQuery(setupQuery);
		throw redirect({ to: setup.required ? "/setup" : "/login" });
	},
	component: AppLayout,
});

function AppLayout() {
	const { data: me, isPending } = useQuery(meQuery);
	if (isPending) return null;
	// The session ended while the page was open.
	if (!me) return <Navigate to="/login" />;
	return (
		<AppShell username={me.username}>
			<Outlet />
		</AppShell>
	);
}
