import { queryOptions } from "@tanstack/react-query";
import { ApiError, api, call } from "./api/client";

export const meQuery = queryOptions({
	queryKey: ["me"],
	queryFn: async () => {
		try {
			return await call(api.GET("/auth/me"));
		} catch (error) {
			if (error instanceof ApiError && error.status === 401) return null;
			throw error;
		}
	},
});

export const setupQuery = queryOptions({
	queryKey: ["setup"],
	queryFn: () => call(api.GET("/setup")),
});
