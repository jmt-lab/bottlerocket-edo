# CLI: `twoliter run`

Run the DAG for an arbitrary edo addr. This is the escape hatch: any addr,
any kind, no assumptions about what it is.

## Usage

```
twoliter run <addr> [--arch <arch>] [--no-lock] [--jobs <N>] [-- <passthrough…>]
```

## Behavior

Thin wrapper around edo's `Scheduler::run` — literally what `edo run <addr>`
does. Included so users don't have to install `edo-cli` separately.

`src/cmd/run.rs` already contains the wiring.

## Relationship to `twoliter build`

`twoliter build variant aws-k8s-1.29` == `twoliter run //variants/aws-k8s-1.29`.
The former does argument validation and addr composition; the latter is raw.

Use `build` when you want type-checked args and clear error messages when a
requested variant doesn't exist. Use `run` when you're driving the tool from
a script or when you're targeting a node with a non-standard kind.

## Old-code references

None — no direct equivalent in old twoliter. This is an edo-native
capability we surface.
