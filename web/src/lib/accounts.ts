import { queryOptions } from "@tanstack/react-query";
import { api, call } from "#/lib/api/client";

export const accountsQuery = queryOptions({
	queryKey: ["chatgpt-accounts"],
	queryFn: () => call(api.GET("/chatgpt/accounts")),
	// Scheduled refreshes and retries change the accounts in the background.
	refetchInterval: 30_000,
});
