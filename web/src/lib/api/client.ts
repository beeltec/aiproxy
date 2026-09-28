import createClient from "openapi-fetch";
import type { components, paths } from "./schema";

export type Schemas = components["schemas"];

export const api = createClient<paths>({ baseUrl: "/admin/api" });

export class ApiError extends Error {
	constructor(
		readonly status: number,
		readonly code: string,
		message: string,
	) {
		super(message);
	}
}

type ApiResult<T> = { data?: T; error?: unknown; response: Response };

/** Returns the answer data, or throws an ApiError with the message from the server. */
export async function call<T>(request: Promise<ApiResult<T>>): Promise<T> {
	const { data, error, response } = await request;
	if (!response.ok) {
		const body = error as Partial<Schemas["ErrorBody"]> | undefined;
		throw new ApiError(
			response.status,
			body?.error?.code ?? "unknown",
			body?.error?.message ?? `The request failed (HTTP ${response.status}).`,
		);
	}
	return data as T;
}

export function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : "Something went wrong.";
}
