import type { AnyFieldApi } from "@tanstack/react-form";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldLabel,
} from "#/components/ui/field";
import { Input } from "#/components/ui/input";

/** Text input bound to a TanStack Form field. */
export function TextField({
	field,
	label,
	description,
	type = "text",
	autoComplete,
	autoFocus,
}: {
	field: AnyFieldApi;
	label: string;
	description?: string;
	type?: "text" | "password";
	autoComplete?: string;
	autoFocus?: boolean;
}) {
	const invalid = field.state.meta.isTouched && !field.state.meta.isValid;
	return (
		<Field data-invalid={invalid || undefined}>
			<FieldLabel htmlFor={field.name}>{label}</FieldLabel>
			<Input
				id={field.name}
				name={field.name}
				type={type}
				autoComplete={autoComplete}
				autoFocus={autoFocus}
				value={field.state.value}
				onBlur={field.handleBlur}
				onChange={(e) => field.handleChange(e.target.value)}
				aria-invalid={invalid || undefined}
			/>
			{invalid ? (
				<FieldError errors={field.state.meta.errors} />
			) : (
				description && <FieldDescription>{description}</FieldDescription>
			)}
		</Field>
	);
}
