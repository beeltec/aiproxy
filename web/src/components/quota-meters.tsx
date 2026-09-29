import type { Schemas } from "#/lib/api/client";
import { formatRelative } from "#/lib/format";

type Account = Schemas["AccountView"];

function windowName(minutes: number | null | undefined): string {
	if (!minutes) return "Limit";
	if (minutes === 10080) return "Week";
	if (minutes % 1440 === 0) return `${minutes / 1440} days`;
	if (minutes % 60 === 0) return `${minutes / 60} hours`;
	return `${minutes} min`;
}

/** The usage limits of the account, as meters. The tick shows the failover threshold. */
export function QuotaMeters({
	account,
	threshold,
}: {
	account: Account;
	threshold: number | null;
}) {
	const windows = [
		{
			used: account.primary_used_percent,
			minutes: account.primary_window_minutes,
			reset: account.primary_reset_at,
		},
		{
			used: account.secondary_used_percent,
			minutes: account.secondary_window_minutes,
			reset: account.secondary_reset_at,
		},
	].filter(
		(w) => w.used !== null && w.used !== undefined && (w.minutes ?? 0) > 0,
	);
	if (windows.length === 0) {
		return (
			<p className="border-t px-4 py-2 text-xs text-muted-foreground">
				Usage limits show up after the first request.
			</p>
		);
	}
	return (
		<div className="grid grid-cols-[repeat(auto-fit,minmax(14rem,1fr))] gap-3 border-t px-4 py-3">
			{windows.map((w) => {
				const used = Math.min(100, Math.max(0, w.used ?? 0));
				return (
					<div key={`${w.minutes}`} className="space-y-1.5">
						<div className="flex items-baseline justify-between gap-2">
							<span className="eyebrow">{windowName(w.minutes)}</span>
							<span className="font-mono text-xs tabular-nums">
								{used.toFixed(0)}%
								{w.reset && w.reset > Date.now() / 1000 && (
									<span className="text-muted-foreground">
										{" "}
										· resets {formatRelative(w.reset)}
									</span>
								)}
							</span>
						</div>
						<div
							className="relative h-1.5 rounded-full bg-muted"
							// The percentage is in the text above; the bar only shows it.
							aria-hidden="true"
						>
							<div
								className="h-full rounded-full bg-meter"
								style={{ width: `${used}%` }}
							/>
							{threshold !== null && (
								<div
									className="absolute -top-1 h-3.5 w-px bg-foreground/50"
									style={{ left: `${threshold}%` }}
									title={`Failover at ${threshold}%`}
								/>
							)}
						</div>
					</div>
				);
			})}
		</div>
	);
}
