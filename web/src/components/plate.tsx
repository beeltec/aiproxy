import { cn } from "#/lib/utils";

/** A panel styled like the rating plate on a device. */
export function Plate({
	className,
	...props
}: React.ComponentProps<"section">) {
	return <section className={cn("plate p-7 sm:p-8", className)} {...props} />;
}

/** One marking on the plate: a small label and its value. */
export function PlateRow({
	label,
	children,
}: {
	label: string;
	children: React.ReactNode;
}) {
	return (
		<div className="flex items-baseline justify-between gap-4 border-b border-dashed py-2 last:border-b-0">
			<span className="eyebrow">{label}</span>
			<span className="truncate font-mono text-xs">{children}</span>
		</div>
	);
}
