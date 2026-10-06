<script lang="ts" module>
	import { cn, type WithElementRef } from "$lib/utils.js";
	import type { HTMLAttributes } from "svelte/elements";
	import { type VariantProps, tv } from "tailwind-variants";

	// The pill the playlist header already drew by hand (On this device, Collab, From Spotify),
	// plus the chips the playlist tools add: a filter that is on, a track's duplicate count.
	// No hover transition on `chip`: it sits in track rows (docs/UI-PERFORMANCE.md).
	export const badgeVariants = tv({
		base: "inline-flex shrink-0 items-center gap-1 whitespace-nowrap rounded-full font-medium [&_svg]:pointer-events-none [&_svg]:size-3 [&_svg]:shrink-0",
		variants: {
			variant: {
				label: "bg-primary/15 px-2 py-0.5 text-[10px] uppercase tracking-wide text-primary",
				chip: "bg-primary/15 px-1.5 py-px text-[11px] tabular-nums text-primary",
				muted: "bg-muted px-2 py-0.5 text-xs text-muted-foreground",
				outline: "border border-border px-2 py-0.5 text-xs text-foreground",
			},
		},
		defaultVariants: {
			variant: "label",
		},
	});

	export type BadgeVariant = VariantProps<typeof badgeVariants>["variant"];

	export type BadgeProps = WithElementRef<HTMLAttributes<HTMLSpanElement>> & {
		variant?: BadgeVariant;
	};
</script>

<script lang="ts">
	let {
		class: className,
		variant = "label",
		ref = $bindable(null),
		children,
		...restProps
	}: BadgeProps = $props();
</script>

<span
	bind:this={ref}
	data-slot="badge"
	class={cn(badgeVariants({ variant }), className)}
	{...restProps}
>
	{@render children?.()}
</span>
