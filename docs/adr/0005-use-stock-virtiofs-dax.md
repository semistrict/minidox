# Use stock virtiofs DAX for guests

Linux guests use the stock virtiofs driver and standard DAX operations; copy-on-write behavior remains entirely host-side. Active mappings are pinned, clean unmapped pages are evictable, and dirty pages require writeback before eviction. The initial system is Linux-hosted, host-local, and does not require a custom guest kernel, cross-host coherence, live migration, or branch merging.
