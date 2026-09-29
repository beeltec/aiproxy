import { cn } from "#/lib/utils";

/** A row of buttons for one choice, such as a time range. */
export function Segmented<T extends string>({
	label,
	options,
	value,
	onChange,
}: {
	label: string;
	options: Record<T, string>;
	value: T;
	onChange: (value: T) => void;
}) {
	return (
		// biome-ignore lint/a11y/useSemanticElements: a group of toggle buttons, not a form fieldset
		<div
			role="group"
			aria-label={label}
			className="inline-flex flex-wrap rounded-lg bg-muted p-0.5"
		>
			{(Object.entries(options) as [T, string][]).map(([key, text]) => (
				<button
					key={key}
					type="button"
					aria-pressed={value === key}
					onClick={() => onChange(key)}
					className={cn(
						"rounded-md px-2.5 py-1 text-sm text-muted-foreground transition-colors hover:text-foreground",
						"focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
						value === key && "bg-card font-medium text-foreground shadow-sm",
					)}
				>
					{text}
				</button>
			))}
		</div>
	);
}
