# minidox

minidox provides copy-on-write filesystem state to forkable microVMs.

## Language

**Source VM**:
Any VM selected as the point from which a new VM is forked. Being a Source VM is temporary; it is not a special VM class.
_Avoid_: Template VM, golden VM

**Child VM**:
A VM created by forking a Source VM's memory and filesystem state copy-on-write.
_Avoid_: Clone, instance

**Fork Point**:
The single instant whose memory and filesystem state becomes the initial state of a Child VM.
_Avoid_: Snapshot time, clone time

**Fork Forest**:
A group of VMs connected by fork ancestry and eligible to share unchanged memory and filesystem pages.
_Avoid_: VM pool, clone group

**Supervisor**:
The host process that owns a Fork Forest and embeds its VMs, minidox, and fork coordination.
_Avoid_: VMM daemon, backend

**Filesystem Branch**:
The isolated filesystem state owned by one VM. A Child VM begins with a CoW branch derived from its Source VM at the Fork Point.
_Avoid_: Filesystem clone, VM volume

**Filesystem Generation**:
An immutable point in a Filesystem Branch's history that can be shared as the common ancestor of branches created by a fork.
_Avoid_: Snapshot, version

**Durable Fork**:
A fork whose Child VM and Filesystem Generation must survive host power loss once creation succeeds.
_Avoid_: Persistent clone, synced fork

**Branch Capability**:
The authority granted to one VM connection to access exactly one Filesystem Branch.
_Avoid_: Branch ID, filesystem token

**Shared Page**:
A page whose contents have not diverged since a fork and remain shared by the Source VM and its children.
_Avoid_: Template page, common page

**Diverged Page**:
A page privately owned by one VM after it writes to a Shared Page.
_Avoid_: Dirty page, changed page
