/** Small gauge with a needle: the product mark. */
export function GaugeMark({ className }: { className?: string }) {
	return (
		<svg viewBox="0 0 24 24" aria-hidden="true" className={className}>
			<path
				d="M3 16a9 9 0 0 1 18 0"
				fill="none"
				stroke="currentColor"
				strokeWidth="2"
				strokeLinecap="round"
			/>
			<path
				d="M12 16 17 8.5"
				className="stroke-meter"
				strokeWidth="2"
				strokeLinecap="round"
			/>
			<circle cx="12" cy="16" r="1.75" fill="currentColor" />
		</svg>
	);
}

export function Wordmark() {
	return (
		<span className="inline-flex items-center gap-2 font-mono text-sm font-semibold tracking-tight">
			<GaugeMark className="size-5" />
			aiproxy
		</span>
	);
}
