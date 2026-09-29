import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";
import { CronField } from "#/components/cron-field";
import { PageHeader } from "#/components/page-header";
import { Section } from "#/components/section";
import { Button } from "#/components/ui/button";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
} from "#/components/ui/field";
import { Input } from "#/components/ui/input";
import { Switch } from "#/components/ui/switch";
import { api, call, type Schemas } from "#/lib/api/client";
import { settingsQuery } from "#/lib/settings";

export const Route = createFileRoute("/_app/settings")({
	loader: ({ context }) =>
		context.queryClient.query({ ...settingsQuery, staleTime: "static" }),
	component: SettingsPage,
});

const TIME_ZONES = Intl.supportedValuesOf("timeZone");

function SettingsPage() {
	const { data } = useQuery(settingsQuery);
	if (!data) return null;
	// Remount when the stored settings change, so the form starts from them.
	return <SettingsForm key={JSON.stringify(data)} initial={data} />;
}

function SettingsForm({ initial }: { initial: Schemas["Settings"] }) {
	const queryClient = useQueryClient();
	const [value, setValue] = useState(initial);
	const save = useMutation({
		mutationFn: (body: Schemas["Settings"]) =>
			call(api.PUT("/settings", { body })),
		onSuccess: (saved) => {
			queryClient.setQueryData(settingsQuery.queryKey, saved);
			void queryClient.invalidateQueries({ queryKey: ["chatgpt-accounts"] });
			toast.success("The settings are saved.");
		},
	});
	const changed = JSON.stringify(value) !== JSON.stringify(initial);

	return (
		<form
			className="space-y-10"
			onSubmit={(e) => {
				e.preventDefault();
				save.mutate(value);
			}}
		>
			<PageHeader
				title="Settings"
				description="Settings for this gateway. They apply to all admins."
				actions={
					<Button type="submit" disabled={!changed || save.isPending}>
						Save changes
					</Button>
				}
			/>
			{save.error && <FieldError>{save.error.message}</FieldError>}
			<Section
				title="Time zone"
				description="Refresh and price sync plans run in this time zone."
			>
				<Field>
					<FieldLabel htmlFor="time-zone">Time zone</FieldLabel>
					<Input
						id="time-zone"
						list="time-zones"
						value={value.time_zone}
						onChange={(e) => setValue({ ...value, time_zone: e.target.value })}
						className="max-w-xs"
					/>
					<datalist id="time-zones">
						{TIME_ZONES.map((tz) => (
							<option key={tz} value={tz} />
						))}
					</datalist>
					<FieldDescription>
						A name such as Europe/Berlin. Your browser uses{" "}
						{Intl.DateTimeFormat().resolvedOptions().timeZone}.
					</FieldDescription>
				</Field>
			</Section>
			<Section
				title="Token refresh"
				description="The gateway renews the tokens of the ChatGPT accounts on this plan. An account can use its own plan."
			>
				<FieldGroup>
					<SwitchField
						id="refresh-enabled"
						label="Refresh on a plan"
						checked={value.refresh.enabled}
						onChange={(enabled) =>
							setValue({ ...value, refresh: { ...value.refresh, enabled } })
						}
					/>
					<div className="max-w-md">
						<CronField
							id="refresh-cron"
							label="Plan (cron)"
							value={value.refresh.cron}
							timeZone={value.time_zone}
							disabled={!value.refresh.enabled}
							onChange={(cron) =>
								setValue({ ...value, refresh: { ...value.refresh, cron } })
							}
						/>
					</div>
					<p className="max-w-prose text-sm text-muted-foreground">
						Tokens are also renewed when a request needs them, even with the
						plan off.
					</p>
				</FieldGroup>
			</Section>
			<Section
				title="Price sync"
				description="The gateway loads the public price lists of LiteLLM, models.dev and OpenRouter on this plan. Costs of new requests use the new prices."
			>
				<FieldGroup>
					<SwitchField
						id="price-sync-enabled"
						label="Load prices on a plan"
						checked={value.price_sync.enabled}
						onChange={(enabled) =>
							setValue({
								...value,
								price_sync: { ...value.price_sync, enabled },
							})
						}
					/>
					<div className="max-w-md">
						<CronField
							id="price-sync-cron"
							label="Plan (cron)"
							value={value.price_sync.cron}
							timeZone={value.time_zone}
							disabled={!value.price_sync.enabled}
							onChange={(cron) =>
								setValue({
									...value,
									price_sync: { ...value.price_sync, cron },
								})
							}
						/>
					</div>
					<p className="max-w-prose text-sm text-muted-foreground">
						You can also load the lists now on the Pricing page.
					</p>
				</FieldGroup>
			</Section>
			<Section
				title="Failover"
				description="When the primary ChatGPT account reaches its usage limit, requests go to the next checked account."
			>
				<FieldGroup>
					<SwitchField
						id="failover-enabled"
						label="Use failover"
						checked={value.failover.enabled}
						onChange={(enabled) =>
							setValue({ ...value, failover: { ...value.failover, enabled } })
						}
					/>
					<Field className="max-w-48">
						<FieldLabel htmlFor="threshold">Switch at usage (%)</FieldLabel>
						<Input
							id="threshold"
							type="number"
							min={1}
							max={100}
							disabled={!value.failover.enabled}
							value={value.failover.threshold_percent}
							onChange={(e) =>
								setValue({
									...value,
									failover: {
										...value.failover,
										threshold_percent: Number(e.target.value),
									},
								})
							}
						/>
					</Field>
					<p className="max-w-prose text-sm text-muted-foreground">
						Choose the accounts and their order on the Subscriptions page.
						OpenAI does not officially support this use of a subscription; more
						accounts in use can raise the risk of a block.
					</p>
				</FieldGroup>
			</Section>
		</form>
	);
}

function SwitchField({
	id,
	label,
	checked,
	onChange,
}: {
	id: string;
	label: string;
	checked: boolean;
	onChange: (checked: boolean) => void;
}) {
	return (
		<Field orientation="horizontal" className="w-fit">
			<Switch id={id} checked={checked} onCheckedChange={onChange} />
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
		</Field>
	);
}
