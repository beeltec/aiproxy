import { queryOptions } from "@tanstack/react-query";
import { api, call } from "#/lib/api/client";

/** All models of the ChatGPT accounts and the connections. */
export const modelsQuery = queryOptions({
	queryKey: ["models"],
	queryFn: () => call(api.GET("/models")),
});
