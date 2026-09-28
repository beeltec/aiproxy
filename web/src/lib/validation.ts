import { z } from "zod";

// Same rules as the server (server/src/admin/admins.rs).
export const usernameSchema = z
	.string()
	.regex(
		/^[A-Za-z0-9._-]{1,64}$/,
		"Use 1 to 64 characters: letters, digits, dot, underscore or hyphen.",
	);

// The server counts Unicode characters, not UTF-16 units.
const length = (value: string) => [...value].length;

export const passwordSchema = z
	.string()
	.refine((v) => length(v) >= 12, "Use at least 12 characters.")
	.refine((v) => length(v) <= 1024, "Use at most 1024 characters.");
