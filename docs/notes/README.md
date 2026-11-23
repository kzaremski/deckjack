# Notes

Durable knowledge that is not obvious from the code: invariants and the reason
for each, gotchas with their exact error strings, dead ends already ruled out,
and decisions the code does not show. DESIGN.md says what Deckjack is meant to
be; APP-SUITE.md and the lane briefs say what the platform does today and what
each lane owes. These notes sit underneath both.

The source is the ground truth. If a note disagrees with the code, the note is
wrong: fix it or delete the line.

| Note | Covers |
| --- | --- |
| [security.md](security.md) | Risk vs grants, grant evaluation, agents and MCP, sessions and cookies, the audit log, sign-in code, passkeys, lockdown, the HTTP surface, terminals |
| [sandbox.md](sandbox.md) | Landlock and seccomp behavior behind the app sandbox, accepted gaps, app-host invariants |
| [testing.md](testing.md) | The test gate, mutation checking, the defect classes this repo keeps producing, route-test and harness traps |
| [operations.md](operations.md) | Install vs working directory, throwaway nodes, the sign-in code, restarts and shutdown, rebuilds, debugging a blank page |
| [web-shell.md](web-shell.md) | Connection liveness, offline honesty, confirmation, window layout, keyboard, tokens, the iOS install |
| [history.md](history.md) | JackLink, pocketdock and agentmancer, the operator's decisions, how parallel work fails here |

Unfinished work lives in [../OPEN-THREADS.md](../OPEN-THREADS.md).
