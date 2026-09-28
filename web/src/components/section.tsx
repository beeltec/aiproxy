/** A titled block on a settings-like page: text on the left, content on the right. */
export function Section({
	title,
	description,
	children,
}: {
	title: string;
	description: string;
	children: React.ReactNode;
}) {
	return (
		<section className="grid gap-4 lg:grid-cols-[16rem_minmax(0,1fr)] lg:gap-10">
			<div className="space-y-1">
				<h2 className="font-semibold">{title}</h2>
				<p className="text-sm text-muted-foreground">{description}</p>
			</div>
			<div className="min-w-0">{children}</div>
		</section>
	);
}
