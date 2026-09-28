import { createFileRoute } from "@tanstack/react-router";
import { GaugeMark } from "#/components/brand";
import { PageHeader } from "#/components/page-header";

export const Route = createFileRoute("/_app/")({ component: OverviewPage });

function OverviewPage() {
	return (
		<div className="space-y-8">
			<PageHeader
				title="Overview"
				description="Costs, token usage and account health of this gateway."
			/>
			<div className="flex flex-col items-center gap-3 rounded-xl border border-dashed px-6 py-16 text-center">
				<GaugeMark className="size-10 text-muted-foreground" />
				<p className="font-medium">No usage recorded yet</p>
				<p className="max-w-sm text-sm text-muted-foreground">
					Link a ChatGPT subscription or add a provider, then send a request
					through the gateway. Usage and costs show up here.
				</p>
			</div>
		</div>
	);
}
