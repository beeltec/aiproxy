import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";
import { createRouter } from "@tanstack/react-router";
import { ApiError } from "./lib/api/client";
import { meQuery } from "./lib/session";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
	// An ended session sends the admin back to the login page. A failed login is a
	// normal form error and has a different code.
	const onError = (error: Error) => {
		if (error instanceof ApiError && error.code === "unauthorized") {
			queryClient.clear();
			queryClient.setQueryData(meQuery.queryKey, null);
			void router.navigate({ to: "/login" });
		}
	};
	const queryClient: QueryClient = new QueryClient({
		queryCache: new QueryCache({ onError }),
		mutationCache: new MutationCache({ onError }),
		defaultOptions: {
			queries: {
				retry: (count, error) =>
					!(error instanceof ApiError && error.status < 500) && count < 2,
			},
		},
	});

	const router = createRouter({
		routeTree,
		context: { queryClient },
		scrollRestoration: true,
		defaultPreload: "intent",
		defaultPreloadStaleTime: 0,
	});
	return router;
}

declare module "@tanstack/react-router" {
	interface Register {
		router: ReturnType<typeof getRouter>;
	}
}
