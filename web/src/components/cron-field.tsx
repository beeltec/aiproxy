import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldLabel,
} from "#/components/ui/field";
import { Input } from "#/components/ui/input";
import { api, call } from "#/lib/api/client";
import { formatRun } from "#/lib/format";
import { settingsQuery } from "#/lib/settings";

/** Cron input with the next 3 runs. The server computes them, like the scheduler does. */
export function CronField({
	id,
	label,
	value,
	onChange,
	timeZone,
	disabled,
}: {
	id: string;
	label: string;
	value: string;
	onChange: (value: string) => void;
	/** Default: the instance time zone. */
	timeZone?: string;
	disabled?: boolean;
}) {
	const cron = useDebounced(value.trim(), 300);
	const { data: settings } = useQuery(settingsQuery);
	const zone = validZone(timeZone?.trim() || settings?.time_zone || "UTC");
	const preview = useQuery({
		queryKey: ["cron-preview", cron, zone],
		queryFn: () =>
			call(
				api.POST("/settings/cron-preview", {
					body: { cron, time_zone: zone },
				}),
			),
		enabled: cron !== "" && !disabled,
		retry: false,
	});

	return (
		<Field data-invalid={preview.isError || undefined}>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			<Input
				id={id}
				value={value}
				disabled={disabled}
				onChange={(e) => onChange(e.target.value)}
				className="font-mono"
				spellCheck={false}
				aria-invalid={preview.isError || undefined}
			/>
			{preview.isError ? (
				<FieldError>{preview.error.message}</FieldError>
			) : (
				<FieldDescription>
					{preview.data && !disabled ? (
						<>
							Next runs ({zone}):{" "}
							{preview.data.next_runs
								.map((at) => formatRun(at, zone))
								.join(" · ")}
						</>
					) : (
						"Minute, hour, day of month, month, day of week. For example 0 3 * * * is every day at 03:00."
					)}
				</FieldDescription>
			)}
		</Field>
	);
}

function useDebounced<T>(value: T, delay: number): T {
	const [debounced, setDebounced] = useState(value);
	useEffect(() => {
		const timer = setTimeout(() => setDebounced(value), delay);
		return () => clearTimeout(timer);
	}, [value, delay]);
	return debounced;
}

/** The browser cannot format dates for unknown zones; the server reports the error on save. */
function validZone(zone: string): string {
	try {
		new Intl.DateTimeFormat("en-GB", { timeZone: zone });
		return zone;
	} catch {
		return "UTC";
	}
}
