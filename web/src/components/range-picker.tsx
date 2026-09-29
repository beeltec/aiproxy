import { Segmented } from "#/components/segmented";
import { Input } from "#/components/ui/input";
import { localDate, RANGES, type Range, rangeBounds } from "#/lib/stats";

/** The time range of a page: a preset, or two local dates (both included). */
export function RangePicker({
	value,
	onChange,
}: {
	value: Range;
	onChange: (value: Range) => void;
}) {
	const pick = (range: Range["range"]) => {
		if (range !== "custom") return onChange({ range });
		// Start with the dates of the preset that was shown.
		const [from, to] = rangeBounds(value);
		onChange({
			range,
			from: localDate(new Date(from * 1000)),
			to: localDate(new Date((to - 1) * 1000)),
		});
	};
	return (
		<div className="flex flex-wrap items-center justify-end gap-2">
			<Segmented
				label="Time range"
				options={RANGES}
				value={value.range}
				onChange={pick}
			/>
			{value.range === "custom" && (
				<div className="flex items-center gap-1.5 text-sm">
					<Input
						type="date"
						aria-label="First day"
						className="h-8 w-40"
						value={value.from ?? ""}
						max={value.to}
						onChange={(e) =>
							e.target.value && onChange({ ...value, from: e.target.value })
						}
					/>
					<span className="text-muted-foreground">to</span>
					<Input
						type="date"
						aria-label="Last day"
						className="h-8 w-40"
						value={value.to ?? ""}
						min={value.from}
						onChange={(e) =>
							e.target.value && onChange({ ...value, to: e.target.value })
						}
					/>
				</div>
			)}
		</div>
	);
}
