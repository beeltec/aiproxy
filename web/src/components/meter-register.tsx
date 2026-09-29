import { useEffect, useState } from "react";
import { formatUsd } from "#/lib/stats";
import { cn } from "#/lib/utils";

const DIGITS = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/**
 * An amount in USD as the counter of a utility meter: dark wheels for dollars, amber wheels
 * for cents and tenths of a cent. The wheels roll to the reading once it is known.
 */
export function MeterRegister({
	nano,
	label,
}: {
	nano: number;
	label: string;
}) {
	// The wheels start at zero and roll to the reading after the first paint.
	const [shown, setShown] = useState(0);
	useEffect(() => {
		const frame = requestAnimationFrame(() => setShown(nano));
		return () => cancelAnimationFrame(frame);
	}, [nano]);

	const tenths = Math.round(Math.max(0, shown) / 1e6);
	const dollars = String(Math.floor(tenths / 1000)).padStart(4, "0");
	const cents = String(tenths % 1000).padStart(3, "0");
	const wheels = [...dollars, ...cents];

	return (
		<div
			role="img"
			aria-label={`${label}: ${formatUsd(nano)}`}
			className="inline-flex items-stretch gap-[0.12em] rounded-[0.35em] bg-meter-well p-[0.14em] font-mono text-[clamp(1.4rem,8vw,2.25rem)] leading-none shadow-[inset_0_2px_6px_rgb(0_0_0/0.6)] sm:text-5xl"
		>
			<span className="flex w-[0.7em] items-center justify-center text-[0.55em] text-white/45">
				$
			</span>
			{wheels.map((digit, index) => (
				<Wheel
					// The wheels have fixed places, like on a meter.
					// biome-ignore lint/suspicious/noArrayIndexKey: the place is the identity
					key={index}
					digit={Number(digit)}
					cents={index >= dollars.length}
					// The right wheels turn first, like an odometer.
					delay={(wheels.length - index) * 70}
					point={index === dollars.length}
				/>
			))}
		</div>
	);
}

function Wheel({
	digit,
	cents,
	delay,
	point,
}: {
	digit: number;
	cents: boolean;
	delay: number;
	point: boolean;
}) {
	return (
		<>
			{point && (
				<span
					aria-hidden="true"
					className="w-[0.18em] self-end pb-[0.12em] text-center text-[0.6em] text-white/60"
				>
					.
				</span>
			)}
			<span
				aria-hidden="true"
				className={cn(
					"relative h-[1.35em] w-[0.86em] overflow-hidden rounded-[0.12em]",
					cents ? "bg-meter text-meter-well" : "bg-[#262b36] text-[#eef0f4]",
				)}
			>
				<span
					className="absolute inset-x-0 top-0 flex flex-col transition-transform duration-[1400ms] ease-[cubic-bezier(0.2,0.8,0.2,1)] motion-reduce:transition-none"
					style={{
						transform: `translateY(-${digit * 10}%)`,
						transitionDelay: `${delay}ms`,
					}}
				>
					{DIGITS.map((d) => (
						<span
							key={d}
							className="flex h-[1.35em] items-center justify-center tabular-nums"
						>
							{d}
						</span>
					))}
				</span>
				{/* The shade of a turning drum: darker at the top and the bottom. */}
				<span className="pointer-events-none absolute inset-0 bg-[linear-gradient(to_bottom,rgb(0_0_0/0.5),transparent_32%,transparent_68%,rgb(0_0_0/0.5))]" />
			</span>
		</>
	);
}
