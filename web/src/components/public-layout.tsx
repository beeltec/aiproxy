import { Wordmark } from "#/components/brand";

/** Page frame for the login and setup pages. */
export function PublicLayout({ children }: { children: React.ReactNode }) {
	return (
		<main className="grid min-h-dvh place-items-center px-4 py-10">
			<div className="w-full max-w-sm space-y-6">
				<div className="flex justify-center">
					<Wordmark />
				</div>
				{children}
			</div>
		</main>
	);
}
