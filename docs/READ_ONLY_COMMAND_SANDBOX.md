# Read-only command sandbox

[English](READ_ONLY_COMMAND_SANDBOX.md) | [简体中文](READ_ONLY_COMMAND_SANDBOX.zh-CN.md)

## Status

**Not enabled.** A `read_only` task refuses `commands_run`. The agent never
advertises `sandbox_read_only_commands`, and the server does not consult it.

`src/command_sandbox.rs` holds a working Linux Landlock foundation. It is kept
because the shape is right and throwing it away would mean rebuilding it, but a
foundation is not a boundary, and this document is the reason it stays switched
off.

## What `read_only` is supposed to mean

A `read_only` task promises the human who started it that nothing consequential
happens: the project is not modified, and nothing else the machine can reach is
either. That is the promise `commands_run` has to keep, not merely "the checkout
did not change".

## What the Landlock foundation actually does

It applies a write-denying ruleset to the command's process and every
descendant, irrevocably, allowing writes only beneath an explicitly listed
scratch directory. Reads are deliberately ungoverned, so the policy never has to
enumerate readable paths — the part of a sandbox that rots as a project grows.

That is a real property, and it is one access class out of several.

## What it does not cover

These are gaps in the current foundation, not speculation. The first is pinned
by a test in `src/command_sandbox.rs` that asserts the read succeeds.

1. **Reads are unrestricted.** A command can read any file the agent's user can
   read, including everything outside the checkout: `~/.ssh`, `~/.aws`, other
   projects, other tenants' state directories. A write filter does not stop
   exfiltration; it stops modification.

2. **The environment is inherited.** The child receives the agent's environment
   variables unless they are explicitly cleared. Anything a deployment puts
   there — tokens, endpoints, cloud credentials — is readable by the command.

3. **The network is untouched.** Landlock filesystem rules say nothing about
   sockets. A `read_only` command can reach the internet, POST the files it just
   read, or call an internal API that changes state elsewhere. Nothing about
   that is visible in the workspace afterwards.

4. **Some metadata operations may not be governed.** The ruleset covers write
   access classes for the negotiated ABI. Operations such as `chmod`, `utimes`,
   or ownership changes are not uniformly represented across ABI versions, and
   a kernel that supports only an older ABI would apply less than intended.
   The implementation requires `FullyEnforced` and rejects `PartiallyEnforced`
   precisely so that "applied less than intended" fails rather than passes, but
   it means the covered set is a function of the kernel.

5. **Therefore the foundation cannot support unapproved arbitrary shell.**
   Points 1 through 3 each let a `read_only` command have effects the human was
   told would not happen. Skipping approval on that basis would be trading a
   real gate for a partial one.

## Conditions required before re-enabling

All of these, not a subset. Each exists because of a specific way the current
foundation falls short.

**Execution boundary**

- Reads outside the checkout are denied, not merely unenumerated.
- Sensitive environment variables are not inherited; the child starts from an
  explicit allow-list.
- The network is closed by default, or every network side effect requires the
  same approval a mutation would.
- Project file contents *and* metadata are both unmodifiable.
- The private scratch directory is created atomically with mode `0700`, is
  verified not to be a symlink, and is removed when the task ends.

**Request integrity**

- A sandbox request is bound to a specific `agent_instance_id`.
- The capability check and the enqueue happen inside the same registry critical
  section, so a capability cannot be observed and then acted on after it has
  changed.
- Replacing an agent does not transfer a pending sandbox request to the new
  instance.
- An older agent cannot downgrade to an ordinary shell by ignoring the new
  field — an unrecognized sandbox mode must refuse, which the agent already
  does.
- The agent independently verifies the sandbox mode rather than trusting the
  server's word for it.
- The capability is advertised only on a proven `FullyEnforced` ruleset, from a
  probe that actually applies it and confirms a write is refused.

**Process**

- An independent threat model covering the boundary above.
- End-to-end acceptance against that threat model, not a unit test of the
  ruleset.

## How the foundation fails closed today

- The probe applies the ruleset in a throwaway child, requires
  `FullyEnforced`, and confirms the kernel refuses a write it should refuse.
  Creating a ruleset file descriptor proves only that the syscall exists.
- `PartiallyEnforced` and `NotEnforced` both deny. The ruleset uses hard
  compatibility rather than best effort, so a kernel that cannot honour the
  policy says so instead of applying less.
- On a non-Linux host a sandbox request is an error before the spawn, never a
  command that runs unconfined.
- The agent refuses an unknown sandbox mode rather than falling back.
- `doctor` reports whether the foundation exists, and says in the same breath
  that it changes nothing today.
