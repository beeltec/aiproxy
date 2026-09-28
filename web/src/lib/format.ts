// The UI is English: English words, 24-hour clock, day before month.
const LOCALE = "en-GB";

const dateTime = new Intl.DateTimeFormat(LOCALE, {
	dateStyle: "medium",
	timeStyle: "short",
});
const relative = new Intl.RelativeTimeFormat(LOCALE, { numeric: "auto" });

/** Formats unix seconds as a local date and time. */
export function formatDateTime(seconds: number): string {
	return dateTime.format(seconds * 1000);
}

/** Formats unix seconds relative to now, for example "5 minutes ago". */
export function formatRelative(seconds: number): string {
	const diff = seconds - Date.now() / 1000;
	const abs = Math.abs(diff);
	if (abs < 60) return relative.format(Math.round(diff), "second");
	if (abs < 3600) return relative.format(Math.round(diff / 60), "minute");
	if (abs < 86400) return relative.format(Math.round(diff / 3600), "hour");
	return relative.format(Math.round(diff / 86400), "day");
}

/** Formats unix seconds with weekday, in the given time zone. */
export function formatRun(seconds: number, timeZone: string): string {
	return new Intl.DateTimeFormat(LOCALE, {
		weekday: "short",
		day: "numeric",
		month: "short",
		hour: "2-digit",
		minute: "2-digit",
		timeZone,
	}).format(seconds * 1000);
}
