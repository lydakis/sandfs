# Using sandfs with Blue

This document shows how a Blue runtime can mount `/blue`, enforce view-specific policies, attach inbox/log recorders, and wrap each turn in snapshots.

## Installation

```bash
pip install git+https://github.com/lydakis/sandfs.git
# once published: pip install sandfs
```

## Bootstrap a sandbox per chat

```python
from sandfs import MemoryStorageAdapter, NodePolicy, SandboxShell, VirtualFileSystem, VisibilityView
from sandfs.integrations import InboxRecorder

# 1. create the sandbox
vfs = VirtualFileSystem()

# 2. mount identity + workspaces with policies
vfs.mount_storage("/blue/identity", MemoryStorageAdapter(initial=identity_blob))
vfs.set_policy(
    "/blue/identity/persona.md",
    NodePolicy(writable=False, classification="private", principals={chat.owner_id}),
)

# 3. attach inbox/log recorders
inbox = InboxRecorder()
inbox.attach(vfs, "/blue/inbox")

# 4. configure the shell for the chat participants
view = VisibilityView(
    classifications={"public", "partner"},
    principals=set(chat.participant_ids),
)
shell = SandboxShell(
    vfs,
    view=view,
    allowed_commands={"ls", "cat", "rg", "python"},
    max_output_bytes=8_000,
)
```

## Per-turn workflow

```python
snapshot = vfs.snapshot()
try:
    result = shell.exec(agent_command)
    process_inbox(inbox.events)  # fan into Blue staging queues
finally:
    vfs.restore(snapshot)
    inbox.events.clear()
```

This keeps `/blue` isolated per turn while still letting the runtime flush writes into its storage layer.
