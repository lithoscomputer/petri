# Sandbox input and saved-record transition

Execution still uses the existing providers, CLI backend flags, fingerprints,
ownership labels and lease policy. This preparation does not enable the new
sandbox API or change allocation, attachment, inspection, fencing or pruning.

## Common workload lowering

| Input | Shared application representation | Compatibility execution |
| --- | --- | --- |
| Native `source: host_directory` or `runtime: host` | `RuntimeTarget::HostProcess` | Existing backend/runner choice |
| Native image source or legacy container runtime | `RuntimeTarget::Container` | Existing provider translation |
| Attractor resolved environment image | The same container target, except legacy `local` | Existing Attractor/Fabro translation |
| Fabro environment name or `run.environment.id` | Resolve once; retain name in graph parameters; pass resolved settings to Attractor | Existing catalog and CLI precedence |
| Native basic services; GitHub container/services | Scope env, typed container options and service declarations | Existing shared scope acquisition |

The future driver adapter consumes these resolved workload inputs once, using
`sandbox_driver::contract::SandboxSpec`. Applications supply workload source,
services and container data. Driver code owns helper images, network creation,
nested runners and provider recipes. No new provider recipe belongs here.

The current legacy declarations do not cover the entire new contract. Native
accepts only host/image sources and service image/env. Unknown keys are refused.
Legacy shell health checks, start periods, service-specific options, image
credentials and platform selection need explicit compatibility decisions before
conversion; they must not be serialized into the new DTO and silently dropped.
New readiness uses argv and finite attempt/total deadlines, not the old Docker
health-check defaults. Explicit false container options must remain explicit.

## Saved records

New coordinator reservations atomically save a `ScopeAdmission` alongside the
lease. It freezes the resolved runtime, workload env and service declarations,
including credential names rather than resolved credentials. On allocation it
also freezes the legacy provider kind and fingerprint. Reacquisition with
changed inputs fails before allocation. Writer updates and log replay refuse
rewritten or removed snapshots and changed bindings. Snapshot debug output
omits workload values. These are legacy application inputs, not a new driver
spec, connection configuration or namespace.

| Record | Reader and transition rule |
| --- | --- |
| Old record without admission | Continue the existing legacy reader and fingerprint/ownership checks. Never fill missing historical inputs or a namespace from current settings. |
| New legacy admission | Keep inputs and established binding immutable across restart, stop, delete and tombstone records. |
| Pending delete | Preserve the intent and resource identity until the existing lease manager confirms deletion. |
| Deleted record | Retain the tombstone for history; ordinary acquisition remains refused. |
| Unknown resource or nested runtime fields | Fail decoding; do not reinterpret a future saved ref as a legacy bare ID. |

Older Petri binaries may ignore additive admission data. Once this writer is in
use, resume with this version or newer to enforce its snapshot protections;
downgrade readers cannot provide those guarantees. Missing bindings, contact
failures and retained Host files are not evidence that a sandbox was deleted.
No new namespace is synthesized from a fingerprint, URL, provider kind or path.

## Remaining cutover dependencies

The pinned driver predates the merged shared contract. Keep that pin and all
legacy readers until a focused consumer integration uses the merged contract
and proven providers. The core catalog must supply the admitted portable spec
and immutable provider-entry connection/launch binding (with secret references)
before Petri can persist those values. The current legacy provider factory API
does not expose that binding; a kind/fingerprint snapshot is not a substitute.

Before any new-ref writer is enabled, add a versioned record reader using the
merged `SandboxRef { provider, namespace, id }`, dispatch legacy and new records
explicitly, and refuse new refs on the legacy execution path. Attach/destroy
must use the saved provider entry and compare the backend-issued namespace
before mutation, independently of whether the environment still exists.
Old records either stay on legacy readers or undergo an explicit verified
migration. Never infer namespace from today's catalog. Retain ownership checks,
delete intents, tombstones and application retention/replacement decisions.

Named environment selection in native/Attractor and full portable overrides
require the shared catalog and descriptor/schema validation. Do not activate
those inputs by falling back to current CLI backends. Connection schemas apply
to provider entries; workload schemas apply only to `provider_config`.

Runtime cutover also requires actual Docker, Daytona and separately installed
Host integration evidence. Host's new process durability/owner-process access
does not permit another worker or server to attach by path. Missing Host must
not disable other entries. Uncertain mutations must not be automatically
replayed. Provider recipes, old observers, fingerprints and setup stay until
that gate passes.
