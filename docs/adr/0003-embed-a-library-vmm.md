# Embed a library VMM in the supervisor

The primary architecture is one Supervisor embedding a library VMM, minidox, and fork coordination for multiple VMs. Keeping virtiofs queues, DAX mappings, branch metadata, and the Fork Point in one process avoids a mandatory vhost-user data plane and makes the sub-10-ms pause path direct. This accepts a larger Supervisor failure domain; persistent Filesystem Branches remain recoverable, and an external transport can be added later as an adapter.
