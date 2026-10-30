# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
[Report a vulnerability](https://github.com/lithoscomputer/petri/security/advisories/new).
Do not open a public issue for a security problem.

Include the Petri commit, the workflow or steps that show the problem, and the
backend you used (host, Docker, or Daytona).

## Supported versions

Petri has no releases yet. Fixes land on `main`.

## Scope

Petri runs the commands and agents that a workflow describes. These are
expected behavior, not vulnerabilities:

- A workflow on the host backend reads or changes your files. The host plugin
  does not isolate commands.
- A GitHub Actions step uses the GitHub token you provide.

Report a problem when Petri breaks a boundary it claims to keep. Examples are
a secret that appears unmasked in a run log, a container or Daytona job that
reaches the host outside its sandbox, or a plugin that Petri launches without
the verification it requires.
