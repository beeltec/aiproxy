import { queryOptions } from "@tanstack/react-query";
import { api, call } from "./api/client";

export const settingsQuery = queryOptions({
	queryKey: ["settings"],
	queryFn: () => call(api.GET("/settings")),
});
